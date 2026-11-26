//! Inbox and goal routes (v2 stage 5): the caller's inbox list, one
//! lifecycle mark on a thread (pin, snooze, settle, visit), and a thread's
//! goal. Each handler forwards to one `ironclaw_product_contracts::inbox`
//! descriptor; the rows are always the authenticated caller's own. Also the
//! generated thread title and the operator-wide background-task models
//! (`ironclaw_product_contracts::task_models`), and terminal session import
//! (`ironclaw_product_contracts::session_import`), the caller's own.

use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use ironclaw_product_contracts::inbox::*;
use ironclaw_product_contracts::session_import::{
    SESSION_IMPORT_COMMAND, SESSION_IMPORT_PROJECTS_VIEW, SESSION_IMPORT_VIEW, SessionImportRequest,
};
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurfaceCaller, ProductSurfaceStreamRequest,
    ProductSurfaceStreamResponse,
};
use ironclaw_product_contracts::task_models::{
    TASK_MODELS_GET_COMMAND, TASK_MODELS_SET_COMMAND, TaskModels,
};
use serde::Deserialize;
use serde_json::Value;

use super::{
    StreamEventsQuery, invoke_product_command, query_product_view, require_operator_webui_config,
    sse_capacity_rejected, sse_error_event, stream_connection_id,
};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::{WebUiV2Capabilities, WebUiV2State};
use crate::webui_v2::sse_capacity::{SSE_MAX_LIFETIME, SseAcquireResult, SseSlot};

/// Keep-alive comment cadence for `GET /api/webchat/v2/inbox/events`.
const INBOX_SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);

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

/// `GET /api/webchat/v2/inbox/events` — caller-scoped SSE stream of coalesced
/// inbox invalidation hints (`{"kind":"inbox_changed"}`).
pub async fn inbox_events(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<StreamEventsQuery>,
) -> Result<Response, WebUiV2HttpError> {
    let connection_id = stream_connection_id(query.connection_id.as_deref());
    let slot = match state.inbox_sse_capacity().try_acquire_ordered(
        &caller.tenant_id,
        &caller.user_id,
        connection_id,
        connection_id.and(query.connection_generation),
    ) {
        SseAcquireResult::Acquired(slot) => slot,
        SseAcquireResult::AtCapacity { refundable } => {
            return Ok(sse_capacity_rejected(refundable));
        }
        SseAcquireResult::StaleGeneration => {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    };
    // Subscribed before the response starts: a change between "200" and
    // the first read is not lost, so a client that reads its inbox once
    // the stream is open has missed nothing.
    let surface = BoundProductSurface::new(state.services().clone(), caller);
    let opened = surface
        .stream_events(ProductSurfaceStreamRequest {
            stream_id: Some(INBOX_EVENTS_STREAM_ID.to_string()),
            after_cursor: None,
        })
        .await
        .map_err(WebUiV2HttpError::from)?;
    let stream = build_inbox_sse_stream(opened, slot);
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(INBOX_SSE_KEEPALIVE_INTERVAL))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

fn build_inbox_sse_stream(
    mut opened: ProductSurfaceStreamResponse,
    slot: SseSlot,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        let mut slot_guard = slot;
        let started_at = tokio::time::Instant::now();

        // Flush headers immediately with an SSE comment so clients do not wait
        // for the first invalidation hint to see HTTP 200.
        yield Ok(Event::default().comment("connected"));

        for raw in std::mem::take(&mut opened.events) {
            if let Ok(payload) = serde_json::to_string(&raw) {
                yield Ok(Event::default().event(INBOX_CHANGED_EVENT_KIND).data(payload));
            }
        }
        let Some(subscription) = opened.subscription.take() else {
            return;
        };

        loop {
            let remaining = SSE_MAX_LIFETIME.saturating_sub(started_at.elapsed());
            if remaining.is_zero() {
                return;
            }
            let next = tokio::select! {
                biased;
                _ = slot_guard.cancelled() => return,
                result = tokio::time::timeout(remaining, subscription.next()) => result,
            };
            let response = match next {
                Ok(Some(Ok(response))) => response,
                Ok(Some(Err(error))) => {
                    yield Ok(sse_error_event(error));
                    return;
                }
                Ok(None) | Err(_) => return,
            };
            for raw in response.events {
                if let Ok(payload) = serde_json::to_string(&raw) {
                    yield Ok(Event::default().event(INBOX_CHANGED_EVENT_KIND).data(payload));
                }
            }
        }
    }
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
    #[serde(default)]
    pub check: Option<GoalCheckSpec>,
    #[serde(default)]
    pub clear_check: bool,
    #[serde(default)]
    pub check_action: Option<GoalCheckAction>,
    #[serde(default)]
    pub check_revision: Option<u64>,
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
        check: body.check,
        clear_check: body.clear_check,
        check_action: body.check_action,
        check_revision: body.check_revision,
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

/// `GET /api/webchat/v2/session-imports` — settings, last run, `running`.
pub async fn session_import_status(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let view = SESSION_IMPORT_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, serde_json::json!({}), None).await?,
    ))
}

/// `GET /api/webchat/v2/session-imports/projects` — the folders terminal
/// sessions ran in (first-run setup's project list). Imports nothing.
pub async fn session_import_projects(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let view = SESSION_IMPORT_PROJECTS_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, serde_json::json!({}), None).await?,
    ))
}

/// `POST /api/webchat/v2/session-imports` — change settings and/or start a
/// run; answers the status.
pub async fn session_import_run(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<SessionImportRequest>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(state.services(), caller, SESSION_IMPORT_COMMAND, body).await?,
    ))
}
