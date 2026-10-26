//! Computer-use routes: a thread's session state with its latest screenshot
//! frame, the user's Stop / take over / hand back, and the live view (every
//! session of the caller, and one window's picture). Each forwards one
//! `ironclaw_product_contracts::computer_use` descriptor; the product surface
//! keys the session by the authenticated caller, so a caller only ever sees
//! its own.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CONTROL_COMMAND, COMPUTER_USE_FRAME_VIEW, COMPUTER_USE_SESSION_VIEW,
    COMPUTER_USE_STATE_VIEW, ComputerUseControlAction, ComputerUseControlRequest,
};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use serde::Deserialize;
use serde_json::Value;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseSessionQuery {
    #[serde(default)]
    pub after_frame: Option<u64>,
}

/// `GET /api/webchat/v2/threads/{thread_id}/computer-use`
pub async fn computer_use_session(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Query(query): Query<ComputerUseSessionQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "thread_id": thread_id, "after_frame": query.after_frame });
    let descriptor = COMPUTER_USE_SESSION_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, descriptor, params, None).await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseControlBody {
    pub action: ComputerUseControlAction,
    #[serde(default)]
    pub viewer: Option<String>,
}

/// `POST /api/webchat/v2/threads/{thread_id}/computer-use`
pub async fn computer_use_control(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<ComputerUseControlBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ComputerUseControlRequest {
        thread_id,
        action: body.action,
        viewer: body.viewer,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            COMPUTER_USE_CONTROL_COMMAND,
            request,
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseStateQuery {
    #[serde(default)]
    pub viewer: Option<String>,
}

/// `GET /api/webchat/v2/computer-use`
pub async fn computer_use_state(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<ComputerUseStateQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "viewer": query.viewer });
    let descriptor = COMPUTER_USE_STATE_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, descriptor, params, None).await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseFrameQuery {
    pub window_id: u32,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default)]
    pub after_seq: Option<u64>,
}

/// `GET /api/webchat/v2/threads/{thread_id}/computer-use/frame`
pub async fn computer_use_frame(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Query(query): Query<ComputerUseFrameQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({
        "thread_id": thread_id,
        "window_id": query.window_id,
        "width": query.width,
        "fps": query.fps,
        "after_seq": query.after_seq,
    });
    let descriptor = COMPUTER_USE_FRAME_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, descriptor, params, None).await?,
    ))
}
