//! Package settings routes. Each handler forwards one
//! `ironclaw_product_contracts::extension_settings` descriptor; scope is always
//! the authenticated caller's own, and input bounds are enforced again behind
//! the surface.
//!
//! - `GET  /api/webchat/v2/extensions/{extension_id}/settings`
//! - `PUT  /api/webchat/v2/extensions/{extension_id}/settings/{setting_id}`
//!
//! A value that does not fit the declaration answers the standard validation
//! error body (`field = "value"`, `validation_code = "invalid_value"`) plus a
//! plain `reason`. The reason is host-authored validation text naming the
//! setting and what it expects; it never echoes the submitted value or any
//! backend detail.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Response};
use ironclaw_product_contracts::extension_settings::{
    EXTENSION_SETTING_SET_COMMAND, EXTENSION_SETTINGS_VIEW, ExtensionSettingSetOutcome,
    ExtensionSettingSetRequest, ExtensionSettingValueBody, ExtensionSettingsResponse,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceValidationCode,
};
use serde::Deserialize;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

#[derive(Debug, Deserialize)]
pub struct ExtensionSettingsPath {
    pub extension_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ExtensionSettingPath {
    pub extension_id: String,
    pub setting_id: String,
}

/// `GET /api/webchat/v2/extensions/{extension_id}/settings`
pub async fn extension_settings_list(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(ExtensionSettingsPath { extension_id }): Path<ExtensionSettingsPath>,
) -> Result<Json<ExtensionSettingsResponse>, WebUiV2HttpError> {
    let response: ExtensionSettingsResponse = query_product_view(
        state.services(),
        caller,
        EXTENSION_SETTINGS_VIEW.descriptor(),
        serde_json::json!({ "extension_id": extension_id }),
        None,
    )
    .await?;
    Ok(Json(response))
}

/// `PUT /api/webchat/v2/extensions/{extension_id}/settings/{setting_id}`
pub async fn extension_setting_set(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(ExtensionSettingPath {
        extension_id,
        setting_id,
    }): Path<ExtensionSettingPath>,
    Json(body): Json<ExtensionSettingValueBody>,
) -> Result<Response, WebUiV2HttpError> {
    let outcome = invoke_product_command(
        state.services(),
        caller,
        EXTENSION_SETTING_SET_COMMAND,
        ExtensionSettingSetRequest {
            extension_id,
            setting_id,
            value: body.value,
        },
    )
    .await?;
    match outcome {
        ExtensionSettingSetOutcome::Updated { setting } => Ok(Json(setting).into_response()),
        ExtensionSettingSetOutcome::Rejected { reason } => {
            let (status, error_body) = WebUiV2HttpError::from(ProductSurfaceError::validation(
                "value",
                ProductSurfaceValidationCode::InvalidValue,
            ))
            .into_response_parts();
            let mut body =
                serde_json::to_value(error_body).map_err(ProductSurfaceError::internal_from)?;
            if let Some(object) = body.as_object_mut() {
                object.insert("reason".to_string(), serde_json::Value::String(reason));
            }
            Ok((status, Json(body)).into_response())
        }
    }
}
