//! Settings → Keys routes: the deployment's service API keys. Operator-only
//! (the same gate as LLM provider keys); each handler forwards one
//! `ironclaw_product_contracts::service_keys` descriptor. No response carries
//! a key value.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, State};
use ironclaw_product_contracts::service_keys::*;
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceValidationCode,
};
use serde_json::Value;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// The body, or a fixed validation error: axum's own rejection text can
/// quote the request (a pasted key), so it never reaches the response.
fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, WebUiV2HttpError> {
    body.map(|Json(body)| body).map_err(|rejection| {
        tracing::debug!(status = %rejection.status(), "keys: request body rejected");
        ProductSurfaceError::validation("request", ProductSurfaceValidationCode::InvalidValue)
            .into()
    })
}

/// `GET /api/webchat/v2/keys`
pub async fn service_keys_list(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            SERVICE_KEYS_VIEW.descriptor(),
            serde_json::json!({}),
            None,
        )
        .await?,
    ))
}

/// `POST /api/webchat/v2/keys` — body: [`ServiceKeySetRequest`].
pub async fn service_key_set(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    request: Result<Json<ServiceKeySetRequest>, JsonRejection>,
) -> Result<Json<ServiceKeySetResponse>, WebUiV2HttpError> {
    let body = body(request)?;
    Ok(Json(
        invoke_product_command(state.services(), caller, SERVICE_KEY_SET_COMMAND, body).await?,
    ))
}

/// `POST /api/webchat/v2/keys/import` — body: [`ServiceKeyIdRequest`].
pub async fn service_key_import(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    request: Result<Json<ServiceKeyIdRequest>, JsonRejection>,
) -> Result<Json<ServiceKeySetResponse>, WebUiV2HttpError> {
    let body = body(request)?;
    Ok(Json(
        invoke_product_command(state.services(), caller, SERVICE_KEY_IMPORT_COMMAND, body).await?,
    ))
}

/// `POST /api/webchat/v2/keys/remove` — body: [`ServiceKeyIdRequest`].
pub async fn service_key_remove(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    request: Result<Json<ServiceKeyIdRequest>, JsonRejection>,
) -> Result<Json<ServiceKeyRemoveResponse>, WebUiV2HttpError> {
    let body = body(request)?;
    Ok(Json(
        invoke_product_command(state.services(), caller, SERVICE_KEY_REMOVE_COMMAND, body).await?,
    ))
}
