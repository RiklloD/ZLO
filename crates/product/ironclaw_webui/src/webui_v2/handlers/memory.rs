//! Memory page and hooks routes. Each handler forwards one
//! `ironclaw_product_contracts::memory_page` / `hooks_catalog` descriptor;
//! scope is always the authenticated caller's own, and input bounds are
//! enforced again behind the surface.

use axum::Json;
use axum::extract::{Extension, Query, State};
use ironclaw_product_contracts::hooks_catalog::HOOK_CATALOG_VIEW;
use ironclaw_product_contracts::memory_page::*;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use serde::Deserialize;
use serde_json::Value;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

async fn view(
    state: &WebUiV2State,
    caller: ProductSurfaceCaller,
    view: ironclaw_product_contracts::views::RebornViewDescriptor,
    params: Value,
) -> Result<Json<Value>, WebUiV2HttpError> {
    Ok(Json(
        query_product_view(state.services(), caller, view, params, None).await?,
    ))
}

/// `GET /api/webchat/v2/memory/documents`
pub async fn memory_tree(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        MEMORY_TREE_VIEW.descriptor(),
        serde_json::json!({}),
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPathQuery {
    pub path: String,
}

/// `GET /api/webchat/v2/memory/documents/content?path=…`
pub async fn memory_document(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<MemoryPathQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        MEMORY_DOCUMENT_VIEW.descriptor(),
        serde_json::json!({ "path": query.path }),
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/webchat/v2/memory/search?q=…&limit=…`
pub async fn memory_search(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<MemorySearchQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        MEMORY_SEARCH_VIEW.descriptor(),
        serde_json::json!({ "query": query.q, "limit": query.limit }),
    )
    .await
}

/// `POST /api/webchat/v2/memory/documents` — body: [`MemoryWriteRequest`].
pub async fn memory_write(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<MemoryWriteRequest>,
) -> Result<Json<MemoryWriteResponse>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(state.services(), caller, MEMORY_WRITE_COMMAND, body).await?,
    ))
}

/// `POST /api/webchat/v2/memory/documents/archive` — body: [`MemoryArchiveRequest`].
pub async fn memory_archive(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<MemoryArchiveRequest>,
) -> Result<Json<MemoryArchiveResponse>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(state.services(), caller, MEMORY_ARCHIVE_COMMAND, body).await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupermemoryQuery {
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub q: Option<String>,
}

/// `GET /api/webchat/v2/memory/supermemory?profile_id=…`
pub async fn supermemory_status(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<SupermemoryQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        SUPERMEMORY_STATUS_VIEW.descriptor(),
        serde_json::json!({ "profile_id": query.profile_id }),
    )
    .await
}

/// `GET /api/webchat/v2/memory/supermemory/documents?profile_id=…&container=…&page=…&limit=…`
pub async fn supermemory_documents(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<SupermemoryQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        SUPERMEMORY_DOCUMENTS_VIEW.descriptor(),
        serde_json::json!({
            "profile_id": query.profile_id,
            "container": query.container,
            "page": query.page,
            "limit": query.limit,
        }),
    )
    .await
}

/// `GET /api/webchat/v2/memory/supermemory/search?q=…&profile_id=…&container=…&limit=…`
pub async fn supermemory_search(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<SupermemoryQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        SUPERMEMORY_SEARCH_VIEW.descriptor(),
        serde_json::json!({
            "profile_id": query.profile_id,
            "query": query.q.unwrap_or_default(),
            "container": query.container,
            "limit": query.limit,
        }),
    )
    .await
}

/// `POST /api/webchat/v2/memory/supermemory/documents/delete`
pub async fn supermemory_delete_document(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<SupermemoryDeleteDocumentRequest>,
) -> Result<Json<SupermemoryDeleteDocumentResponse>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            SUPERMEMORY_DELETE_DOCUMENT_COMMAND,
            body,
        )
        .await?,
    ))
}

/// `POST /api/webchat/v2/memory/supermemory/memories/forget`
pub async fn supermemory_forget(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<SupermemoryForgetRequest>,
) -> Result<Json<SupermemoryForgetResponse>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(state.services(), caller, SUPERMEMORY_FORGET_COMMAND, body).await?,
    ))
}

/// `GET /api/webchat/v2/hooks` — read-only hook inventory.
pub async fn hooks_catalog(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    view(
        &state,
        caller,
        HOOK_CATALOG_VIEW.descriptor(),
        serde_json::json!({}),
    )
    .await
}
