//! The peer links this installation holds: which other environments its
//! agents may act on, where to reach them, and with what access.
//!
//! ```text
//! /tenant-shared/reborn-environment/peer-links/<environment_id>.json
//! ```
//!
//! The link's device token is not here: it lives in the operator secret
//! store (`ironclaw_product_contracts::peer::peer_link_token_handle`). A
//! forgotten link's record is removed — it holds no LLM data, only routing
//! — and the grant on the other side stays until it is revoked there.

use async_trait::async_trait;
use ironclaw_filesystem::{CasExpectation, FileType, FilesystemError, RootFilesystem};
use ironclaw_product_contracts::environment_access::EnvironmentId;
use ironclaw_product_contracts::peer::{PeerLinkError, PeerLinkRecords, PeerLinkSummary};

use super::{ENVIRONMENT_ROOT, FilesystemEnvironmentAccess, scoped_path};

/// Bound on how many links one listing reads.
const MAX_LISTED_LINKS: usize = 256;

fn link_path(environment_id: &str) -> Result<ironclaw_host_api::path::ScopedPath, PeerLinkError> {
    let id = EnvironmentId::new(environment_id)
        .map_err(|_| PeerLinkError::NotLinked(environment_id.to_string()))?;
    scoped_path(format!(
        "{ENVIRONMENT_ROOT}/peer-links/{}.json",
        id.as_str()
    ))
    .map_err(|_| PeerLinkError::Unavailable)
}

fn store_error(operation: &'static str, cause: impl std::fmt::Display) -> PeerLinkError {
    tracing::debug!(
        target: "ironclaw::reborn::environment_access",
        operation,
        error = %cause,
        "peer link store failure"
    );
    PeerLinkError::Unavailable
}

#[async_trait]
impl<F> PeerLinkRecords for FilesystemEnvironmentAccess<F>
where
    F: RootFilesystem + 'static,
{
    async fn list(&self) -> Result<Vec<PeerLinkSummary>, PeerLinkError> {
        let dir = scoped_path(format!("{ENVIRONMENT_ROOT}/peer-links"))
            .map_err(|_| PeerLinkError::Unavailable)?;
        let entries = match self.filesystem.list_dir(&self.scope, &dir).await {
            Ok(entries) => entries,
            Err(FilesystemError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(store_error("list peer links", error)),
        };
        let mut links = Vec::new();
        for entry in entries.into_iter().take(MAX_LISTED_LINKS) {
            if entry.file_type != FileType::File {
                continue;
            }
            let Some(stem) = entry.name.strip_suffix(".json") else {
                continue;
            };
            if let Some(link) = self.get(stem).await? {
                links.push(link);
            }
        }
        links.sort_by_key(|link| link.linked_at);
        Ok(links)
    }

    async fn get(&self, environment_id: &str) -> Result<Option<PeerLinkSummary>, PeerLinkError> {
        let Ok(path) = link_path(environment_id) else {
            return Ok(None);
        };
        self.read_record::<PeerLinkSummary>(&path)
            .await
            .map_err(|error| store_error("read peer link", error))
    }

    async fn put(&self, link: &PeerLinkSummary) -> Result<(), PeerLinkError> {
        let path = link_path(&link.environment_id)?;
        // Last writer wins: a link is replaced as a whole (renewed, its
        // preference or addresses changed) and never merged field by field.
        let current = self
            .filesystem
            .get(&self.scope, &path)
            .await
            .map_err(|error| store_error("read peer link", error))?;
        let cas = match current {
            Some(versioned) => CasExpectation::Version(versioned.version),
            None => CasExpectation::Absent,
        };
        self.put_record(&path, link, cas)
            .await
            .map_err(|error| store_error("write peer link", error))
    }

    async fn remove(&self, environment_id: &str) -> Result<bool, PeerLinkError> {
        let path = link_path(environment_id)?;
        match self.filesystem.delete(&self.scope, &path).await {
            Ok(()) => Ok(true),
            Err(FilesystemError::NotFound { .. }) => Ok(false),
            Err(error) => Err(store_error("remove peer link", error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use ironclaw_product_contracts::environment_access::PeerLinkAccess;
    use ironclaw_product_contracts::peer::BalancePreference;

    use super::*;
    use crate::environment_access::tests::{ready, root};

    fn link(id: &str) -> PeerLinkSummary {
        PeerLinkSummary {
            environment_id: id.to_string(),
            label: "vps".into(),
            urls: vec!["https://vps.tail9915dd.ts.net".into()],
            access: PeerLinkAccess::Operate,
            linked_at: Utc::now(),
            expires_at: Utc::now(),
            preference: BalancePreference::Normal,
            revoked: false,
        }
    }

    #[tokio::test]
    async fn links_round_trip_replace_and_forget() {
        let root = root();
        let access = ready(&root).await;
        let id = "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10";
        assert!(access.list().await.expect("list").is_empty());
        access.put(&link(id)).await.expect("put");
        let mut renewed = link(id);
        renewed.preference = BalancePreference::Prefer;
        access.put(&renewed).await.expect("replace");
        let listed = access.list().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].preference, BalancePreference::Prefer);
        assert_eq!(access.get("not-an-id").await.expect("get"), None);
        assert!(access.remove(id).await.expect("remove"));
        assert!(!access.remove(id).await.expect("second remove"));
        assert!(access.list().await.expect("list").is_empty());
    }
}
