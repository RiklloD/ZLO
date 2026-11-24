//! Files that must not outlive their use: window captures (screenshots) and
//! cursor observation files. A failed delete (other than "already gone") is
//! retried with a short bounded backoff; a path that still will not delete is
//! recorded and retried by the next sweep — on the next capture, the next
//! session, and (for stale captures) at startup — so a screenshot is never
//! left on disk indefinitely.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

/// Waits between delete attempts after the first one failed.
const DEFAULT_BACKOFF: [Duration; 3] = [
    Duration::from_millis(20),
    Duration::from_millis(100),
    Duration::from_millis(400),
];

/// Test seam over file deletion.
pub(crate) trait FileRemover: Send + Sync {
    fn remove(&self, path: &Path) -> io::Result<()>;
}

struct FsRemover;

impl FileRemover for FsRemover {
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

pub(crate) struct Leftovers {
    remover: Arc<dyn FileRemover>,
    backoff: Vec<Duration>,
    pending: Mutex<BTreeSet<PathBuf>>,
}

impl Leftovers {
    pub(crate) fn new() -> Self {
        Self::with_remover(Arc::new(FsRemover), DEFAULT_BACKOFF.to_vec())
    }

    /// Deletes through `remover` (e.g. descriptor-relative) with the default
    /// backoff.
    pub(crate) fn with_default_backoff(remover: Arc<dyn FileRemover>) -> Self {
        Self::with_remover(remover, DEFAULT_BACKOFF.to_vec())
    }

    pub(crate) fn with_remover(remover: Arc<dyn FileRemover>, backoff: Vec<Duration>) -> Self {
        Self {
            remover,
            backoff,
            pending: Mutex::new(BTreeSet::new()),
        }
    }

    fn pending_set(&self) -> MutexGuard<'_, BTreeSet<PathBuf>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One attempt; a file that is already gone counts as removed.
    fn try_remove(&self, path: &Path) -> io::Result<()> {
        match self.remover.remove(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Delete `path`, retrying with the bounded backoff; if every attempt
    /// fails, record it for the next sweep.
    pub(crate) async fn remove(&self, path: &Path) {
        let mut result = self.try_remove(path);
        for delay in &self.backoff {
            if result.is_ok() {
                return;
            }
            tokio::time::sleep(*delay).await;
            result = self.try_remove(path);
        }
        if let Err(error) = result {
            self.defer(path, &error);
        }
    }

    /// For synchronous callers (possibly holding a lock): one attempt now,
    /// then the backoff retries on the runtime, or the next sweep without one.
    pub(crate) fn remove_soon(self: &Arc<Self>, path: &Path) {
        let Err(error) = self.try_remove(path) else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let leftovers = Arc::clone(self);
                let path = path.to_path_buf();
                runtime.spawn(async move { leftovers.remove(&path).await });
            }
            Err(_) => self.defer(path, &error),
        }
    }

    fn defer(&self, path: &Path, error: &io::Error) {
        tracing::debug!(%error, "computer-use file deletion failed; retrying at the next sweep");
        self.pending_set().insert(path.to_path_buf());
    }

    /// Retry every recorded leftover once; failures stay recorded.
    pub(crate) fn sweep(&self) {
        let pending = std::mem::take(&mut *self.pending_set());
        for path in pending {
            if let Err(error) = self.try_remove(&path) {
                tracing::debug!(%error, "computer-use leftover file still cannot be deleted");
                self.pending_set().insert(path);
            }
        }
    }

    /// Delete regular files in `dir` whose name `matches` and that were last
    /// modified more than `older_than` ago: leftovers of an earlier process.
    /// Younger files may belong to a capture in flight in another process.
    /// Symlinks and other non-regular entries are never followed or touched,
    /// and with `owner_uid` only files that user owns are considered.
    pub(crate) fn sweep_stale(
        &self,
        dir: &Path,
        matches: &dyn Fn(&str) -> bool,
        older_than: Duration,
        owner_uid: Option<u32>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::debug!(%error, "computer-use capture directory unreadable for sweep");
                return;
            }
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !matches(name) {
                continue;
            }
            let path = entry.path();
            // lstat: a symlink is reported as a symlink, never its target.
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue; // silent-ok: vanished or unreadable entry; nothing to delete
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            #[cfg(unix)]
            if let Some(owner_uid) = owner_uid
                && std::os::unix::fs::MetadataExt::uid(&metadata) != owner_uid
            {
                continue;
            }
            #[cfg(not(unix))]
            let _ = owner_uid;
            let stale = metadata
                .modified()
                .map(|modified| now.duration_since(modified).unwrap_or_default() >= older_than)
                // An unreadable timestamp: treat as stale rather than keep a
                // screenshot forever.
                .unwrap_or(true); // silent-ok: unknown age sweeps, never retains
            if !stale {
                continue;
            }
            if let Err(error) = self.try_remove(&path) {
                self.defer(&path, &error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Fails the first `failures` deletions with a permission error, then
    /// really deletes.
    struct FlakyRemover {
        failures: AtomicUsize,
        calls: AtomicUsize,
    }

    impl FlakyRemover {
        fn failing(times: usize) -> Arc<Self> {
            Arc::new(Self {
                failures: AtomicUsize::new(times),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl FileRemover for FlakyRemover {
        fn remove(&self, path: &Path) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self
                .failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            std::fs::remove_file(path)
        }
    }

    fn pending(leftovers: &Leftovers) -> Vec<PathBuf> {
        leftovers.pending_set().iter().cloned().collect()
    }

    fn fast_backoff() -> Vec<Duration> {
        vec![Duration::from_millis(1); 3]
    }

    fn screenshot(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"jpeg bytes").unwrap();
        path
    }

    #[tokio::test]
    async fn a_transient_delete_failure_is_retried_within_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = screenshot(dir.path(), "1_0.jpg");
        let remover = FlakyRemover::failing(2);
        let leftovers = Leftovers::with_remover(remover.clone(), fast_backoff());

        leftovers.remove(&path).await;

        assert!(!path.exists());
        assert_eq!(remover.calls.load(Ordering::SeqCst), 3);
        assert!(pending(&leftovers).is_empty());
    }

    #[tokio::test]
    async fn a_persistent_delete_failure_is_recorded_and_swept_later() {
        let dir = tempfile::tempdir().unwrap();
        let path = screenshot(dir.path(), "1_0.jpg");
        // First attempt + three retries all fail.
        let remover = FlakyRemover::failing(4);
        let leftovers = Leftovers::with_remover(remover.clone(), fast_backoff());

        leftovers.remove(&path).await;

        assert!(path.exists(), "the delete failed every attempt");
        assert_eq!(pending(&leftovers), vec![path.clone()]);

        leftovers.sweep();

        assert!(
            !path.exists(),
            "the sweep must delete the leftover screenshot"
        );
        assert!(pending(&leftovers).is_empty());
    }

    #[tokio::test]
    async fn a_missing_file_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let leftovers = Leftovers::new();

        leftovers
            .remove(&dir.path().join("never-written.jpg"))
            .await;

        assert!(pending(&leftovers).is_empty());
    }

    #[test]
    fn stale_sweep_deletes_only_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let capture = screenshot(dir.path(), "ironclaw_cu_9_1.jpg");
        let unrelated = screenshot(dir.path(), "notes.jpg");
        let leftovers = Leftovers::new();

        leftovers.sweep_stale(
            dir.path(),
            &|name| name.starts_with("ironclaw_cu_") && name.ends_with(".jpg"),
            Duration::ZERO,
            None,
        );

        assert!(!capture.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn stale_sweep_keeps_files_younger_than_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let in_flight = screenshot(dir.path(), "ironclaw_cu_9_1.jpg");
        let leftovers = Leftovers::new();

        leftovers.sweep_stale(
            dir.path(),
            &|name| name.starts_with("ironclaw_cu_") && name.ends_with(".jpg"),
            Duration::from_secs(3600),
            None,
        );

        assert!(in_flight.exists());
    }

    #[test]
    fn sync_removal_without_a_runtime_defers_to_the_next_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let path = screenshot(dir.path(), "cursor");
        let leftovers = Arc::new(Leftovers::with_remover(
            FlakyRemover::failing(1),
            fast_backoff(),
        ));

        leftovers.remove_soon(&path);
        assert_eq!(pending(&leftovers), vec![path.clone()]);

        leftovers.sweep();
        assert!(!path.exists());
    }
}
