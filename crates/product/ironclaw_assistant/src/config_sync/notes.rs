//! Small records configuration sync keeps on this installation about its
//! own work: what a pass created here (so a later pass can take it back),
//! and copies of what it replaced. They live under the database-backed
//! settings mount, travel with this installation's store, and are never
//! themselves synced.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_filesystem::{CasExpectation, Entry, FilesystemError, RootFilesystem};
use ironclaw_host_api::path::VirtualPath;

use super::ConfigSyncError;

const NOTES_ROOT: &str = "/system/settings/config-sync";

/// A record by its path below the sync notes root. Each name is an id or a
/// fixed word: letters, digits, `-`, `_` and inner dots only.
#[async_trait]
pub trait SyncNotes: Send + Sync {
    async fn read(&self, path: &[&str]) -> Result<Option<Vec<u8>>, ConfigSyncError>;

    /// Write over whatever is there.
    async fn write(&self, path: &[&str], bytes: &[u8]) -> Result<(), ConfigSyncError>;

    /// Write only where nothing is. `Ok(false)`: a record is there already
    /// and was left as it is.
    async fn create(&self, path: &[&str], bytes: &[u8]) -> Result<bool, ConfigSyncError>;
}

pub struct SettingsNotes<F> {
    filesystem: Arc<F>,
}

impl<F> SettingsNotes<F> {
    pub fn new(filesystem: Arc<F>) -> Self {
        Self { filesystem }
    }
}

fn unavailable(reason: impl ToString) -> ConfigSyncError {
    ConfigSyncError::Unavailable {
        reason: reason.to_string(),
    }
}

/// One name of a record's path: nothing that could leave its folder.
pub(crate) fn safe_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && !raw.starts_with('.')
        && !raw.contains("..")
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn record_path(path: &[&str]) -> Result<VirtualPath, ConfigSyncError> {
    if path.is_empty() || !path.iter().all(|name| safe_name(name)) {
        return Err(unavailable("a name cannot be used for a sync record"));
    }
    VirtualPath::new(format!("{NOTES_ROOT}/{}", path.join("/"))).map_err(unavailable)
}

#[async_trait]
impl<F: RootFilesystem> SyncNotes for SettingsNotes<F> {
    async fn read(&self, path: &[&str]) -> Result<Option<Vec<u8>>, ConfigSyncError> {
        match self.filesystem.read_file(&record_path(path)?).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(FilesystemError::NotFound { .. }) => Ok(None),
            Err(error) => Err(unavailable(error)),
        }
    }

    async fn write(&self, path: &[&str], bytes: &[u8]) -> Result<(), ConfigSyncError> {
        self.filesystem
            .write_file(&record_path(path)?, bytes)
            .await
            .map_err(unavailable)
    }

    async fn create(&self, path: &[&str], bytes: &[u8]) -> Result<bool, ConfigSyncError> {
        match self
            .filesystem
            .put(
                &record_path(path)?,
                Entry::bytes(bytes.to_vec()),
                CasExpectation::Absent,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(FilesystemError::VersionMismatch { .. }) => Ok(false),
            Err(error) => Err(unavailable(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_filesystem::InMemoryBackend;

    use super::*;

    #[tokio::test]
    async fn a_record_is_read_back_and_a_copy_is_never_written_over() {
        let notes = SettingsNotes::new(Arc::new(InMemoryBackend::new()));
        let at = [
            "skill-backups",
            "tenant-a",
            "alice",
            "deploy",
            "0123456789ab",
        ];
        assert_eq!(notes.read(&at).await.expect("read"), None);
        assert!(notes.create(&at, b"first").await.expect("create"));
        assert!(!notes.create(&at, b"second").await.expect("create"));
        assert_eq!(
            notes.read(&at).await.expect("read").as_deref(),
            Some(&b"first"[..])
        );
        notes.write(&at, b"third").await.expect("write");
        assert_eq!(
            notes.read(&at).await.expect("read").as_deref(),
            Some(&b"third"[..])
        );
        for bad in [&["..", "x"][..], &["a/b"], &[""], &[".hidden"], &[]] {
            assert!(notes.read(bad).await.is_err(), "{bad:?}");
        }
    }
}
