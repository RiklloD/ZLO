//! Computer-use routes: a thread's session state with its latest screenshot
//! frame, and the user's Stop / take over / hand back. Each forwards one
//! `ironclaw_product_contracts::computer_use` descriptor; the product surface
//! keys the session by the authenticated caller, so a caller only ever sees
//! its own.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CONTROL_COMMAND, COMPUTER_USE_SESSION_VIEW, ComputerUseControlAction,
    ComputerUseControlRequest,
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
