//! Inbox and goal routes (v2 stage 5): the caller's inbox list, one
//! lifecycle mark on a thread (pin, snooze, settle, visit), and a thread's
//! goal. Each handler forwards to one `ironclaw_product_contracts::inbox`
//! descriptor; the rows are always the authenticated caller's own. Also the
//! generated thread title and the operator-wide background-task models
//! (`ironclaw_product_contracts::task_models`).

use axum::Json;
use axum::extract::{Extension, Path, State};
use ironclaw_product_contracts::inbox::*;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use ironclaw_product_contracts::task_models::{
    TASK_MODELS_GET_COMMAND, TASK_MODELS_SET_COMMAND, TaskModels,
};
use serde::Deserialize;
use serde_json::Value;

use super::{invoke_product_command, query_product_view, require_operator_webui_config};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::{WebUiV2Capabilities, WebUiV2State};

/// `GET /api/webchat/v2/inbox` — rows joined and classified by the backend.
pub async fn inbox_list(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let view = INBOX_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, serde_json::json!({}), None).await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxMutateBody {
    pub action: InboxAction,
    #[serde(default)]
    pub until: Option<i64>,
}

/// `POST /api/webchat/v2/inbox/{thread_id}` — answers with the list re-read.
pub async fn inbox_mutate(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<InboxMutateBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = InboxMutationRequest {
        thread_id,
        action: body.action,
        until: body.until,
    };
    Ok(Json(
        invoke_product_command(state.services(), caller, INBOX_MUTATE_COMMAND, request).await?,
    ))
}

/// `GET /api/webchat/v2/threads/{thread_id}/goal`
pub async fn thread_goal(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "thread_id": thread_id });
    let view = THREAD_GOAL_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, params, None).await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGoalBody {
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub status: Option<GoalStatus>,
}

/// `POST /api/webchat/v2/threads/{thread_id}/goal` — an empty body clears it.
pub async fn thread_goal_set(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<ThreadGoalBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ThreadGoalSetRequest {
        thread_id,
        objective: body.objective,
        status: body.status,
    };
    Ok(Json(
        invoke_product_command(state.services(), caller, THREAD_GOAL_SET_COMMAND, request).await?,
    ))
}

/// `GET /api/webchat/v2/llm/task-models`
pub async fn task_models_get(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Extension(capabilities): Extension<WebUiV2Capabilities>,
) -> Result<Json<TaskModels>, WebUiV2HttpError> {
    require_operator_webui_config(capabilities)?;
    let input = serde_json::json!({});
    Ok(Json(
        invoke_product_command(state.services(), caller, TASK_MODELS_GET_COMMAND, input).await?,
    ))
}

/// `POST /api/webchat/v2/llm/task-models` — replaces every task's model.
pub async fn task_models_set(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Extension(capabilities): Extension<WebUiV2Capabilities>,
    Json(body): Json<TaskModels>,
) -> Result<Json<TaskModels>, WebUiV2HttpError> {
    require_operator_webui_config(capabilities)?;
    Ok(Json(
        invoke_product_command(state.services(), caller, TASK_MODELS_SET_COMMAND, body).await?,
    ))
}
