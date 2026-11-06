//! Assembly for the Memory page and the hooks view: the native provider's
//! document operations behind [`MemoryDocumentsSource`], and the hook
//! inventory behind [`HookCatalogSource`]. Composition only wires; the page's
//! behavior is `ironclaw_assistant::memory_page`, the document semantics are
//! `ironclaw_memory_native`'s.
//!
//! **Direct provider access, on purpose.** The model reaches memory through
//! the `ironclaw.memory.*` capabilities (authorized, approval-gated). The
//! Memory page is the authenticated user editing their *own* documents in
//! their *own* scope — the same posture as the user-profile reader, which also
//! builds the native service over the raw root filesystem. No grant, approval,
//! resource, or runtime obligation applies to that edit; what does apply is
//! kept: scope comes from the authenticated caller, writes run the provider's
//! prompt-write-safety policy and content-hash CAS, edits keep a version, and
//! "remove" archives instead of deleting.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_filesystem::RootFilesystem;
use ironclaw_host_api::ids::{CorrelationId, InvocationId};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_memory::{
    MemoryInvocation, MemoryServiceError, MemoryServiceErrorKind, MemoryServiceSearchRequest,
};
use ironclaw_memory_native::{NativeMemoryService, OwnerWriteOutcome};
use ironclaw_product_contracts::hooks_catalog::{HookCatalog, HookCatalogSource};
use ironclaw_product_contracts::memory_page::{
    MemoryArchiveOutcome, MemoryArchiveRequest, MemoryArchiveResponse, MemoryDocument,
    MemoryDocumentEntry, MemoryDocumentsSource, MemoryPageWriteOutcome, MemoryProvenance,
    MemorySearchHit, MemoryWriteRequest, MemoryWriteResponse, memory_document_kind,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceValidationCode,
};

/// What the Memory page decorator is built from; held on the runtime.
#[derive(Clone)]
pub(crate) struct MemoryPageParts {
    /// `None` unless the native (document-shaped) provider is bound.
    pub(crate) documents: Option<Arc<dyn MemoryDocumentsSource>>,
    pub(crate) supermemory: Arc<ironclaw_supermemory::Supermemory>,
    pub(crate) hooks: Arc<dyn HookCatalogSource>,
}

/// The native provider's documents for the authenticated caller.
pub(crate) struct NativeMemoryDocuments {
    service: NativeMemoryService,
}

impl NativeMemoryDocuments {
    /// `prompt_safety_sink` is the redacted audit-backed sink the memory
    /// tools use; with it, page writes to protected files record the same
    /// safety events (and warned writes, which require one, go through).
    pub(crate) fn new(
        filesystem: Arc<dyn RootFilesystem>,
        prompt_safety_sink: Option<Arc<dyn ironclaw_memory::PromptWriteSafetyEventSink>>,
    ) -> Self {
        Self {
            service: NativeMemoryService::from_filesystem(filesystem, prompt_safety_sink),
        }
    }

    fn invocation(caller: &ProductSurfaceCaller) -> MemoryInvocation {
        MemoryInvocation {
            scope: ResourceScope {
                tenant_id: caller.tenant_id.clone(),
                user_id: caller.user_id.clone(),
                agent_id: caller.agent_id.clone(),
                project_id: caller.project_id.clone(),
                mission_id: None,
                thread_id: None,
                invocation_id: InvocationId::new(),
            },
            correlation_id: CorrelationId::new(),
        }
    }

    async fn document(
        &self,
        caller: &ProductSurfaceCaller,
        path: &str,
    ) -> Result<Option<MemoryDocument>, ProductSurfaceError> {
        // The owner's read: archived documents are readable on the page.
        let read = match self
            .service
            .owner_read(Self::invocation(caller), path)
            .await
        {
            Ok(read) => read,
            // The native read reports a missing document as an input error.
            Err(error) if error.kind() == MemoryServiceErrorKind::Input => return Ok(None),
            Err(error) => return Err(map_memory_error(error)),
        };
        let history = self
            .service
            .history(Self::invocation(caller), path)
            .await
            .map_err(map_memory_error)?;
        Ok(Some(MemoryDocument {
            kind: memory_document_kind(&read.path),
            path: read.path,
            content: read.content,
            content_hash: read.content_hash,
            word_count: read.word_count,
            provenance: MemoryProvenance {
                version_count: history.version_count,
                last_changed_by: history.last_changed_by,
            },
        }))
    }
}

fn map_memory_error(error: MemoryServiceError) -> ProductSurfaceError {
    match error.kind() {
        MemoryServiceErrorKind::Input => {
            tracing::debug!(%error, "memory page: rejected input");
            ProductSurfaceError::validation("path", ProductSurfaceValidationCode::InvalidValue)
        }
        MemoryServiceErrorKind::Unavailable => {
            tracing::debug!(%error, "memory page: provider unavailable");
            ProductSurfaceError::unavailable(true)
        }
        MemoryServiceErrorKind::Operation => ProductSurfaceError::internal_from(error),
    }
}

#[async_trait]
impl MemoryDocumentsSource for NativeMemoryDocuments {
    async fn tree(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<Vec<MemoryDocumentEntry>, ProductSurfaceError> {
        let paths = self
            .service
            .list_paths(Self::invocation(caller))
            .await
            .map_err(map_memory_error)?;
        Ok(paths
            .into_iter()
            .map(|path| MemoryDocumentEntry {
                kind: memory_document_kind(&path),
                path,
            })
            .collect())
    }

    async fn read(
        &self,
        caller: &ProductSurfaceCaller,
        path: &str,
    ) -> Result<MemoryDocument, ProductSurfaceError> {
        self.document(caller, path)
            .await?
            .ok_or_else(ProductSurfaceError::not_found)
    }

    async fn search(
        &self,
        caller: &ProductSurfaceCaller,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>, ProductSurfaceError> {
        let response = self
            .service
            .search(
                Self::invocation(caller),
                MemoryServiceSearchRequest {
                    query: query.to_string(),
                    limit,
                },
            )
            .await
            .map_err(map_memory_error)?;
        Ok(response
            .results
            .into_iter()
            .map(|hit| MemorySearchHit {
                path: hit.path,
                snippet: hit.content,
                score: hit.score,
            })
            .collect())
    }

    async fn write(
        &self,
        caller: &ProductSurfaceCaller,
        request: MemoryWriteRequest,
    ) -> Result<MemoryWriteResponse, ProductSurfaceError> {
        // Literal path, conditional on the hash the editor read: `None` is
        // an atomic absent-CAS create that never overwrites, and the agent
        // tool's `memory`/`daily_log` aliases are not resolved here.
        let outcome = self
            .service
            .owner_write(
                Self::invocation(caller),
                &request.path,
                &request.content,
                request.expected_content_hash.as_deref(),
            )
            .await
            .map_err(map_memory_error)?;
        let outcome = match outcome {
            OwnerWriteOutcome::Written => MemoryPageWriteOutcome::Written,
            OwnerWriteOutcome::Conflict => MemoryPageWriteOutcome::Conflict,
            OwnerWriteOutcome::RejectedBySafety => {
                return Err(ProductSurfaceError::validation(
                    "content",
                    ProductSurfaceValidationCode::InvalidValue,
                ));
            }
        };
        // Read back: the saved document (or the current one on a conflict)
        // is the evidence the editor shows.
        Ok(MemoryWriteResponse {
            outcome,
            document: self.document(caller, &request.path).await?,
        })
    }

    async fn archive(
        &self,
        caller: &ProductSurfaceCaller,
        request: MemoryArchiveRequest,
    ) -> Result<MemoryArchiveResponse, ProductSurfaceError> {
        if self.document(caller, &request.path).await?.is_none() {
            return Err(ProductSurfaceError::not_found());
        }
        let archived = self
            .service
            .archive(
                Self::invocation(caller),
                &request.path,
                &request.expected_content_hash,
            )
            .await
            .map_err(map_memory_error)?;
        // Read back: the archive copy must exist before success is reported.
        if let Some(path) = &archived
            && self.document(caller, path).await?.is_none()
        {
            return Err(ProductSurfaceError::internal_from(
                "memory archive copy missing after archive",
            ));
        }
        Ok(MemoryArchiveResponse {
            outcome: if archived.is_some() {
                MemoryArchiveOutcome::Archived
            } else {
                MemoryArchiveOutcome::Conflict
            },
            archived_path: archived,
        })
    }
}

/// A catalog computed once at build: hooks are fixed for a runtime's life.
pub(crate) struct StaticHookCatalog(pub(crate) HookCatalog);

#[async_trait]
impl HookCatalogSource for StaticHookCatalog {
    async fn catalog(&self) -> HookCatalog {
        self.0.clone()
    }
}

#[cfg(test)]
#[path = "memory_page_parts_tests.rs"]
mod tests;
