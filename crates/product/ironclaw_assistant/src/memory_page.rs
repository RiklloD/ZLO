//! `ProductSurface` decorator that answers the Memory page
//! (`ironclaw_product_contracts::memory_page`): local memory documents through
//! a [`MemoryDocumentsSource`], and the caller's Supermemory account through
//! [`Supermemory`]. Every other request passes through untouched.
//!
//! Input is bounded here, before any store or network call. Scope comes from
//! the authenticated caller only; a `profile_id` selects one of the caller's
//! own profiles (read through the orchestrator's profile store), never another
//! user's. The Supermemory key never leaves the service: views report only
//! whether one is stored.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::profiles::{ProfileOwner, parse_profile_slug};
use ironclaw_product_contracts::memory_page::*;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode,
    ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse, ProductSurfaceQueryPage,
    ProductSurfaceQueryRequest, ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
    ProductSurfaceValidationCode,
};
use ironclaw_supermemory::{Supermemory, SupermemoryBinding, SupermemoryCaller, SupermemoryError};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct MemoryPageSurface {
    inner: Arc<dyn ProductSurface>,
    documents: Option<Arc<dyn MemoryDocumentsSource>>,
    supermemory: Option<Arc<Supermemory>>,
    profiles: Option<Arc<Engine>>,
}

impl MemoryPageSurface {
    /// `documents` is `None` when the bound memory provider is not
    /// document-shaped; `supermemory` when the deployment has none; `profiles`
    /// when there is no orchestrator (then only the default binding exists).
    pub fn new(
        inner: Arc<dyn ProductSurface>,
        documents: Option<Arc<dyn MemoryDocumentsSource>>,
        supermemory: Option<Arc<Supermemory>>,
        profiles: Option<Arc<Engine>>,
    ) -> Self {
        Self {
            inner,
            documents,
            supermemory,
            profiles,
        }
    }

    fn documents(&self) -> Result<&Arc<dyn MemoryDocumentsSource>, ProductSurfaceError> {
        self.documents
            .as_ref()
            .ok_or_else(|| ProductSurfaceError::unavailable(false))
    }

    fn supermemory(&self) -> Result<&Arc<Supermemory>, ProductSurfaceError> {
        self.supermemory
            .as_ref()
            .ok_or_else(|| ProductSurfaceError::unavailable(false))
    }

    /// The profile's own Supermemory binding, `None` for the default. A
    /// profile without Supermemory settings also falls back to the default.
    async fn profile_binding(
        &self,
        caller: &ProductSurfaceCaller,
        profile_id: Option<&str>,
    ) -> Result<Option<SupermemoryBinding>, ProductSurfaceError> {
        let Some(raw) = profile_id.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return Ok(None);
        };
        let engine = self
            .profiles
            .as_ref()
            .ok_or_else(ProductSurfaceError::not_found)?;
        let id = parse_profile_slug(raw).map_err(|error| {
            tracing::debug!(%error, "memory page: invalid profile id");
            ProductSurfaceError::validation("profile_id", ProductSurfaceValidationCode::InvalidId)
        })?;
        let owner = ProfileOwner {
            tenant_id: caller.tenant_id.clone(),
            user_id: caller.user_id.clone(),
        };
        let record = engine
            .store()
            .profile(&owner, &id, false)
            .await
            .map_err(ProductSurfaceError::internal_from)?
            .ok_or_else(ProductSurfaceError::not_found)?;
        Ok(record
            .settings
            .supermemory
            .filter(|sm| !sm.container.trim().is_empty())
            .map(|sm| SupermemoryBinding {
                primary_container: sm.container,
                read_containers: sm.read_containers,
                instructions: sm.instructions,
            }))
    }

    async fn answer_view(
        &self,
        caller: &ProductSurfaceCaller,
        request: &ProductSurfaceQueryRequest,
    ) -> Option<Result<serde_json::Value, ProductSurfaceError>> {
        let id = request.view_id.as_str();
        let input = request.input.clone();
        Some(if id == MEMORY_TREE_VIEW.id {
            self.tree(caller).await.and_then(to_value)
        } else if id == MEMORY_DOCUMENT_VIEW.id {
            match parse::<MemoryDocumentRequest>(input) {
                Ok(params) => self.read(caller, params).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == MEMORY_SEARCH_VIEW.id {
            match parse::<MemorySearchRequest>(input) {
                Ok(params) => self.search(caller, params).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == SUPERMEMORY_STATUS_VIEW.id {
            match parse::<SupermemoryStatusRequest>(input) {
                Ok(params) => self.sm_status(caller, params).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == SUPERMEMORY_DOCUMENTS_VIEW.id {
            match parse::<SupermemoryDocumentsRequest>(input) {
                Ok(params) => self.sm_documents(caller, params).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == SUPERMEMORY_SEARCH_VIEW.id {
            match parse::<SupermemorySearchRequest>(input) {
                Ok(params) => self.sm_search(caller, params).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else {
            return None;
        })
    }

    async fn answer_command(
        &self,
        caller: &ProductSurfaceCaller,
        request: &ProductSurfaceInvokeRequest,
    ) -> Option<Result<serde_json::Value, ProductSurfaceError>> {
        let id = request.operation_id.as_str();
        let input = request.input.clone();
        Some(if id == MEMORY_WRITE_COMMAND_ID {
            match parse::<MemoryWriteRequest>(input) {
                Ok(body) => self.write(caller, body).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == MEMORY_ARCHIVE_COMMAND_ID {
            match parse::<MemoryArchiveRequest>(input) {
                Ok(body) => self.archive(caller, body).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == SUPERMEMORY_DELETE_DOCUMENT_COMMAND_ID {
            match parse::<SupermemoryDeleteDocumentRequest>(input) {
                Ok(body) => self.sm_delete(caller, body).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else if id == SUPERMEMORY_FORGET_COMMAND_ID {
            match parse::<SupermemoryForgetRequest>(input) {
                Ok(body) => self.sm_forget(caller, body).await.and_then(to_value),
                Err(error) => Err(error),
            }
        } else {
            return None;
        })
    }

    // --- local documents ---

    async fn tree(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<MemoryTreeResponse, ProductSurfaceError> {
        let Some(documents) = self.documents.as_ref() else {
            return Ok(MemoryTreeResponse {
                available: false,
                documents: Vec::new(),
            });
        };
        let mut documents = documents.tree(caller).await?;
        documents.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(MemoryTreeResponse {
            available: true,
            documents,
        })
    }

    async fn read(
        &self,
        caller: &ProductSurfaceCaller,
        params: MemoryDocumentRequest,
    ) -> Result<MemoryDocument, ProductSurfaceError> {
        let path = validate_path(&params.path)?;
        self.documents()?.read(caller, &path).await
    }

    async fn search(
        &self,
        caller: &ProductSurfaceCaller,
        params: MemorySearchRequest,
    ) -> Result<MemorySearchResponse, ProductSurfaceError> {
        let query = validate_query(&params.query)?;
        let limit = params.limit.unwrap_or(10).clamp(1, MAX_MEMORY_SEARCH_LIMIT);
        let hits = self.documents()?.search(caller, &query, limit).await?;
        Ok(MemorySearchResponse { query, hits })
    }

    async fn write(
        &self,
        caller: &ProductSurfaceCaller,
        mut body: MemoryWriteRequest,
    ) -> Result<MemoryWriteResponse, ProductSurfaceError> {
        body.path = validate_path(&body.path)?;
        if body.path.starts_with("archive/") {
            // Archived documents are kept as they were removed.
            return Err(ProductSurfaceError::validation(
                "path",
                ProductSurfaceValidationCode::InvalidValue,
            ));
        }
        if body.content.len() > MAX_MEMORY_DOCUMENT_BYTES {
            return Err(ProductSurfaceError::validation(
                "content",
                ProductSurfaceValidationCode::TooLong,
            ));
        }
        if body.content.trim().is_empty() {
            return Err(ProductSurfaceError::validation(
                "content",
                ProductSurfaceValidationCode::Blank,
            ));
        }
        if let Some(hash) = &body.expected_content_hash {
            validate_hash(hash)?;
        }
        self.documents()?.write(caller, body).await
    }

    async fn archive(
        &self,
        caller: &ProductSurfaceCaller,
        mut body: MemoryArchiveRequest,
    ) -> Result<MemoryArchiveResponse, ProductSurfaceError> {
        body.path = validate_path(&body.path)?;
        validate_hash(&body.expected_content_hash)?;
        self.documents()?.archive(caller, body).await
    }

    // --- Supermemory ---

    async fn sm_status(
        &self,
        caller: &ProductSurfaceCaller,
        params: SupermemoryStatusRequest,
    ) -> Result<SupermemoryStatus, ProductSurfaceError> {
        let Some(supermemory) = self.supermemory.as_ref() else {
            return Ok(SupermemoryStatus {
                available: false,
                key_configured: false,
                profile_id: params.profile_id,
                binding: None,
                active: false,
                auto_recall: false,
                auto_capture: false,
            });
        };
        let binding = self
            .profile_binding(caller, params.profile_id.as_deref())
            .await?;
        let origin = if binding.is_some() {
            SupermemoryBindingOrigin::Profile
        } else {
            SupermemoryBindingOrigin::Default
        };
        let status = supermemory
            .admin_status(&sm_caller(caller), binding.as_ref())
            .await
            .map_err(map_supermemory_error)?;
        Ok(SupermemoryStatus {
            available: true,
            key_configured: status.key_configured,
            profile_id: params.profile_id,
            active: status.active(),
            binding: status.binding.map(|binding| SupermemoryBindingInfo {
                primary_container: binding.primary.into_inner(),
                read_containers: binding
                    .readable
                    .into_iter()
                    .map(|tag| tag.into_inner())
                    .collect(),
                origin,
            }),
            auto_recall: status.auto_recall,
            auto_capture: status.auto_capture,
        })
    }

    async fn sm_documents(
        &self,
        caller: &ProductSurfaceCaller,
        params: SupermemoryDocumentsRequest,
    ) -> Result<SupermemoryDocumentPage, ProductSurfaceError> {
        let supermemory = self.supermemory()?;
        let binding = self
            .profile_binding(caller, params.profile_id.as_deref())
            .await?;
        let page = supermemory
            .list_documents(
                &sm_caller(caller),
                binding.as_ref(),
                params.container.as_deref(),
                params.page,
                params
                    .limit
                    .map(|limit| limit.min(MAX_SUPERMEMORY_PAGE_LIMIT)),
            )
            .await
            .map_err(map_supermemory_error)?;
        Ok(SupermemoryDocumentPage {
            container: page.container.into_inner(),
            documents: page
                .documents
                .into_iter()
                .map(|document| SupermemoryDocumentSummary {
                    id: document.id,
                    custom_id: document.custom_id,
                    title: document.title,
                    preview: document.preview,
                    status: document.status,
                    container_tags: document.container_tags,
                    created_at: document.created_at,
                    updated_at: document.updated_at,
                })
                .collect(),
            page: page.page,
            total_pages: page.total_pages,
            total_items: page.total_items,
        })
    }

    async fn sm_search(
        &self,
        caller: &ProductSurfaceCaller,
        params: SupermemorySearchRequest,
    ) -> Result<SupermemorySearchResponse, ProductSurfaceError> {
        let supermemory = self.supermemory()?;
        let query = validate_query(&params.query)?;
        let binding = self
            .profile_binding(caller, params.profile_id.as_deref())
            .await?;
        let hits = supermemory
            .search_memories(
                &sm_caller(caller),
                binding.as_ref(),
                &query,
                params.container.as_deref(),
                params.limit,
            )
            .await
            .map_err(map_supermemory_error)?;
        Ok(SupermemorySearchResponse {
            query,
            hits: hits
                .into_iter()
                .map(|hit| SupermemoryMemoryHit {
                    id: hit.id,
                    container: hit.container.into_inner(),
                    content: hit.content,
                    similarity: hit.similarity,
                    updated_at: hit.updated_at,
                })
                .collect(),
        })
    }

    async fn sm_delete(
        &self,
        caller: &ProductSurfaceCaller,
        body: SupermemoryDeleteDocumentRequest,
    ) -> Result<SupermemoryDeleteDocumentResponse, ProductSurfaceError> {
        let supermemory = self.supermemory()?;
        let binding = self
            .profile_binding(caller, body.profile_id.as_deref())
            .await?;
        let evidence = supermemory
            .delete_document(&sm_caller(caller), binding.as_ref(), &body.id)
            .await
            .map_err(map_supermemory_error)?;
        Ok(SupermemoryDeleteDocumentResponse {
            id: evidence.id,
            verified: evidence.verified,
        })
    }

    async fn sm_forget(
        &self,
        caller: &ProductSurfaceCaller,
        body: SupermemoryForgetRequest,
    ) -> Result<SupermemoryForgetResponse, ProductSurfaceError> {
        let supermemory = self.supermemory()?;
        let binding = self
            .profile_binding(caller, body.profile_id.as_deref())
            .await?;
        let evidence = supermemory
            .forget_memory(
                &sm_caller(caller),
                binding.as_ref(),
                &body.container,
                &body.id,
            )
            .await
            .map_err(map_supermemory_error)?;
        Ok(SupermemoryForgetResponse {
            id: evidence.id,
            forgotten: evidence.forgotten,
        })
    }
}

#[async_trait]
impl ProductSurface for MemoryPageSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        match self.answer_command(&caller, &request).await {
            Some(output) => Ok(ProductSurfaceInvokeResponse { output: output? }),
            None => self.inner.invoke(caller, request).await,
        }
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        match self.answer_view(&caller, &request).await {
            Some(value) => Ok(ProductSurfaceQueryPage {
                items: vec![value?],
                next_cursor: None,
            }),
            None => self.inner.query(caller, request).await,
        }
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}

fn sm_caller(caller: &ProductSurfaceCaller) -> SupermemoryCaller {
    SupermemoryCaller {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
        agent_id: caller.agent_id.clone(),
        project_id: caller.project_id.clone(),
        thread_id: None,
    }
}

fn parse<T: DeserializeOwned>(input: serde_json::Value) -> Result<T, ProductSurfaceError> {
    serde_json::from_value(input).map_err(|error| {
        tracing::debug!(%error, "memory page: invalid input");
        ProductSurfaceError::validation("input", ProductSurfaceValidationCode::InvalidValue)
    })
}

fn to_value<T: Serialize>(value: T) -> Result<serde_json::Value, ProductSurfaceError> {
    serde_json::to_value(value).map_err(ProductSurfaceError::internal_from)
}

/// A memory-relative document path: no leading slash, no `..`, no
/// backslash or control characters, bounded.
fn validate_path(raw: &str) -> Result<String, ProductSurfaceError> {
    let invalid =
        || ProductSurfaceError::validation("path", ProductSurfaceValidationCode::InvalidValue);
    let path = raw.trim();
    if path.is_empty() {
        return Err(ProductSurfaceError::validation(
            "path",
            ProductSurfaceValidationCode::Blank,
        ));
    }
    if path.len() > MAX_MEMORY_PATH_BYTES {
        return Err(ProductSurfaceError::validation(
            "path",
            ProductSurfaceValidationCode::TooLong,
        ));
    }
    if path.chars().any(char::is_control) {
        return Err(ProductSurfaceError::validation(
            "path",
            ProductSurfaceValidationCode::InvalidControlCharacter,
        ));
    }
    if path.starts_with('/')
        || path.contains('\\')
        || path.contains("://")
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(invalid());
    }
    Ok(path.to_string())
}

fn validate_query(raw: &str) -> Result<String, ProductSurfaceError> {
    let query = raw.trim();
    if query.is_empty() {
        return Err(ProductSurfaceError::validation(
            "query",
            ProductSurfaceValidationCode::Blank,
        ));
    }
    if query.chars().count() > MAX_MEMORY_QUERY_CHARS {
        return Err(ProductSurfaceError::validation(
            "query",
            ProductSurfaceValidationCode::TooLong,
        ));
    }
    Ok(query.to_string())
}

/// SHA-256 hex, as the read returned it.
fn validate_hash(raw: &str) -> Result<(), ProductSurfaceError> {
    let hash = raw.strip_prefix("sha256:").unwrap_or(raw);
    if hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ProductSurfaceError::validation(
            "expected_content_hash",
            ProductSurfaceValidationCode::InvalidValue,
        ))
    }
}

fn map_supermemory_error(error: SupermemoryError) -> ProductSurfaceError {
    tracing::debug!(kind = error.kind(), "memory page: supermemory call failed");
    match error {
        SupermemoryError::Disabled => ProductSurfaceError::unavailable(false),
        SupermemoryError::InvalidContainer { .. } => {
            ProductSurfaceError::validation("container", ProductSurfaceValidationCode::InvalidValue)
        }
        SupermemoryError::InvalidInput { .. } => {
            ProductSurfaceError::validation("input", ProductSurfaceValidationCode::InvalidValue)
        }
        SupermemoryError::Http { status: 404 } => ProductSurfaceError::not_found(),
        SupermemoryError::RateLimited => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::RateLimited, 429, true)
        }
        SupermemoryError::Unauthorized { .. }
        | SupermemoryError::Secret { .. }
        | SupermemoryError::InvalidConfig { .. } => ProductSurfaceError::unavailable(false),
        SupermemoryError::Http { .. }
        | SupermemoryError::Transport { .. }
        | SupermemoryError::Decode { .. } => ProductSurfaceError::unavailable(true),
    }
}

#[cfg(test)]
#[path = "memory_page_tests.rs"]
mod tests;
