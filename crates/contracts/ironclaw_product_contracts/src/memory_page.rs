//! Memory page descriptors, wire DTOs and the local-documents port.
//!
//! The Memory page shows two clearly separate stores:
//!
//! - **Local** memory documents (the bound memory provider's `/memory`
//!   documents: `MEMORY.md`, daily logs, notes, profile files). Browse, read,
//!   search, edit (conditional on the content hash the read returned) and
//!   archive. Archive moves a document into the provider's own `archive/`
//!   tree, unindexed; nothing is deleted.
//! - **Supermemory** (remote, the user's own account): which container the
//!   chosen profile is bound to, the documents captured there, semantic search,
//!   and deleting a wrong document or forgetting a wrong extracted memory.
//!
//! `ironclaw_assistant::memory_page` answers these descriptors; composition
//! implements [`MemoryDocumentsSource`] over the bound provider. Nothing here
//! carries the Supermemory API key or a host path.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};
use crate::surface::{ProductSurfaceCaller, ProductSurfaceError};

/// Longest document path a client may name, in bytes.
pub const MAX_MEMORY_PATH_BYTES: usize = 512;
/// Largest document body the editor may save, in bytes.
pub const MAX_MEMORY_DOCUMENT_BYTES: usize = 512 * 1024;
/// Longest search query, in characters.
pub const MAX_MEMORY_QUERY_CHARS: usize = 500;
/// Most search hits one query returns.
pub const MAX_MEMORY_SEARCH_LIMIT: usize = 20;
/// Most Supermemory documents one page lists.
pub const MAX_SUPERMEMORY_PAGE_LIMIT: u32 = 100;

// --- local documents ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryTreeRequest {}

/// `GET /api/webchat/v2/memory/documents`.
pub const MEMORY_TREE_VIEW: ProductView<MemoryTreeRequest, MemoryTreeResponse> =
    ProductView::unpaginated("memory.documents.tree");

/// How the page should present a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDocumentKind {
    /// The curated standing memory (`MEMORY.md`), recalled every turn.
    Standing,
    /// Identity/system-prompt files (`USER.md`, `SOUL.md`, `AGENTS.md`, …).
    Identity,
    /// The structured user profile (`context/profile.json`).
    Profile,
    /// Daily logs (`daily/…`).
    DailyLog,
    /// Archived by the user; kept, unindexed, never recalled.
    Archived,
    /// Any other note.
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDocumentEntry {
    /// Path relative to the caller's memory root, e.g. `daily/2026-11-03.md`.
    pub path: String,
    pub kind: MemoryDocumentKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTreeResponse {
    /// `false` when the bound memory provider is not document-shaped (or no
    /// provider is bound); `documents` is then empty.
    pub available: bool,
    /// Sorted by path.
    pub documents: Vec<MemoryDocumentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDocumentRequest {
    pub path: String,
}

/// `GET /api/webchat/v2/memory/documents/content?path=…`.
pub const MEMORY_DOCUMENT_VIEW: ProductView<MemoryDocumentRequest, MemoryDocument> =
    ProductView::unpaginated("memory.documents.read");

/// Who last changed a document, as far as the provider recorded it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    /// Saved earlier versions of this document (each edit keeps one).
    pub version_count: u64,
    /// The provider's writer key for the latest saved version (tenant/user/
    /// agent scope). The native provider does not record the thread or turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changed_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDocument {
    pub path: String,
    pub kind: MemoryDocumentKind,
    pub content: String,
    /// SHA-256 of `content`; send it back as `expected_content_hash` to save.
    pub content_hash: String,
    pub word_count: usize,
    pub provenance: MemoryProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchRequest {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `GET /api/webchat/v2/memory/search?q=…`.
pub const MEMORY_SEARCH_VIEW: ProductView<MemorySearchRequest, MemorySearchResponse> =
    ProductView::unpaginated("memory.documents.search");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchHit {
    pub path: String,
    pub snippet: String,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchResponse {
    pub query: String,
    pub hits: Vec<MemorySearchHit>,
}

/// Full-document save from the editor. `expected_content_hash` is the hash
/// the read returned; `None` creates a document that must not exist yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteRequest {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_content_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteOutcome {
    /// Saved; `document` is the read-back.
    Written,
    /// The document changed since it was read (or exists, for a create);
    /// nothing was overwritten. `document` is the current version when it
    /// exists.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryWriteResponse {
    pub outcome: MemoryWriteOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<MemoryDocument>,
}

pub const MEMORY_WRITE_COMMAND_ID: &str = "memory.documents.write";
/// `POST /api/webchat/v2/memory/documents`.
pub const MEMORY_WRITE_COMMAND: ProductSurfaceCommandDescriptor<
    MemoryWriteRequest,
    MemoryWriteResponse,
> = ProductSurfaceCommandDescriptor::new(MEMORY_WRITE_COMMAND_ID);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryArchiveRequest {
    pub path: String,
    /// The hash the read returned; archive refuses when the document changed.
    pub expected_content_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryArchiveOutcome {
    Archived,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryArchiveResponse {
    pub outcome: MemoryArchiveOutcome,
    /// Where the document now lives, e.g. `archive/20261103T101500Z/notes.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_path: Option<String>,
}

pub const MEMORY_ARCHIVE_COMMAND_ID: &str = "memory.documents.archive";
/// `POST /api/webchat/v2/memory/documents/archive`.
pub const MEMORY_ARCHIVE_COMMAND: ProductSurfaceCommandDescriptor<
    MemoryArchiveRequest,
    MemoryArchiveResponse,
> = ProductSurfaceCommandDescriptor::new(MEMORY_ARCHIVE_COMMAND_ID);

/// The caller's local memory documents, served by the bound provider.
///
/// Requests reaching an implementation are already bounded (path length,
/// body size, query length) by the product decorator; implementations still
/// validate paths against their own grammar and return
/// `ProductSurfaceError` validation/not-found codes for correctable input.
#[async_trait]
pub trait MemoryDocumentsSource: Send + Sync {
    async fn tree(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<Vec<MemoryDocumentEntry>, ProductSurfaceError>;

    async fn read(
        &self,
        caller: &ProductSurfaceCaller,
        path: &str,
    ) -> Result<MemoryDocument, ProductSurfaceError>;

    async fn search(
        &self,
        caller: &ProductSurfaceCaller,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>, ProductSurfaceError>;

    async fn write(
        &self,
        caller: &ProductSurfaceCaller,
        request: MemoryWriteRequest,
    ) -> Result<MemoryWriteResponse, ProductSurfaceError>;

    async fn archive(
        &self,
        caller: &ProductSurfaceCaller,
        request: MemoryArchiveRequest,
    ) -> Result<MemoryArchiveResponse, ProductSurfaceError>;
}

/// Presentation class for a memory path. Shared by every implementation so
/// the page groups documents the same way whichever provider serves them.
pub fn memory_document_kind(path: &str) -> MemoryDocumentKind {
    let path = path.trim_matches('/');
    let name = path.rsplit('/').next().unwrap_or(path);
    if path.starts_with("archive/") {
        MemoryDocumentKind::Archived
    } else if path == "MEMORY.md" {
        MemoryDocumentKind::Standing
    } else if path == "context/profile.json" {
        MemoryDocumentKind::Profile
    } else if path.starts_with("daily/") {
        MemoryDocumentKind::DailyLog
    } else if !path.contains('/')
        && matches!(
            name,
            "USER.md"
                | "SOUL.md"
                | "AGENTS.md"
                | "IDENTITY.md"
                | "TOOLS.md"
                | "HEARTBEAT.md"
                | "BOOTSTRAP.md"
        )
    {
        MemoryDocumentKind::Identity
    } else {
        MemoryDocumentKind::Note
    }
}

// --- Supermemory (remote) ---

/// Selects whose Supermemory binding to use: a profile's, or the default
/// (environment) binding when `profile_id` is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupermemoryStatusRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// `GET /api/webchat/v2/memory/supermemory?profile_id=…`.
pub const SUPERMEMORY_STATUS_VIEW: ProductView<SupermemoryStatusRequest, SupermemoryStatus> =
    ProductView::unpaginated("memory.supermemory.status");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupermemoryBindingOrigin {
    /// The profile's own `supermemory` settings.
    Profile,
    /// The deployment default (`SUPERMEMORY_CONTAINER_TAG`).
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryBindingInfo {
    /// Recalled every turn and receives every write.
    pub primary_container: String,
    /// Searchable, never written.
    pub read_containers: Vec<String>,
    pub origin: SupermemoryBindingOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryStatus {
    /// Supermemory is compiled in and wired for this deployment.
    pub available: bool,
    /// An API key is stored. The key itself never leaves the host.
    pub key_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<SupermemoryBindingInfo>,
    /// Supermemory replaces local memory for threads on this profile.
    pub active: bool,
    pub auto_recall: bool,
    pub auto_capture: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupermemoryDocumentsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// One of the binding's containers; the primary when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// 1-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `GET /api/webchat/v2/memory/supermemory/documents`.
pub const SUPERMEMORY_DOCUMENTS_VIEW: ProductView<
    SupermemoryDocumentsRequest,
    SupermemoryDocumentPage,
> = ProductView::unpaginated("memory.supermemory.documents");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryDocumentSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The provider's summary, or the start of the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub container_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryDocumentPage {
    pub container: String,
    pub documents: Vec<SupermemoryDocumentSummary>,
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_items: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupermemorySearchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `GET /api/webchat/v2/memory/supermemory/search?q=…`.
pub const SUPERMEMORY_SEARCH_VIEW: ProductView<
    SupermemorySearchRequest,
    SupermemorySearchResponse,
> = ProductView::unpaginated("memory.supermemory.search");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupermemoryMemoryHit {
    /// Memory id; pass it to forget.
    pub id: String,
    pub container: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupermemorySearchResponse {
    pub query: String,
    pub hits: Vec<SupermemoryMemoryHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupermemoryDeleteDocumentRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryDeleteDocumentResponse {
    pub id: String,
    /// A follow-up read by id no longer finds the document.
    pub verified: bool,
}

pub const SUPERMEMORY_DELETE_DOCUMENT_COMMAND_ID: &str = "memory.supermemory.delete_document";
/// `POST /api/webchat/v2/memory/supermemory/documents/delete`.
pub const SUPERMEMORY_DELETE_DOCUMENT_COMMAND: ProductSurfaceCommandDescriptor<
    SupermemoryDeleteDocumentRequest,
    SupermemoryDeleteDocumentResponse,
> = ProductSurfaceCommandDescriptor::new(SUPERMEMORY_DELETE_DOCUMENT_COMMAND_ID);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupermemoryForgetRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub container: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryForgetResponse {
    pub id: String,
    /// The API confirmed the memory is marked forgotten (a soft delete on
    /// Supermemory's side).
    pub forgotten: bool,
}

pub const SUPERMEMORY_FORGET_COMMAND_ID: &str = "memory.supermemory.forget";
/// `POST /api/webchat/v2/memory/supermemory/memories/forget`.
pub const SUPERMEMORY_FORGET_COMMAND: ProductSurfaceCommandDescriptor<
    SupermemoryForgetRequest,
    SupermemoryForgetResponse,
> = ProductSurfaceCommandDescriptor::new(SUPERMEMORY_FORGET_COMMAND_ID);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_group_paths() {
        assert_eq!(
            memory_document_kind("MEMORY.md"),
            MemoryDocumentKind::Standing
        );
        assert_eq!(
            memory_document_kind("USER.md"),
            MemoryDocumentKind::Identity
        );
        assert_eq!(
            memory_document_kind("SOUL.md"),
            MemoryDocumentKind::Identity
        );
        assert_eq!(
            memory_document_kind("notes/USER.md"),
            MemoryDocumentKind::Note
        );
        assert_eq!(
            memory_document_kind("context/profile.json"),
            MemoryDocumentKind::Profile
        );
        assert_eq!(
            memory_document_kind("daily/2026-11-03.md"),
            MemoryDocumentKind::DailyLog
        );
        assert_eq!(
            memory_document_kind("archive/20261103T000000Z/MEMORY.md"),
            MemoryDocumentKind::Archived
        );
    }

    #[test]
    fn write_request_rejects_unknown_fields() {
        let error = serde_json::from_value::<MemoryWriteRequest>(serde_json::json!({
            "path": "a.md", "content": "x", "append": true
        }));
        assert!(error.is_err());
    }
}
