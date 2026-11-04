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
    MemoryInvocation, MemoryServiceError, MemoryServiceErrorKind, MemoryServiceReadRequest,
    MemoryServiceSearchRequest, MemoryServiceWriteRequest, MemoryWriteStatus,
};
use ironclaw_memory_native::NativeMemoryService;
use ironclaw_product_contracts::hooks_catalog::{HookCatalog, HookCatalogSource};
use ironclaw_product_contracts::memory_page::{
    MemoryArchiveOutcome, MemoryArchiveRequest, MemoryArchiveResponse, MemoryDocument,
    MemoryDocumentEntry, MemoryDocumentsSource, MemoryProvenance, MemorySearchHit,
    MemoryWriteOutcome, MemoryWriteRequest, MemoryWriteResponse, memory_document_kind,
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
    pub(crate) fn new(filesystem: Arc<dyn RootFilesystem>) -> Self {
        Self {
            service: NativeMemoryService::from_filesystem(filesystem, None),
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
        let read = match self
            .service
            .read(
                Self::invocation(caller),
                MemoryServiceReadRequest {
                    path: path.to_string(),
                },
            )
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
        // A create must not overwrite: the native write is conditional only
        // when given a hash, so check absence first and report a conflict.
        if request.expected_content_hash.is_none()
            && let Some(existing) = self.document(caller, &request.path).await?
        {
            return Ok(MemoryWriteResponse {
                outcome: MemoryWriteOutcome::Conflict,
                document: Some(existing),
            });
        }
        let response = self
            .service
            .write(
                Self::invocation(caller),
                MemoryServiceWriteRequest {
                    target: request.path.clone(),
                    content: request.content,
                    append: false,
                    expected_content_hash: request.expected_content_hash,
                    old_string: None,
                    new_string: None,
                    replace_all: false,
                    metadata: None,
                    timezone: None,
                },
            )
            .await
            .map_err(map_memory_error)?;
        let outcome = match response.status {
            MemoryWriteStatus::Conflict => MemoryWriteOutcome::Conflict,
            _ => MemoryWriteOutcome::Written,
        };
        // Read back: the saved document (or the current one on a conflict)
        // is the evidence the editor shows.
        Ok(MemoryWriteResponse {
            outcome,
            document: self.document(caller, &response.path).await?,
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
