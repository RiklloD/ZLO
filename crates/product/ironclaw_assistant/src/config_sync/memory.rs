//! `memory` domain handler: the local memory documents (`MEMORY.md`, daily
//! logs, notes, profile files), through the Memory page's documents port.
//!
//! Two-way, and nothing an agent wrote is lost. Each side remembers, per
//! other computer, the content it last agreed on for every document (the
//! base, [`MemorySyncBaseStore`]). With it a document changed on one side
//! only moves to the other; one removed here stays removed; and one changed
//! on both sides is settled once: when one text continues the other the
//! longer wins, otherwise the source's text takes the path and the
//! receiver's is kept beside it under `sync-conflicts/`. The return leg
//! never replaces what the receiver changed.
//!
//! Search indexes and embeddings are rebuilt by the provider on write and
//! are not carried.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload,
};
use ironclaw_product_contracts::memory_page::{
    MAX_MEMORY_PATH_BYTES, MemoryDocumentsSource, MemoryPageWriteOutcome, MemoryWriteRequest,
};
use ironclaw_product_contracts::surface::{ProductSurfaceCaller, ProductSurfaceError};

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult, items,
};

const DOMAIN: ConfigSyncDomain = ConfigSyncDomain::Memory;
/// Where the receiver's text goes when both sides changed a document.
pub const CONFLICTS_ROOT: &str = "sync-conflicts";
/// How long a computed index answers manifests before documents are read
/// again. Short: it only spares the reads when several computers are
/// checked at the same moment; an edit must show at the next check.
pub const INDEX_TTL: Duration = Duration::from_secs(2);

/// The content last agreed with one other computer: document path to
/// digest. Machine-local state of this backend; never synced itself.
#[async_trait]
pub trait MemorySyncBaseStore: Send + Sync {
    async fn load(
        &self,
        caller: &ProductSurfaceCaller,
        peer_environment_id: &str,
    ) -> Result<BTreeMap<String, String>, ConfigSyncError>;

    async fn save(
        &self,
        caller: &ProductSurfaceCaller,
        peer_environment_id: &str,
        base: &BTreeMap<String, String>,
    ) -> Result<(), ConfigSyncError>;
}

type Index = BTreeMap<String, String>;

pub struct MemoryDomainHandler {
    documents: Arc<dyn MemoryDocumentsSource>,
    base: Arc<dyn MemorySyncBaseStore>,
    index_ttl: Duration,
    /// The last index per caller (tenant, user), while it is fresh.
    cached: Mutex<Option<(Instant, String, Index)>>,
}

impl MemoryDomainHandler {
    pub fn new(
        documents: Arc<dyn MemoryDocumentsSource>,
        base: Arc<dyn MemorySyncBaseStore>,
    ) -> Self {
        Self {
            documents,
            base,
            index_ttl: INDEX_TTL,
            cached: Mutex::new(None),
        }
    }

    /// Tests: read documents on every call.
    pub fn with_index_ttl(mut self, ttl: Duration) -> Self {
        self.index_ttl = ttl;
        self
    }

    fn who(caller: &ProductSurfaceCaller) -> String {
        format!("{}/{}", caller.tenant_id.as_str(), caller.user_id.as_str())
    }

    fn forget_index(&self) {
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Every document here: path to content digest.
    async fn index(
        &self,
        caller: &ProductSurfaceCaller,
        fresh: bool,
    ) -> Result<Index, ConfigSyncError> {
        let who = Self::who(caller);
        if !fresh
            && let Some((at, whose, index)) = self
                .cached
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            && *whose == who
            && at.elapsed() < self.index_ttl
        {
            return Ok(index.clone());
        }
        let mut index = Index::new();
        for entry in self.documents.tree(caller).await.map_err(store_error)? {
            match self.documents.read(caller, &entry.path).await {
                Ok(document) => {
                    index.insert(entry.path, items::sha256(document.content.as_bytes()));
                }
                // Gone between the listing and the read.
                Err(error) => {
                    tracing::debug!(path = %entry.path, %error, "config sync: memory document not read");
                }
            }
        }
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), who, index.clone()));
        Ok(index)
    }

    /// Write `content` at `path` over what is there now (`None`: nothing).
    /// `Ok(false)`: it changed under us; nothing was written.
    async fn put(
        &self,
        caller: &ProductSurfaceCaller,
        path: &str,
        content: &str,
        exists: bool,
    ) -> Result<bool, ProductSurfaceError> {
        let expected_content_hash = if exists {
            Some(self.documents.read(caller, path).await?.content_hash)
        } else {
            None
        };
        let written = self
            .documents
            .write(
                caller,
                MemoryWriteRequest {
                    path: path.to_string(),
                    content: content.to_string(),
                    expected_content_hash,
                },
            )
            .await?;
        Ok(written.outcome == MemoryPageWriteOutcome::Written)
    }
}

fn store_error(error: ProductSurfaceError) -> ConfigSyncError {
    ConfigSyncError::Store {
        domain: DOMAIN,
        reason: error.to_string(),
    }
}

/// Where the receiver's version of `path` is kept: its own content names
/// the folder, so the same conflict lands in the same place on a retry.
fn conflict_path(path: &str, held_hash: &str) -> Option<String> {
    let short: String = held_hash
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect();
    let out = format!("{CONFLICTS_ROOT}/{short}/{path}");
    (out.len() <= MAX_MEMORY_PATH_BYTES).then_some(out)
}

/// What to do with one incoming document.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Both sides hold the same: remember it.
    Agree,
    /// Leave what is here (or its absence) alone.
    Keep,
    Create,
    Replace,
    /// Both changed and neither continues the other: needs the texts.
    Compare,
}

fn step(incoming: &str, held: Option<&String>, base: Option<&String>, has_content: bool) -> Step {
    match held {
        Some(held) if held == incoming => Step::Agree,
        _ if !has_content => Step::Keep,
        // Agreed on before and gone here since: it was removed here.
        None if base.is_some_and(|b| b == incoming) => Step::Keep,
        None => Step::Create,
        Some(held) if base == Some(held) => Step::Replace,
        Some(_) if base.is_some_and(|b| b == incoming) => Step::Keep,
        Some(_) => Step::Compare,
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for MemoryDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        DOMAIN
    }

    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let index = self.index(&ctx.caller, false).await?;
        Ok(ConfigSyncDomainDigest {
            hash: items::domain_hash(&index),
            updated_at: None,
            item_count: index.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let index = self.index(&ctx.caller, true).await?;
        let page = items::page(&index, &ctx.page, |path| {
            let documents = Arc::clone(&self.documents);
            let caller = ctx.caller.clone();
            async move {
                match documents.read(&caller, &path).await {
                    Ok(document) => Ok(Some(vec![(String::new(), document.content.into_bytes())])),
                    Err(error) => {
                        tracing::debug!(%path, %error, "config sync: memory document not exported");
                        Ok(None)
                    }
                }
            }
        })
        .await?;
        Ok(ConfigSyncDomainPayload::Memory(page))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::Memory(page) = payload else {
            return Err(ConfigSyncError::validation(
                "memory",
                "expected memory payload",
            ));
        };
        items::validate(DOMAIN, page)?;
        for item in &page.items {
            if item.key.len() > MAX_MEMORY_PATH_BYTES {
                return Err(ConfigSyncError::validation(
                    "memory.items",
                    "a document path is too long",
                ));
            }
            if let Some(files) = items::decode(DOMAIN, item)?
                && (files.len() != 1 || std::str::from_utf8(&files[0].1).is_err())
            {
                return Err(ConfigSyncError::validation(
                    "memory.items",
                    format!("document `{}` is not one UTF-8 text", item.key),
                ));
            }
        }
        Ok(())
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::Memory(page) = payload else {
            return Err(ConfigSyncError::validation(
                "memory",
                "expected memory payload",
            ));
        };
        let Some(peer) = ctx.peer_environment_id.as_deref() else {
            return Err(ConfigSyncError::validation(
                "snapshot.source_environment_id",
                "memory sync needs to know which computer the documents come from",
            ));
        };
        let caller = &ctx.caller;
        let held = self.index(caller, true).await?;
        let mut base = self.base.load(caller, peer).await?;
        let before = base.clone();
        let (mut applied, mut skipped) = (0u64, 0u64);
        let mut warnings: Vec<String> = page
            .too_large
            .iter()
            .map(|item| format!("memory document {item} is too large to sync"))
            .collect();

        for item in &page.items {
            let path = item.key.as_str();
            let content = items::decode(DOMAIN, item)?
                .and_then(|mut files| files.pop())
                .and_then(|(_, bytes)| String::from_utf8(bytes).ok());
            let mine = held.get(path);
            let mut todo = step(&item.hash, mine, base.get(path), content.is_some());
            if todo == Step::Agree {
                base.insert(path.to_string(), item.hash.clone());
                skipped += 1;
                continue;
            }
            let Some(content) = content else {
                skipped += 1;
                continue;
            };
            let mut keep_copy = None;
            if todo == Step::Compare {
                let existing = match self.documents.read(caller, path).await {
                    Ok(document) => document.content,
                    Err(error) => {
                        warnings.push(format!("{path} was not synced: {error}"));
                        skipped += 1;
                        continue;
                    }
                };
                todo = if content.starts_with(&existing) {
                    Step::Replace
                } else if existing.starts_with(&content) || ctx.merge_back {
                    Step::Keep
                } else {
                    keep_copy = Some(existing);
                    Step::Replace
                };
            }
            if todo == Step::Keep {
                skipped += 1;
                continue;
            }
            // The receiver's own text is put aside before it is replaced.
            if let (Some(existing), Some(mine)) = (keep_copy, mine) {
                let Some(aside) = conflict_path(path, mine) else {
                    warnings.push(format!(
                        "{path} changed on both computers and its path is too long to keep both; left as it is here"
                    ));
                    skipped += 1;
                    continue;
                };
                match self.put(caller, &aside, &existing, held.contains_key(&aside)).await {
                    Ok(_) => warnings.push(format!(
                        "{path} changed on both computers; this computer's version is kept at {aside}"
                    )),
                    Err(error) => {
                        warnings.push(format!("{path} was not synced: {error}"));
                        skipped += 1;
                        continue;
                    }
                }
            }
            match self
                .put(caller, path, &content, todo == Step::Replace)
                .await
            {
                Ok(true) => {
                    base.insert(path.to_string(), item.hash.clone());
                    applied += 1;
                }
                Ok(false) => {
                    warnings.push(format!(
                        "{path} changed while syncing; it is taken next time"
                    ));
                    skipped += 1;
                }
                Err(error) => {
                    tracing::debug!(%path, %error, "config sync: memory document not written");
                    warnings.push(format!("{path} was not synced: {error}"));
                    skipped += 1;
                }
            }
        }

        if applied > 0 {
            self.forget_index();
        }
        if base != before {
            self.base.save(caller, peer, &base).await?;
        }
        let mut result = if applied == 0 {
            DomainApplyResult::unchanged(skipped)
        } else {
            DomainApplyResult::applied(applied, skipped)
        };
        result.outcome.warnings = warnings;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_case_of_the_three_way_rule() {
        let (a, b) = ("a".to_string(), "b".to_string());
        // Same on both sides.
        assert_eq!(step("a", Some(&a), None, true), Step::Agree);
        // New to this side.
        assert_eq!(step("a", None, None, true), Step::Create);
        // Agreed on, then removed here: stays removed.
        assert_eq!(step("a", None, Some(&a), true), Step::Keep);
        // Removed here, changed there since: it comes back.
        assert_eq!(step("b", None, Some(&a), true), Step::Create);
        // Only the other side changed.
        assert_eq!(step("b", Some(&a), Some(&a), true), Step::Replace);
        // Only this side changed.
        assert_eq!(step("a", Some(&b), Some(&a), true), Step::Keep);
        // Both changed, or never agreed on.
        assert_eq!(step("c", Some(&b), Some(&a), true), Step::Compare);
        assert_eq!(step("c", Some(&b), None, true), Step::Compare);
        // The sender believed this side held it already.
        assert_eq!(step("c", Some(&b), Some(&a), false), Step::Keep);
    }

    #[test]
    fn a_conflict_copy_is_named_by_the_content_it_keeps() {
        assert_eq!(
            conflict_path("notes/plan.md", "sha256:0123456789abcdef0123").as_deref(),
            Some("sync-conflicts/0123456789ab/notes/plan.md")
        );
        assert_eq!(
            conflict_path(&"x".repeat(MAX_MEMORY_PATH_BYTES), "sha256:ab"),
            None
        );
    }
}
