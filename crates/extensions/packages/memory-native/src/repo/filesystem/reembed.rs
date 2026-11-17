//! Storage half of the re-embed backfill (`crate::reembed`): which chunks
//! have no vector in a given embedding space, attaching one to a chunk in
//! place, and the durable per-space progress cursor.
//!
//! Attaching a vector never removes anything: the chunk keeps its content,
//! its other indexed keys, and every vector it holds under another space's
//! key. Each attach is a read-modify-write of the chunk's CURRENT record:
//! it re-reads the document and the chunk, and adds the one vector key only
//! while both are still what was embedded — the document's content hash
//! unchanged and the chunk's identity digest (body, indexed content,
//! document path, chunk index, scope projection) equal, then CASes on the
//! version it just read. Two windows, two guards: the identity check covers
//! a rewrite between the scan and the re-read; the CAS covers one between
//! the re-read and the put, because a recreated chunk's version is always
//! above the deleted one's (filesystem CONTRACT.md invariant 9), so the
//! stale version fails instead of matching the new chunk. The scanned
//! record is never written back.

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

/// Attempts of one attach when the chunk's version moves under it while its
/// identity still matches (a concurrent attach of another space's vector).
const ATTACH_ATTEMPTS: usize = 3;

/// One chunk that has no vector in the space being backfilled, with what it
/// was when scanned: its identity digest and its document's revision.
#[derive(Debug, Clone)]
pub(crate) struct PendingChunk {
    pub(crate) content: String,
    path: VirtualPath,
    identity: String,
    document: Arc<DocumentRevision>,
}

/// A document as scanned: where its body lives and the hash of that body.
#[derive(Debug)]
struct DocumentRevision {
    path: VirtualPath,
    content_hash: String,
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

/// Digest of everything that makes a chunk record *this* chunk: its kind,
/// body, and the identity-bearing indexed projections. Vectors, the space
/// label, and versions are deliberately not part of it.
fn chunk_identity(entry: &Entry) -> String {
    let field = |key: &str| match IndexKey::new(key)
        .ok()
        .and_then(|key| entry.indexed.get(&key))
    {
        Some(IndexValue::Text(text)) => format!("t:{text}"),
        Some(IndexValue::I64(number)) => format!("i:{number}"),
        Some(other) => format!("o:{other:?}"),
        None => "-".to_string(),
    };
    let mut canonical = Vec::new();
    let kind = entry.kind.as_ref().map_or("", |kind| kind.as_str());
    for part in [
        kind.to_string(),
        field(fs_keys::DOC_PATH),
        field(fs_keys::CHUNK_INDEX),
        field(fs_keys::CONTENT),
        field(fs_keys::TENANT),
        field(fs_keys::USER),
        field(fs_keys::AGENT),
        field(fs_keys::PROJECT),
    ] {
        canonical.extend_from_slice(&(part.len() as u64).to_le_bytes());
        canonical.extend_from_slice(part.as_bytes());
    }
    canonical.extend_from_slice(&(entry.body.len() as u64).to_le_bytes());
    canonical.extend_from_slice(&entry.body);
    content_bytes_sha256(&canonical)
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
        let entries = self.query_all(&prefix, &Filter::All).await?;
        // The revision each chunk was derived from: the document bodies in
        // this same scan.
        let body_prefix = format!("{}/", prefix.as_str().trim_end_matches('/'));
        let revisions: HashMap<String, Arc<DocumentRevision>> = entries
            .iter()
            .filter(|versioned| {
                versioned
                    .entry
                    .kind
                    .as_ref()
                    .is_some_and(|kind| kind.as_str() == DOCUMENT_KIND)
            })
            .filter_map(|versioned| {
                let relative = versioned.path.as_str().strip_prefix(&body_prefix)?;
                Some((
                    relative.to_string(),
                    Arc::new(DocumentRevision {
                        path: versioned.path.clone(),
                        content_hash: content_bytes_sha256(&versioned.entry.body),
                    }),
                ))
            })
            .collect();
        let mut by_document = BTreeMap::<String, Vec<(i64, PendingChunk)>>::new();
        let mut ineligible = 0_u64;
        for versioned in entries {
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
            // A chunk whose document is gone (an orphan) or whose text the
            // embedder cannot take is never pending: sending it would cost a
            // call on every run without ever completing.
            let revision = revisions.get(&document).cloned();
            let content = match (String::from_utf8(versioned.entry.body.clone()), revision) {
                (Ok(content), Some(revision)) if embeddable(embedder, &content) => {
                    (content, revision)
                }
                _ => {
                    ineligible += 1;
                    continue;
                }
            };
            let (content, revision) = content;
            let index = match versioned
                .entry
                .indexed
                .get(&Self::index_key(fs_keys::CHUNK_INDEX))
            {
                Some(IndexValue::I64(index)) => *index,
                _ => i64::MAX,
            };
            let identity = chunk_identity(&versioned.entry);
            by_document.entry(document).or_default().push((
                index,
                PendingChunk {
                    content,
                    path: versioned.path,
                    identity,
                    document: revision,
                },
            ));
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

    /// Add `vector` (in `fingerprint`'s space) to the chunk's current
    /// record, only while it is still the chunk that was embedded. `false`
    /// (nothing written) when the document's body changed, the chunk is gone
    /// or is a different chunk now, or it already holds a vector in this
    /// space.
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
        for _ in 0..ATTACH_ATTEMPTS {
            match self.filesystem.get(&chunk.document.path).await? {
                Some(document)
                    if content_bytes_sha256(&document.entry.body)
                        == chunk.document.content_hash => {}
                _ => return Ok(false),
            }
            let Some(current) = self.filesystem.get(&chunk.path).await? else {
                return Ok(false);
            };
            let is_chunk = current
                .entry
                .kind
                .as_ref()
                .is_some_and(|kind| kind.as_str() == CHUNK_KIND);
            if !is_chunk
                || chunk_identity(&current.entry) != chunk.identity
                || current.entry.indexed.contains_key(&vector_key)
            {
                return Ok(false);
            }
            let mut entry = current.entry;
            entry.indexed.insert(
                vector_key.clone(),
                IndexValue::Bytes(encode_embedding_blob(vector)),
            );
            match self
                .filesystem
                .put(&chunk.path, entry, CasExpectation::Version(current.version))
                .await
            {
                Ok(_) => return Ok(true),
                // Moved since the read: re-read and re-check identity.
                Err(FilesystemError::VersionMismatch { .. }) => continue,
                Err(FilesystemError::NotFound { .. }) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
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
