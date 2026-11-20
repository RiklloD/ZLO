//! The transcript IronClaw keeps for agents whose own session store it cannot
//! read (Antigravity's conversation database, Muse's session log): one JSONL
//! file per session, a `HistoryMessage` per line, append-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole, RuntimeError};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// How long a caller waits for its rows to land. A slower write (a stalled
/// disk, a FIFO nobody reads) is not dropped: it stays queued in the file's
/// writer and lands when the file takes it, after every row queued before
/// it; the caller moves on, so a turn never hangs its teardown on it.
const APPEND_WAIT: Duration = Duration::from_secs(2);

type Pending = (String, oneshot::Sender<()>);

/// One writer per transcript file: appends land in the order they were
/// made, however long one of them takes.
fn writer(path: &Path) -> mpsc::UnboundedSender<Pending> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, mpsc::UnboundedSender<Pending>>>> =
        OnceLock::new();
    let writers = WRITERS.get_or_init(Mutex::default);
    // A poisoned map only follows a panic elsewhere; its data is intact.
    let mut writers = writers.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(tx) = writers.get(path).filter(|tx| !tx.is_closed()) {
        return tx.clone();
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Pending>();
    let file = path.to_path_buf();
    tokio::spawn(async move {
        while let Some((body, done)) = rx.recv().await {
            if let Err(error) = write(&file, &body).await {
                tracing::debug!(%error, "journal: transcript not written");
            }
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the caller may have stopped waiting"
            )]
            let _ = done.send(());
        }
    });
    writers.insert(path.to_path_buf(), tx.clone());
    tx
}

async fn write(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(body.as_bytes()).await?;
    file.flush().await
}

pub(crate) async fn append(path: &Path, messages: &[HistoryMessage]) {
    append_within(path, messages, APPEND_WAIT).await;
}

/// [`append`] with another wait bound; `true` once the rows landed.
pub(crate) async fn append_within(
    path: &Path,
    messages: &[HistoryMessage],
    wait: Duration,
) -> bool {
    let mut body = String::new();
    for message in messages {
        match serde_json::to_string(message) {
            Ok(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            Err(error) => tracing::debug!(%error, "journal: message not encoded"),
        }
    }
    let (done, landed) = oneshot::channel();
    if writer(path).send((body, done)).is_err() {
        tracing::debug!("journal: transcript writer gone; rows not written");
        return false;
    }
    match tokio::time::timeout(wait, landed).await {
        Ok(landed) => landed.is_ok(),
        Err(_) => {
            tracing::debug!(
                "journal: transcript write still pending; it lands when the file takes it"
            );
            false
        }
    }
}

pub(crate) async fn read(path: &Path, limit: usize) -> Result<Vec<HistoryMessage>, RuntimeError> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(RuntimeError::Unavailable(format!(
                "transcript: {}",
                e.kind()
            )));
        }
    };
    let mut out: Vec<HistoryMessage> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok()) // silent-ok: a torn last line is skipped
        .collect();
    let skip = out.len().saturating_sub(limit);
    Ok(out.split_off(skip))
}

pub(crate) fn message(role: HistoryRole, text: String, tool_calls: Vec<String>) -> HistoryMessage {
    HistoryMessage {
        role,
        text,
        tool_calls,
        run_id: None,
        hidden: false,
        at: None,
    }
}

/// Persist a CLI's own limit reading for the Usage page (atomic replace).
/// Losing one is harmless: the next turn writes another.
pub(crate) async fn write_limit_reading(path: &Path, key: &str, reading: &serde_json::Value) {
    let body =
        serde_json::json!({"observedAt": ironclaw_orchestrator::model::now_ms(), key: reading})
            .to_string();
    let tmp = path.with_extension("json.tmp");
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "limit reading not saved");
    }
}

/// Words a provider uses when a subscription or quota limit stops a turn.
pub(crate) fn mentions_usage_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "usage limit",
        "rate limit",
        "quota",
        "resource_exhausted",
        "resource exhausted",
        "weekly limit",
        "session limit",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sol 6: an append stuck on a file nobody can take (a FIFO without a
    /// reader) does not hold its caller; the rows are kept and land, in
    /// order, once the file takes them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_append_releases_its_caller_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("t.jsonl");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let row = |text: &str| message(HistoryRole::User, text.into(), Vec::new());
        let started = std::time::Instant::now();
        assert!(!append_within(&fifo, &[row("one")], Duration::from_millis(200)).await);
        assert!(!append_within(&fifo, &[row("two")], Duration::from_millis(200)).await);
        assert!(started.elapsed() < Duration::from_secs(2));
        // Keep reading the FIFO: each open of the writer finds a reader, so
        // every queued row lands (and the runtime can shut down).
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = fifo.clone();
        std::thread::spawn(move || {
            loop {
                let mut chunk = String::new();
                match std::fs::File::open(&reader) {
                    Ok(mut file) => {
                        if std::io::Read::read_to_string(&mut file, &mut chunk).is_err() {
                            return;
                        }
                        if tx.send(chunk).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        let mut kept = String::new();
        while !kept.contains("two") {
            kept.push_str(&rx.recv_timeout(Duration::from_secs(5)).expect("rows land"));
        }
        let one = kept.find("one").expect("first row kept");
        assert!(one < kept.find("two").unwrap(), "in order: {kept}");
    }
}
