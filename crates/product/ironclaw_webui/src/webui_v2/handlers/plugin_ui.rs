//! Plugin UI slot routes. Each handler forwards one
//! `ironclaw_product_contracts::plugin_ui` view; the caller is always the
//! authenticated one, and the surface re-resolves the slot, bounds the event
//! payload, bounds render time, and validates the returned tree.

use axum::Json;
use axum::extract::{Extension, State};
use ironclaw_extension_contracts::plugin_ui::{PluginUiAction, PluginUiSlotId};
use ironclaw_host_api::ids::ExtensionId;
use ironclaw_product_contracts::plugin_ui::{
    PLUGIN_UI_RENDER_VIEW, PLUGIN_UI_SLOTS_VIEW, PluginUiEvent, PluginUiRenderContext,
    PluginUiRenderRequest,
};
use ironclaw_product_contracts::surface::{ProductSurfaceCaller, ProductSurfaceError};
use serde::Deserialize;
use serde_json::Value;

use super::query_product_view;
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// `POST /api/webchat/v2/plugin-ui/render` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiRenderBody {
    pub extension_id: ExtensionId,
    pub slot_id: PluginUiSlotId,
    #[serde(default)]
    pub context: PluginUiRenderContext,
}

/// `POST /api/webchat/v2/plugin-ui/event` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiEventBody {
    pub extension_id: ExtensionId,
    pub slot_id: PluginUiSlotId,
    #[serde(default)]
    pub context: PluginUiRenderContext,
    pub action: PluginUiAction,
    #[serde(default)]
    pub payload: Value,
}

async fn render(
    state: &WebUiV2State,
    caller: ProductSurfaceCaller,
    request: PluginUiRenderRequest,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::to_value(request).map_err(ProductSurfaceError::internal_from)?;
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            PLUGIN_UI_RENDER_VIEW.descriptor(),
            params,
            None,
        )
        .await?,
    ))
}

/// `GET /api/webchat/v2/plugin-ui/slots` — `{ slots: [...] }`.
pub async fn plugin_ui_slots(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            PLUGIN_UI_SLOTS_VIEW.descriptor(),
            serde_json::json!({}),
            None,
        )
        .await?,
    ))
}

/// `POST /api/webchat/v2/plugin-ui/render` — `{ tree }` or `{ error }`.
pub async fn plugin_ui_render(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<PluginUiRenderBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    render(
        &state,
        caller,
        PluginUiRenderRequest {
            extension_id: body.extension_id,
            slot_id: body.slot_id,
            context: body.context,
            event: None,
        },
    )
    .await
}

/// `POST /api/webchat/v2/plugin-ui/event` — re-render after the event:
/// `{ tree }` or `{ error }`.
pub async fn plugin_ui_event(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<PluginUiEventBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    render(
        &state,
        caller,
        PluginUiRenderRequest {
            extension_id: body.extension_id,
            slot_id: body.slot_id,
            context: body.context,
            event: Some(PluginUiEvent {
                action: body.action,
                payload: body.payload,
            }),
        },
    )
    .await
}
