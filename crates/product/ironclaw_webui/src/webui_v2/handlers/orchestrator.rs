//! Orchestrator routes: delegated tasks of a thread, task status and
//! cancel, the thread's durable queue, runs, lineage, fork and merge-back.
//! Each handler forwards to one `ironclaw_product_contracts::orchestration`
//! descriptor; the product surface checks that the caller owns the thread.

use axum::Json;
use axum::extract::{Extension, Path, State};
use ironclaw_product_contracts::descriptors::ProductView;
use ironclaw_product_contracts::orchestration::*;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use serde::Deserialize;
use serde_json::Value;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

async fn thread_view(
    state: &WebUiV2State,
    caller: ProductSurfaceCaller,
    view: ProductView<OrchestratorThreadRequest, Value>,
    thread_id: String,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "thread_id": thread_id });
    Ok(Json(
        query_product_view(state.services(), caller, view.descriptor(), params, None).await?,
    ))
}

/// `GET /api/webchat/v2/threads/{thread_id}/tasks`
pub async fn orchestrator_thread_tasks(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    thread_view(&state, caller, ORCHESTRATOR_THREAD_TASKS_VIEW, thread_id).await
}

/// `GET /api/webchat/v2/threads/{thread_id}/queue`
pub async fn orchestrator_thread_queue(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    thread_view(&state, caller, ORCHESTRATOR_THREAD_QUEUE_VIEW, thread_id).await
}

/// `GET /api/webchat/v2/threads/{thread_id}/telemetry`
pub async fn orchestrator_thread_telemetry(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "thread_id": thread_id });
    let descriptor = ORCHESTRATOR_THREAD_TELEMETRY_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, descriptor, params, None).await?,
    ))
}

/// `GET /api/webchat/v2/threads/{thread_id}/route`
pub async fn orchestrator_thread_route(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    thread_view(&state, caller, ORCHESTRATOR_THREAD_ROUTE_VIEW, thread_id).await
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorRouteBody {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub picker: Option<Value>,
}

/// `POST /api/webchat/v2/threads/{thread_id}/route`
pub async fn orchestrator_thread_route_set(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<OrchestratorRouteBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = OrchestratorRouteRequest {
        thread_id,
        provider: body.provider,
        model: body.model,
        effort: body.effort,
        picker: body.picker,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            ORCHESTRATOR_ROUTE_COMMAND,
            request,
        )
        .await?,
    ))
}

/// `GET /api/webchat/v2/threads/{thread_id}/lineage`
pub async fn orchestrator_thread_lineage(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    thread_view(&state, caller, ORCHESTRATOR_THREAD_LINEAGE_VIEW, thread_id).await
}

/// `GET /api/webchat/v2/threads/{thread_id}/runs`
pub async fn orchestrator_thread_runs(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    thread_view(&state, caller, ORCHESTRATOR_THREAD_RUNS_VIEW, thread_id).await
}

/// `GET /api/webchat/v2/tasks/{task_id}`
pub async fn orchestrator_task(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "task_id": task_id });
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            ORCHESTRATOR_TASK_VIEW.descriptor(),
            params,
            None,
        )
        .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorCancelBody {
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/webchat/v2/tasks/{task_id}/cancel` — stops the child and
/// everything it delegated.
pub async fn orchestrator_task_cancel(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(task_id): Path<String>,
    Json(body): Json<OrchestratorCancelBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = OrchestratorTaskCancelRequest {
        task_id,
        reason: body.reason,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            ORCHESTRATOR_TASK_CANCEL_COMMAND,
            request,
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorQueueBody {
    pub action: String,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub before_id: Option<String>,
}

/// `POST /api/webchat/v2/threads/{thread_id}/queue` — `action` is one of
/// add, edit, remove, reorder, clear, hold, release.
pub async fn orchestrator_queue_mutate(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<OrchestratorQueueBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = OrchestratorQueueMutationRequest {
        thread_id,
        action: body.action,
        item_id: body.item_id,
        text: body.text,
        before_id: body.before_id,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            ORCHESTRATOR_QUEUE_MUTATE_COMMAND,
            request,
        )
        .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorForkBody {
    #[serde(default)]
    pub run: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// `POST /api/webchat/v2/threads/{thread_id}/fork`
pub async fn orchestrator_thread_fork(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<OrchestratorForkBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = OrchestratorForkRequest {
        thread_id,
        run: body.run,
        title: body.title,
        provider: body.provider,
        model: body.model,
    };
    Ok(Json(
        invoke_product_command(state.services(), caller, ORCHESTRATOR_FORK_COMMAND, request)
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorMergeBackBody {
    #[serde(default)]
    pub target_thread_id: Option<String>,
    #[serde(default)]
    pub run: Option<u32>,
}

/// `POST /api/webchat/v2/threads/{thread_id}/merge-back` — `thread_id` is
/// the fork; the target defaults to the thread it was forked from.
pub async fn orchestrator_thread_merge_back(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(body): Json<OrchestratorMergeBackBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = OrchestratorMergeBackRequest {
        thread_id,
        target_thread_id: body.target_thread_id,
        run: body.run,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            ORCHESTRATOR_MERGE_BACK_COMMAND,
            request,
        )
        .await?,
    ))
}
