//! One ordered writer for this app's small files on disk (preferences,
//! drafts, voice and SnapShot settings).
//!
//! Every save goes through a single background thread fed by a channel, so
//! saves land in the order they were made; several saves of one file that
//! queue up while a write runs collapse into the newest (latest wins), and a
//! removal is ordered with the writes of the same file. Each write is a temp
//! file in the same folder renamed over the target, so a reader (this app,
//! or the GPUI app sharing `prefs.json` and `drafts/`) sees either the old
//! or the new file, never a torn one. A failed write stays queued and is
//! retried, and [`Writer::flush`] waits for everything queued before it
//! (quit uses it).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use serde_json::{Map, Value};

/// How long a failed write waits before the next try.
const RETRY: Duration = Duration::from_secs(2);

/// What should end up at one path.
#[derive(Clone, Debug)]
enum Op {
    Write(Vec<u8>),
    Remove,
    /// Re-read the file and overwrite only these top-level keys of its JSON
    /// object; `full` is written when the file is missing or unreadable.
    Patch { changes: Map<String, Value>, full: Value },
}

enum Msg {
    Op(PathBuf, Op),
    /// Answered once everything queued before it has been tried: `true`
    /// when nothing is left failing.
    Flush(Sender<bool>),
}

#[derive(Clone)]
pub struct Writer {
    tx: Sender<Msg>,
}

impl Writer {
    /// Start a writer thread. The app uses [`global`]; tests make their own.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("zlo-writer".into())
            .spawn(move || run(rx));
        if let Err(e) = spawned {
            tracing::warn!("could not start the file writer: {e}");
        }
        Self { tx }
    }

    fn send(&self, msg: Msg) {
        if self.tx.send(msg).is_err() {
            tracing::warn!("the file writer is gone; a save was lost");
        }
    }

    pub fn write(&self, path: PathBuf, bytes: Vec<u8>) {
        self.send(Msg::Op(path, Op::Write(bytes)));
    }

    pub fn remove(&self, path: PathBuf) {
        self.send(Msg::Op(path, Op::Remove));
    }

    /// Change only `changes`' keys of the JSON object at `path`, keeping
    /// whatever another writer (the GPUI app) put in the other keys.
    pub fn patch(&self, path: PathBuf, changes: Map<String, Value>, full: Value) {
        self.send(Msg::Op(path, Op::Patch { changes, full }));
    }

    /// Wait (up to `timeout`) until everything queued so far has been
    /// written. `false` on timeout or when a write is still failing.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        self.send(Msg::Flush(tx));
        rx.recv_timeout(timeout).unwrap_or(false)
    }
}

/// The app's one writer.
pub fn global() -> &'static Writer {
    static WRITER: OnceLock<Writer> = OnceLock::new();
    WRITER.get_or_init(Writer::spawn)
}

/// Queue `op` for `path`, collapsing it into a queued op for the same path.
fn queue(pending: &mut Vec<(PathBuf, Op)>, path: PathBuf, op: Op) {
    let Some(at) = pending.iter().position(|(p, _)| *p == path) else {
        pending.push((path, op));
        return;
    };
    let (_, old) = pending.remove(at);
    let op = match (old, op) {
        // Two patches: the later keys win, the rest of the earlier stay.
        (
            Op::Patch { changes: mut keys, .. },
            Op::Patch { changes, full },
        ) => {
            keys.extend(changes);
            Op::Patch { changes: keys, full }
        }
        (_, op) => op,
    };
    pending.push((path, op));
}

fn run(rx: Receiver<Msg>) {
    let mut pending: Vec<(PathBuf, Op)> = Vec::new();
    loop {
        // Failed writes wait for the retry; otherwise sleep until a save.
        let first = if pending.is_empty() {
            match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(RETRY) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    apply_all(&mut pending);
                    return;
                }
            }
        };
        let mut flushes = Vec::new();
        let mut take = |msg: Msg, pending: &mut Vec<(PathBuf, Op)>| match msg {
            Msg::Op(path, op) => queue(pending, path, op),
            Msg::Flush(reply) => flushes.push(reply),
        };
        if let Some(msg) = first {
            take(msg, &mut pending);
        }
        while let Ok(msg) = rx.try_recv() {
            take(msg, &mut pending);
        }
        apply_all(&mut pending);
        for reply in flushes {
            let _ = reply.send(pending.is_empty());
        }
    }
}

/// Try every queued op in order; the failed ones stay queued.
fn apply_all(pending: &mut Vec<(PathBuf, Op)>) {
    pending.retain(|(path, op)| match apply(path, op) {
        Ok(()) => false,
        Err(e) => {
            tracing::warn!(path = %path.display(), "could not save a file (will retry): {e}");
            true
        }
    });
}

fn apply(path: &Path, op: &Op) -> std::io::Result<()> {
    match op {
        Op::Write(bytes) => atomic_write(path, bytes),
        Op::Remove => match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
        Op::Patch { changes, full } => {
            let on_disk = std::fs::read(path)
                .ok() // silent-ok: a missing file gets the full value
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok()); // silent-ok: an unreadable file is replaced whole
            let value = match on_disk {
                Some(Value::Object(mut keys)) => {
                    keys.extend(changes.clone());
                    Value::Object(keys)
                }
                _ => full.clone(),
            };
            let json = serde_json::to_vec_pretty(&value).map_err(std::io::Error::other)?;
            atomic_write(path, &json)
        }
    }
}

/// Write `bytes` to a temp file beside `path`, then rename it over `path`.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zlo-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saves_land_in_order_and_a_removal_is_never_undone_by_an_older_write() {
        let dir = temp("order");
        let w = Writer::spawn();
        let f = dir.join("d.json");
        for i in 0..200 {
            w.write(f.clone(), format!("{{\"n\":{i}}}").into_bytes());
        }
        assert!(w.flush(Duration::from_secs(5)));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{\"n\":199}");
        w.write(f.clone(), b"old".to_vec());
        w.remove(f.clone());
        assert!(w.flush(Duration::from_secs(5)));
        assert!(!f.exists(), "an older write resurrected a deleted file");
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 0, "temp files left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_kept_and_retried() {
        let dir = temp("retry");
        let w = Writer::spawn();
        // A file where the folder should be makes the write fail.
        let blocker = dir.join("sub");
        std::fs::write(&blocker, "x").unwrap();
        let f = blocker.join("d.json");
        w.write(f.clone(), b"kept".to_vec());
        assert!(!w.flush(Duration::from_secs(5)));
        std::fs::remove_file(&blocker).unwrap();
        assert!(w.flush(Duration::from_secs(5)));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_patch_keeps_the_keys_another_app_wrote() {
        let dir = temp("patch");
        let w = Writer::spawn();
        let f = dir.join("prefs.json");
        std::fs::write(&f, r#"{"theme":"gpui","width":1}"#).unwrap();
        let mut changes = Map::new();
        changes.insert("width".into(), Value::from(2));
        w.patch(f.clone(), changes, serde_json::json!({"theme":"rune","width":2}));
        assert!(w.flush(Duration::from_secs(5)));
        let back: Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({"theme":"gpui","width":2}));
        // A missing or corrupt file gets the whole value.
        std::fs::write(&f, "{not json").unwrap();
        w.patch(f.clone(), Map::new(), serde_json::json!({"theme":"rune"}));
        assert!(w.flush(Duration::from_secs(5)));
        let back: Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({"theme":"rune"}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queued_patches_merge_their_keys() {
        let mut pending = Vec::new();
        let p = PathBuf::from("/x");
        let one = |k: &str| {
            let mut m = Map::new();
            m.insert(k.into(), Value::from(1));
            m
        };
        queue(&mut pending, p.clone(), Op::Patch { changes: one("a"), full: Value::Null });
        queue(&mut pending, p.clone(), Op::Patch { changes: one("b"), full: Value::Null });
        assert_eq!(pending.len(), 1);
        let Op::Patch { changes, .. } = &pending[0].1 else { panic!() };
        assert!(changes.contains_key("a") && changes.contains_key("b"));
    }
}
