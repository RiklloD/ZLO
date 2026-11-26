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
/// Folders tried for one conflict copy before the document is left alone.
const CONFLICT_SLOTS: usize = 4;
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

    /// Record what a pass agreed on for that computer. Each change names
    /// the value the pass saw before (`None`: no entry); a path another
    /// pass has moved on meanwhile is left as that pass recorded it.
    async fn record(
        &self,
        caller: &ProductSurfaceCaller,
        peer_environment_id: &str,
        changes: &BTreeMap<String, BaseChange>,
    ) -> Result<(), ConfigSyncError>;
}

/// One path's step in the base: from what the pass saw to what it agreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseChange {
    pub seen: Option<String>,
    pub agreed: String,
}

/// The provider's archive tree: a removed document's text is kept there
/// (`archive/<when>/<path>`). It is evidence of a removal on this computer
/// and is not itself carried (the provider does not take writes there).
const ARCHIVE_ROOT: &str = "archive/";

/// One document here: its content digest (the item hash) and the
/// provider's own token for that content, which a conditional write names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    hash: String,
    token: String,
}

type Index = BTreeMap<String, Held>;

/// What travels: every document outside the archive tree, by digest.
fn hashes(index: &Index) -> BTreeMap<String, String> {
    index
        .iter()
        .filter(|(path, _)| !path.starts_with(ARCHIVE_ROOT))
        .map(|(path, held)| (path.clone(), held.hash.clone()))
        .collect()
}

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
                    index.insert(
                        entry.path,
                        Held {
                            hash: items::sha256(document.content.as_bytes()),
                            token: document.content_hash,
                        },
                    );
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

    /// Write `content` at `path` only over the version `over` names (`None`:
    /// only where nothing is). `Ok(false)`: something else is there now and
    /// nothing was written.
    async fn put(
        &self,
        caller: &ProductSurfaceCaller,
        path: &str,
        content: &str,
        over: Option<&Held>,
    ) -> Result<bool, ProductSurfaceError> {
        let written = self
            .documents
            .write(
                caller,
                MemoryWriteRequest {
                    path: path.to_string(),
                    content: content.to_string(),
                    expected_content_hash: over.map(|held| held.token.clone()),
                },
            )
            .await?;
        Ok(written.outcome == MemoryPageWriteOutcome::Written)
    }

    /// Keep `text` (this computer's version of `path`, digest `hash`)
    /// beside the document before it is replaced. `Some(where)` only once
    /// a copy with exactly that text is confirmed to exist; an existing
    /// document with other text is never written over.
    async fn keep_aside(
        &self,
        caller: &ProductSurfaceCaller,
        held: &Index,
        path: &str,
        hash: &str,
        text: &str,
    ) -> Option<String> {
        for attempt in 0..CONFLICT_SLOTS {
            let aside = conflict_path(path, hash, attempt)?;
            match held.get(&aside) {
                // Listed with this text: confirmed by reading it now, since
                // it may have been edited after the listing.
                Some(existing) if existing.hash == hash => {
                    match self.documents.read(caller, &aside).await {
                        Ok(document) if document.content == text => return Some(aside),
                        _ => continue,
                    }
                }
                // Someone changed an earlier copy: it is theirs now.
                Some(_) => continue,
                None => match self.put(caller, &aside, text, None).await {
                    Ok(true) => return Some(aside),
                    Ok(false) => continue,
                    Err(error) => {
                        tracing::debug!(%aside, %error, "config sync: conflict copy not written");
                        return None;
                    }
                },
            }
        }
        None
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
/// `slot` above zero names the next folder, for when that place is taken
/// by other text.
fn conflict_path(path: &str, held_hash: &str, slot: usize) -> Option<String> {
    let short: String = held_hash
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect();
    let folder = if slot == 0 {
        short
    } else {
        format!("{short}-{}", slot + 1)
    };
    let out = format!("{CONFLICTS_ROOT}/{folder}/{path}");
    (out.len() <= MAX_MEMORY_PATH_BYTES).then_some(out)
}

/// Whether `path` was archived on this computer: the provider keeps a
/// removed document at `archive/<when>/<path>`, whatever its text was by
/// then.
fn archived_here(held: &Index, path: &str) -> bool {
    held.keys().any(|at| {
        at.strip_prefix(ARCHIVE_ROOT)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, archived)| archived == path)
    })
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
        let index = hashes(&self.index(&ctx.caller, false).await?);
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
        // A later page of the same pass reads the index the first one made.
        let index = hashes(&self.index(&ctx.caller, ctx.page.after.is_none()).await?);
        let page = items::page(&index, &ctx.page, |path| {
            let documents = Arc::clone(&self.documents);
            let caller = ctx.caller.clone();
            let listed = index.get(&path).cloned();
            async move {
                match documents.read(&caller, &path).await {
                    // Changed since it was listed: its hash would name other
                    // bytes. It is taken on the next pass.
                    Ok(document)
                        if listed.as_deref()
                            != Some(items::sha256(document.content.as_bytes()).as_str()) =>
                    {
                        Ok(None)
                    }
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
            if let Some(files) = items::decode(DOMAIN, item)? {
                if files.len() != 1 || std::str::from_utf8(&files[0].1).is_err() {
                    return Err(ConfigSyncError::validation(
                        "memory.items",
                        format!("document `{}` is not one UTF-8 text", item.key),
                    ));
                }
                // The hash decides the merge and is what gets remembered:
                // it must be the hash of the text that came with it.
                if items::sha256(&files[0].1) != item.hash {
                    return Err(ConfigSyncError::validation(
                        "memory.items",
                        format!("document `{}` does not match its hash", item.key),
                    ));
                }
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
        let base = self.base.load(caller, peer).await?;
        // What this pass and the other computer now agree on.
        let mut agreed: BTreeMap<String, BaseChange> = BTreeMap::new();
        let mut agree = |path: &str, hash: &str| {
            agreed.insert(
                path.to_string(),
                BaseChange {
                    seen: base.get(path).cloned(),
                    agreed: hash.to_string(),
                },
            );
        };
        let (mut applied, mut skipped) = (0u64, 0u64);
        let mut warnings: Vec<String> = page
            .not_carried
            .iter()
            .map(|item| format!("memory document {item} was not carried"))
            .collect();

        for item in &page.items {
            let path = item.key.as_str();
            // The archive tree is each computer's own.
            if path.starts_with(ARCHIVE_ROOT) {
                skipped += 1;
                continue;
            }
            let content = items::decode(DOMAIN, item)?
                .and_then(|mut files| files.pop())
                .and_then(|(_, bytes)| String::from_utf8(bytes).ok());
            let mine = held.get(path);
            let mut todo = step(
                &item.hash,
                mine.map(|held| &held.hash),
                base.get(path),
                content.is_some(),
            );
            if todo == Step::Agree {
                if base.get(path) != Some(&item.hash) {
                    agree(path, &item.hash);
                }
                skipped += 1;
                continue;
            }
            let Some(content) = content else {
                skipped += 1;
                continue;
            };
            // Never agreed on, and archived here: it was removed on this
            // computer (whatever its text was by then) and stays removed.
            if todo == Step::Create && !base.contains_key(path) && archived_here(&held, path) {
                agree(path, &item.hash);
                skipped += 1;
                continue;
            }
            if let (Step::Compare, Some(mine)) = (&todo, mine) {
                let existing = match self.documents.read(caller, path).await {
                    // The text the decision is made on must be the text
                    // that was listed, which the write below names.
                    Ok(document) if document.content_hash == mine.token => document.content,
                    Ok(_) => {
                        warnings.push(format!(
                            "{path} changed while syncing; it is taken next time"
                        ));
                        skipped += 1;
                        continue;
                    }
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
                    // This computer's text is confirmed kept before the
                    // document is touched.
                    match self
                        .keep_aside(caller, &held, path, &mine.hash, &existing)
                        .await
                    {
                        Some(aside) => {
                            warnings.push(format!(
                                "{path} changed on both computers; this computer's version is kept at {aside}"
                            ));
                            Step::Replace
                        }
                        None => {
                            warnings.push(format!(
                                "{path} changed on both computers and this computer's version could not be kept aside; left as it is here"
                            ));
                            Step::Keep
                        }
                    }
                };
            }
            if todo == Step::Keep {
                skipped += 1;
                continue;
            }
            // Only over the version the decision was made on: an edit that
            // landed meanwhile makes the write fail, and the next pass
            // decides again with that edit in view.
            let over = if todo == Step::Replace { mine } else { None };
            match self.put(caller, path, &content, over).await {
                Ok(true) => {
                    agree(path, &item.hash);
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
        if !agreed.is_empty() {
            self.base.record(caller, peer, &agreed).await?;
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
            conflict_path("notes/plan.md", "sha256:0123456789abcdef0123", 0).as_deref(),
            Some("sync-conflicts/0123456789ab/notes/plan.md")
        );
        assert_eq!(
            conflict_path("notes/plan.md", "sha256:0123456789abcdef0123", 1).as_deref(),
            Some("sync-conflicts/0123456789ab-2/notes/plan.md")
        );
        assert_eq!(
            conflict_path(&"x".repeat(MAX_MEMORY_PATH_BYTES), "sha256:ab", 0),
            None
        );
    }
}
