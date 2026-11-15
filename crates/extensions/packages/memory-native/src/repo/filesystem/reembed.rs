//! Storage half of the re-embed backfill (`crate::reembed`): which chunks
//! have no vector in a given embedding space, attaching one to a chunk in
//! place, and the durable per-space progress cursor.
//!
//! Attaching a vector never removes anything: the chunk keeps its content,
//! its other indexed keys, and every vector it holds under another space's
//! key. Each attach is a version-checked put, so a chunk the indexer replaced
//! meanwhile (the document was rewritten) is left alone.

use std::collections::BTreeMap;

use ironclaw_filesystem::RecordVersion;
use serde::{Deserialize, Serialize};

use super::*;
use crate::embedding::{MemoryEmbedder, MemoryEmbeddingFingerprint, embeddable};

const CURSOR_KIND: &str = "memory_reembed_cursor";
/// Re-embed cursors live beside the per-tenant trees, never inside a user's
/// memory, so no document listing or search can see them.
const CURSOR_ROOT: &str = "/memory/.reembed";
const TENANTS_ROOT: &str = "/memory/tenants";
/// The virtual-path sentinel for an absent agent/project segment.
const ABSENT_SEGMENT: &str = "_none";
/// Directory children read per page while enumerating scopes.
const SCOPE_PAGE: usize = 256;

/// One chunk that has no vector in the space being backfilled.
#[derive(Debug, Clone)]
pub(crate) struct PendingChunk {
    pub(crate) content: String,
    versioned: ironclaw_filesystem::VersionedEntry,
}

/// The pending chunks of one document, in chunk order.
#[derive(Debug, Clone)]
pub(crate) struct DocumentBacklog {
    pub(crate) relative_path: String,
    pub(crate) pending: Vec<PendingChunk>,
}

/// One scope's backlog: documents in path order, plus chunks that can never
/// be embedded (blank, or over the embedder's per-input limit).
#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeBacklog {
    pub(crate) documents: Vec<DocumentBacklog>,
    pub(crate) ineligible: u64,
}

/// Durable progress of one embedding space's backfill pass. `position` is the
/// last `(scope, document)` whose pending chunks the pass has dealt with; a
/// resumed pass skips everything at or before it. `None` = start of a pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReembedCursor {
    pub(crate) fingerprint: String,
    pub(crate) position: Option<ReembedPosition>,
    /// Passes that reached the end of the tree.
    pub(crate) completed_passes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct ReembedPosition {
    /// `[tenant, user, agent, project]` path segments (`_none` = absent).
    pub(crate) scope: [String; 4],
    pub(crate) document: String,
}

/// The ordering key of a scope, matching [`ReembedPosition::scope`].
pub(crate) fn scope_key(scope: &MemoryDocumentScope) -> [String; 4] {
    [
        scope.tenant_id().to_string(),
        scope.user_id().to_string(),
        scope.agent_id().unwrap_or(ABSENT_SEGMENT).to_string(),
        scope.project_id().unwrap_or(ABSENT_SEGMENT).to_string(),
    ]
}

fn present_segment(segment: &str) -> Option<&str> {
    (segment != ABSENT_SEGMENT).then_some(segment)
}

fn reembed_path(
    raw: String,
    operation: FilesystemOperation,
) -> Result<VirtualPath, FilesystemError> {
    VirtualPath::new(raw)
        .map_err(|error| memory_error(valid_memory_path(), operation, error.to_string()))
}

impl<F> FilesystemMemoryDocumentRepository<F>
where
    F: RootFilesystem + ?Sized + 'static,
{
    /// Names of the directories directly under `path`, in name order; an
    /// absent directory has none.
    async fn child_names(&self, path: &VirtualPath) -> Result<Vec<String>, FilesystemError> {
        let mut names = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = match self
                .filesystem
                .list_dir_page(path, after.as_deref(), SCOPE_PAGE)
                .await
            {
                Ok(page) => page,
                Err(FilesystemError::NotFound { .. }) => break,
                Err(error) => return Err(error),
            };
            let full = page.len() >= SCOPE_PAGE;
            after = page.last().map(|entry| entry.name.clone());
            names.extend(
                page.into_iter()
                    .filter(|entry| entry.file_type == ironclaw_filesystem::FileType::Directory)
                    .map(|entry| entry.name),
            );
            if !full {
                break;
            }
        }
        Ok(names)
    }

    /// Every memory scope that holds anything, in `scope_key` order.
    pub(crate) async fn memory_scopes(&self) -> Result<Vec<MemoryDocumentScope>, FilesystemError> {
        let op = FilesystemOperation::ListDir;
        let mut scopes = Vec::new();
        for tenant in self
            .child_names(&reembed_path(TENANTS_ROOT.into(), op)?)
            .await?
        {
            let users = reembed_path(format!("{TENANTS_ROOT}/{tenant}/users"), op)?;
            for user in self.child_names(&users).await? {
                let agents = reembed_path(format!("{}/{user}/agents", users.as_str()), op)?;
                for agent in self.child_names(&agents).await? {
                    let projects =
                        reembed_path(format!("{}/{agent}/projects", agents.as_str()), op)?;
                    for project in self.child_names(&projects).await? {
                        // A directory whose names are not a valid scope holds
                        // no memory this provider wrote; skip it.
                        if let Ok(scope) = MemoryDocumentScope::new_with_agent(
                            tenant.clone(),
                            user.clone(),
                            present_segment(&agent),
                            present_segment(&project),
                        ) {
                            scopes.push(scope);
                        }
                    }
                }
            }
        }
        scopes.sort_by_key(scope_key);
        Ok(scopes)
    }

    /// The chunks under `scope` with no vector in `embedder`'s space, grouped
    /// by document in path order. One query over the scope, like
    /// `list_documents`.
    pub(crate) async fn scope_backlog(
        &self,
        scope: &MemoryDocumentScope,
        embedder: &dyn MemoryEmbedder,
    ) -> Result<ScopeBacklog, FilesystemError> {
        let prefix = scope.virtual_prefix().map_err(|error| {
            memory_error(
                valid_memory_path(),
                FilesystemOperation::Query,
                error.to_string(),
            )
        })?;
        let vector_key = IndexKey::new(embedder.fingerprint().vector_key()).map_err(|error| {
            memory_error(
                valid_memory_path(),
                FilesystemOperation::Query,
                error.to_string(),
            )
        })?;
        let mut by_document = BTreeMap::<String, Vec<(i64, PendingChunk)>>::new();
        let mut ineligible = 0_u64;
        for versioned in self.query_all(&prefix, &Filter::All).await? {
            if versioned
                .entry
                .kind
                .as_ref()
                .is_none_or(|kind| kind.as_str() != CHUNK_KIND)
                || versioned.entry.indexed.contains_key(&vector_key)
            {
                continue;
            }
            let Some(document) = Self::document_relative_path_from_chunk_entry(&versioned) else {
                continue;
            };
            let content = match String::from_utf8(versioned.entry.body.clone()) {
                Ok(content) if embeddable(embedder, &content) => content,
                _ => {
                    ineligible += 1;
                    continue;
                }
            };
            let index = match versioned
                .entry
                .indexed
                .get(&Self::index_key(fs_keys::CHUNK_INDEX))
            {
                Some(IndexValue::I64(index)) => *index,
                _ => i64::MAX,
            };
            by_document
                .entry(document)
                .or_default()
                .push((index, PendingChunk { content, versioned }));
        }
        let documents = by_document
            .into_iter()
            .map(|(relative_path, mut chunks)| {
                chunks.sort_by_key(|(index, _)| *index);
                DocumentBacklog {
                    relative_path,
                    pending: chunks.into_iter().map(|(_, chunk)| chunk).collect(),
                }
            })
            .collect();
        Ok(ScopeBacklog {
            documents,
            ineligible,
        })
    }

    /// Add `vector` (in `fingerprint`'s space) to `chunk`, keeping everything
    /// it already holds. `false` when the chunk changed since it was read (a
    /// reindex replaced or removed it): nothing is written.
    pub(crate) async fn attach_chunk_vector(
        &self,
        chunk: &PendingChunk,
        fingerprint: &MemoryEmbeddingFingerprint,
        vector: &[f32],
    ) -> Result<bool, FilesystemError> {
        let vector_key = IndexKey::new(fingerprint.vector_key()).map_err(|error| {
            memory_error(
                valid_memory_path(),
                FilesystemOperation::WriteFile,
                error.to_string(),
            )
        })?;
        let entry = chunk
            .versioned
            .entry
            .clone()
            .with_indexed(
                Self::index_key(fs_keys::EMBEDDING_FINGERPRINT),
                IndexValue::Text(fingerprint.as_str().to_string()),
            )
            .with_indexed(vector_key, IndexValue::Bytes(encode_embedding_blob(vector)));
        match self
            .filesystem
            .put(
                &chunk.versioned.path,
                entry,
                CasExpectation::Version(chunk.versioned.version),
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(FilesystemError::VersionMismatch { .. } | FilesystemError::NotFound { .. }) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn cursor_path(
        fingerprint: &MemoryEmbeddingFingerprint,
    ) -> Result<VirtualPath, FilesystemError> {
        reembed_path(
            format!("{CURSOR_ROOT}/{}", fingerprint.vector_key()),
            FilesystemOperation::ReadFile,
        )
    }

    /// The space's durable cursor and its version, `None` before the first
    /// pass.
    pub(crate) async fn read_reembed_cursor(
        &self,
        fingerprint: &MemoryEmbeddingFingerprint,
    ) -> Result<Option<(ReembedCursor, RecordVersion)>, FilesystemError> {
        let path = Self::cursor_path(fingerprint)?;
        let Some(versioned) = self.filesystem.get(&path).await? else {
            return Ok(None);
        };
        let cursor: ReembedCursor =
            serde_json::from_slice(&versioned.entry.body).map_err(|error| {
                memory_error(
                    path.clone(),
                    FilesystemOperation::ReadFile,
                    format!("memory re-embed cursor is unreadable: {error}"),
                )
            })?;
        Ok(Some((cursor, versioned.version)))
    }

    /// Compare-and-swap the space's cursor: `expected` is the version last
    /// read (`None` = must not exist yet). Returns the new version, or `None`
    /// on a conflict (another run moved it first).
    pub(crate) async fn write_reembed_cursor(
        &self,
        fingerprint: &MemoryEmbeddingFingerprint,
        cursor: &ReembedCursor,
        expected: Option<RecordVersion>,
    ) -> Result<Option<RecordVersion>, FilesystemError> {
        let path = Self::cursor_path(fingerprint)?;
        let value = serde_json::to_value(cursor).map_err(|error| {
            memory_error(
                path.clone(),
                FilesystemOperation::WriteFile,
                error.to_string(),
            )
        })?;
        let kind = RecordKind::new(CURSOR_KIND).map_err(|error| {
            memory_error(
                path.clone(),
                FilesystemOperation::WriteFile,
                error.to_string(),
            )
        })?;
        let entry = Entry::record(kind, &value).map_err(|error| {
            memory_error(
                path.clone(),
                FilesystemOperation::WriteFile,
                error.to_string(),
            )
        })?;
        let expectation = match expected {
            Some(version) => CasExpectation::Version(version),
            None => CasExpectation::Absent,
        };
        match self.filesystem.put(&path, entry, expectation).await {
            Ok(version) => Ok(Some(version)),
            Err(FilesystemError::VersionMismatch { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
