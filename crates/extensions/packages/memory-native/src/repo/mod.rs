//! Memory document repository trait and shared repo helpers.

use async_trait::async_trait;
use ironclaw_filesystem::{FilesystemError, FilesystemOperation};

use crate::metadata::MemoryWriteOptions;
use crate::path::{memory_backend_unsupported, memory_error, valid_memory_path};
use crate::search::{MemorySearchRequest, MemorySearchResult};
use ironclaw_memory::{MemoryDocumentPath, MemoryDocumentScope};

mod filesystem;
mod in_memory;

pub use filesystem::FilesystemMemoryDocumentRepository;
pub use in_memory::InMemoryMemoryDocumentRepository;

/// Result of an optimistic atomic append attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAppendOutcome {
    Appended,
    Conflict,
}

/// Saved-version summary for one document (its provenance trail).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryDocumentHistory {
    /// Earlier versions kept by overwrites.
    pub version_count: u64,
    /// Writer key recorded on the newest saved version.
    pub last_changed_by: Option<String>,
}

/// Result of an optimistic atomic document replacement attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryWriteOutcome {
    Written,
    Conflict,
}

/// Repository for file-shaped memory documents.
///
/// Implementations own the actual source of truth, such as the existing
/// `memory_documents` table. Search chunks should be updated by
/// the memory service/indexer, not by generic filesystem routing code.
#[async_trait]
pub trait MemoryDocumentRepository: Send + Sync {
    async fn read_document(
        &self,
        path: &MemoryDocumentPath,
    ) -> Result<Option<Vec<u8>>, FilesystemError>;

    async fn write_document(
        &self,
        path: &MemoryDocumentPath,
        bytes: &[u8],
    ) -> Result<(), FilesystemError>;

    async fn write_document_with_options(
        &self,
        path: &MemoryDocumentPath,
        bytes: &[u8],
        options: &MemoryWriteOptions,
    ) -> Result<(), FilesystemError> {
        let _ = options;
        self.write_document(path, bytes).await
    }

    async fn compare_and_append_document_with_options(
        &self,
        path: &MemoryDocumentPath,
        expected_previous_hash: Option<&str>,
        bytes: &[u8],
        options: &MemoryWriteOptions,
    ) -> Result<MemoryAppendOutcome, FilesystemError> {
        let _ = (expected_previous_hash, bytes, options);
        Err(memory_error(
            path.virtual_path().unwrap_or_else(|_| valid_memory_path()),
            FilesystemOperation::AppendFile,
            "memory document repository does not support atomic append",
        ))
    }

    async fn compare_and_write_document_with_options(
        &self,
        path: &MemoryDocumentPath,
        expected_previous_hash: Option<&str>,
        bytes: &[u8],
        options: &MemoryWriteOptions,
    ) -> Result<MemoryWriteOutcome, FilesystemError> {
        let _ = (expected_previous_hash, bytes, options);
        Err(memory_error(
            path.virtual_path().unwrap_or_else(|_| valid_memory_path()),
            FilesystemOperation::WriteFile,
            "memory document repository does not support atomic write",
        ))
    }

    async fn read_document_metadata(
        &self,
        path: &MemoryDocumentPath,
    ) -> Result<Option<serde_json::Value>, FilesystemError> {
        let _ = path;
        Ok(None)
    }

    /// Persist new metadata for `path`.
    ///
    /// Implementations are expected to invalidate stale chunk rows when
    /// the metadata change makes the document's existing index invalid.
    /// In particular, both native repositories CLEAR chunk rows when
    /// the new metadata sets `skip_indexing=true` (for `.config` paths,
    /// the clear cascades to descendants whose own metadata does not
    /// override the inherited skip).
    ///
    /// **Contract limitation**: this method only invalidates the
    /// already-stored *index*; it cannot re-create chunks. If a caller
    /// flips `skip_indexing` from `true` back to `false`, the document
    /// stays unindexed until the caller drives a reindex itself
    /// (e.g. by re-writing the document body, or by calling the
    /// indexer directly). The repo has no access to chunkers and
    /// therefore cannot do this on its own
    /// (zmanian #3180 MED `native_libsql.rs:491`).
    async fn write_document_metadata(
        &self,
        path: &MemoryDocumentPath,
        metadata: &serde_json::Value,
    ) -> Result<(), FilesystemError> {
        let _ = (path, metadata);
        Ok(())
    }

    async fn list_documents(
        &self,
        scope: &MemoryDocumentScope,
    ) -> Result<Vec<MemoryDocumentPath>, FilesystemError>;

    async fn search_documents(
        &self,
        scope: &MemoryDocumentScope,
        request: &MemorySearchRequest,
    ) -> Result<Vec<MemorySearchResult>, FilesystemError> {
        let _ = request;
        Err(memory_backend_unsupported(
            scope,
            FilesystemOperation::ReadFile,
            "memory backend does not support search",
        ))
    }

    /// Take `path` out of the live tree, but only while its body still hashes
    /// to `expected_previous_hash`: `Written` when removed, `Conflict` when it
    /// changed (nothing removed). Callers keep the body elsewhere first (the
    /// service's archive copy); the version trail is left in place, so no
    /// earlier content is lost. Default: unsupported, fail closed.
    async fn remove_document_if_unchanged(
        &self,
        path: &MemoryDocumentPath,
        expected_previous_hash: &str,
    ) -> Result<MemoryWriteOutcome, FilesystemError> {
        let _ = expected_previous_hash;
        Err(memory_error(
            path.virtual_path().unwrap_or_else(|_| valid_memory_path()),
            FilesystemOperation::Delete,
            "memory document repository does not support conditional removal",
        ))
    }

    /// Saved-version summary for `path`. Default: no history recorded.
    async fn document_history(
        &self,
        path: &MemoryDocumentPath,
    ) -> Result<MemoryDocumentHistory, FilesystemError> {
        let _ = path;
        Ok(MemoryDocumentHistory::default())
    }
}

pub(crate) fn scoped_memory_owner_key(scope: &MemoryDocumentScope) -> String {
    format!(
        "tenant:{}:user:{}:project:{}",
        scope.tenant_id(),
        scope.user_id(),
        scope.project_id().unwrap_or("_none")
    )
}

pub(crate) fn scoped_memory_changed_by_key(scope: &MemoryDocumentScope) -> String {
    if let Some(agent_id) = scope.agent_id() {
        return format!(
            "tenant:{}:user:{}:agent:{}:project:{}",
            scope.tenant_id(),
            scope.user_id(),
            agent_id,
            scope.project_id().unwrap_or("_none")
        );
    }
    scoped_memory_owner_key(scope)
}

pub(crate) fn ensure_document_path_does_not_conflict(
    path: &MemoryDocumentPath,
    documents: &[MemoryDocumentPath],
    operation: FilesystemOperation,
) -> Result<(), FilesystemError> {
    let relative_path = path.relative_path();
    let descendant_prefix = format!("{relative_path}/");
    if documents
        .iter()
        .any(|document| document.relative_path().starts_with(&descendant_prefix))
    {
        return Err(memory_error(
            path.virtual_path().unwrap_or_else(|_| valid_memory_path()),
            operation,
            "memory document path conflicts with an existing directory",
        ));
    }

    let segments: Vec<&str> = relative_path.split('/').collect();
    for end in 1..segments.len() {
        let ancestor = segments[..end].join("/");
        if documents
            .iter()
            .any(|document| document.relative_path() == ancestor)
        {
            return Err(memory_error(
                path.virtual_path().unwrap_or_else(|_| valid_memory_path()),
                operation,
                "memory document path conflicts with an existing file ancestor",
            ));
        }
    }

    Ok(())
}
