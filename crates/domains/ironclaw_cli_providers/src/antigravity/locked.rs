//! Read-merge-write of the driver's small local state files (the model
//! list, the sign-in note) under an OS advisory lock on a sidecar
//! `<file>.lock`. Two writers (a turn and a catalog refresh, or two ZLO
//! processes on one state folder) each see the other's last write before
//! merging their own, so neither drops what the other learned.
//!
//! Why a file lock rather than the repo's bounded CAS helper: these are
//! local cache files of one driver, not a durable store behind
//! `RootFilesystem`, and the lock is held only for one read and one
//! rename on the blocking pool, never across agent or network I/O. The
//! wait for it is bounded; a writer that cannot get it skips its update.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// How long a writer waits for another one to finish.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// Replace `path` with `update(current)`, atomically (a private temp file
/// renamed over it), under the file's lock. `update` returning `None`
/// leaves the file as it is. `Ok(false)`: nothing written.
pub(super) async fn update<F>(path: PathBuf, update: F) -> std::io::Result<bool>
where
    F: FnOnce(Option<&str>) -> Option<String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || update_blocking(&path, update))
        .await
        .map_err(|e| std::io::Error::other(format!("lock task: {e}")))?
}

fn update_blocking<F>(path: &Path, update: F) -> std::io::Result<bool>
where
    F: FnOnce(Option<&str>) -> Option<String>,
{
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("state file has no name"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path.with_file_name(format!("{name}.lock")))?;
    let deadline = Instant::now() + LOCK_WAIT;
    let _held = loop {
        match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
            Ok(held) => break held,
            Err((file, Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                lock_file = file;
                std::thread::sleep(LOCK_RETRY);
            }
            Err((_, errno)) => return Err(std::io::Error::from(errno)),
        }
    };
    let current = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let Some(next) = update(current.as_deref()) else {
        return Ok(false);
    };
    let tmp = path.with_file_name(format!("{name}.{}.tmp", uuid::Uuid::new_v4()));
    let written = std::fs::write(&tmp, next).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(error) = written {
        if let Err(cleanup) = std::fs::remove_file(&tmp) {
            tracing::debug!(%cleanup, "antigravity: state temp file not removed");
        }
        return Err(error);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Overlapping writers each merge into what the others wrote.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_updates_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("list.json");
        let writers: Vec<_> = (0..16)
            .map(|n| {
                let path = path.clone();
                tokio::spawn(async move {
                    update(path, move |current| {
                        let mut all: Vec<u32> = current
                            .and_then(|c| serde_json::from_str(c).ok())
                            .unwrap_or_default();
                        // Widen the race window between read and write.
                        std::thread::sleep(Duration::from_millis(2));
                        all.push(n);
                        Some(serde_json::to_string(&all).unwrap())
                    })
                    .await
                    .unwrap()
                })
            })
            .collect();
        for writer in writers {
            assert!(writer.await.unwrap());
        }
        let mut all: Vec<u32> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        all.sort_unstable();
        assert_eq!(all, (0..16).collect::<Vec<_>>());
    }
}
