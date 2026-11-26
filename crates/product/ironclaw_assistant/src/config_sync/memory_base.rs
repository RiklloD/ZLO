//! Where memory sync keeps, per caller and other computer, the content it
//! last agreed on ([`MemorySyncBaseStore`]): one small JSON record under
//! the database-backed settings mount, so it travels with this
//! installation's store and is never itself synced.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_filesystem::{FilesystemError, RootFilesystem};
use ironclaw_host_api::path::VirtualPath;
use ironclaw_product_contracts::config_sync::ConfigSyncDomain;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;

use super::ConfigSyncError;
use super::memory::MemorySyncBaseStore;

const MEMORY_BASE_ROOT: &str = "/system/settings/config-sync/memory-base";

pub struct SettingsMemorySyncBase<F> {
    filesystem: Arc<F>,
}

impl<F> SettingsMemorySyncBase<F> {
    pub fn new(filesystem: Arc<F>) -> Self {
        Self { filesystem }
    }
}

/// One path segment made of an id: nothing that could leave its folder.
fn segment(raw: &str) -> Result<&str, ConfigSyncError> {
    let safe = !raw.is_empty()
        && raw.len() <= 128
        && !raw.starts_with('.')
        && !raw.contains("..")
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    safe.then_some(raw).ok_or_else(|| ConfigSyncError::Store {
        domain: ConfigSyncDomain::Memory,
        reason: "an id cannot name a memory sync record".to_string(),
    })
}

fn base_path(caller: &ProductSurfaceCaller, peer: &str) -> Result<VirtualPath, ConfigSyncError> {
    VirtualPath::new(format!(
        "{MEMORY_BASE_ROOT}/{}/{}/{}.json",
        segment(caller.tenant_id.as_str())?,
        segment(caller.user_id.as_str())?,
        segment(peer)?,
    ))
    .map_err(|error| ConfigSyncError::Store {
        domain: ConfigSyncDomain::Memory,
        reason: error.to_string(),
    })
}

fn base_error(error: FilesystemError) -> ConfigSyncError {
    ConfigSyncError::Store {
        domain: ConfigSyncDomain::Memory,
        reason: error.to_string(),
    }
}

#[async_trait]
impl<F: RootFilesystem> MemorySyncBaseStore for SettingsMemorySyncBase<F> {
    async fn load(
        &self,
        caller: &ProductSurfaceCaller,
        peer_environment_id: &str,
    ) -> Result<BTreeMap<String, String>, ConfigSyncError> {
        let path = base_path(caller, peer_environment_id)?;
        let bytes = match self.filesystem.read_file(&path).await {
            Ok(bytes) => bytes,
            Err(FilesystemError::NotFound { .. }) => return Ok(BTreeMap::new()),
            Err(error) => return Err(base_error(error)),
        };
        serde_json::from_slice(&bytes).map_err(|source| ConfigSyncError::Serialization {
            domain: ConfigSyncDomain::Memory,
            source,
        })
    }

    async fn save(
        &self,
        caller: &ProductSurfaceCaller,
        peer_environment_id: &str,
        base: &BTreeMap<String, String>,
    ) -> Result<(), ConfigSyncError> {
        let path = base_path(caller, peer_environment_id)?;
        let bytes = serde_json::to_vec(base).map_err(|source| ConfigSyncError::Serialization {
            domain: ConfigSyncDomain::Memory,
            source,
        })?;
        self.filesystem
            .write_file(&path, &bytes)
            .await
            .map_err(base_error)
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_filesystem::InMemoryBackend;
    use ironclaw_host_api::ids::{TenantId, UserId};

    use super::*;

    fn caller(user: &str) -> ProductSurfaceCaller {
        ProductSurfaceCaller::new(
            TenantId::new("tenant-a").expect("tenant"),
            UserId::new(user).expect("user"),
            None,
            None,
        )
    }

    #[tokio::test]
    async fn a_base_is_kept_per_caller_and_per_other_computer() {
        let store = SettingsMemorySyncBase::new(Arc::new(InMemoryBackend::new()));
        let alice = caller("alice");
        assert!(store.load(&alice, "box").await.expect("load").is_empty());
        let base = BTreeMap::from([("MEMORY.md".to_string(), "sha256:1".to_string())]);
        store.save(&alice, "box", &base).await.expect("save");
        assert_eq!(store.load(&alice, "box").await.expect("load"), base);
        assert!(store.load(&alice, "studio").await.expect("load").is_empty());
        assert!(
            store
                .load(&caller("bob"), "box")
                .await
                .expect("load")
                .is_empty()
        );
    }

    #[test]
    fn an_id_that_could_leave_its_folder_names_no_record() {
        for bad in ["", "..", "a/b", ".hidden", "a..b", "a b"] {
            assert!(segment(bad).is_err(), "{bad:?}");
        }
        assert!(segment("0f6c2d0e-3b0a-4a51").is_ok());
        assert!(segment("user.name_1").is_ok());
    }
}
