//! Schedule routes (`t3_schedule` rows): list the caller's schedules,
//! preview a spec, create/update/delete/run one, rotate a webhook's signing
//! secret and reveal it. Each handler forwards to one
//! `ironclaw_product_contracts::orchestration` descriptor; the product
//! surface checks the caller owns the schedule and its thread. The reveal
//! answer carries a secret: it is returned to the caller and nothing here
//! logs a body.

use axum::Json;
use axum::extract::{Extension, Path, State};
use ironclaw_product_contracts::orchestration::*;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use serde::Deserialize;
use serde_json::Value;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// Create (`thread_id` required) or update body; absent fields keep the
/// stored value on update.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleBody {
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub schedule: Option<Value>,
    #[serde(default)]
    pub bind_to_current_thread: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Absent keeps the stored signature; `null` turns signing off.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub signature: Option<Option<Value>>,
    #[serde(default)]
    pub client_request_id: Option<String>,
}

async fn mutate(
    state: &WebUiV2State,
    caller: ProductSurfaceCaller,
    action: OrchestratorScheduleAction,
    schedule_id: Option<String>,
    body: ScheduleBody,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = OrchestratorScheduleMutationRequest {
        action,
        schedule_id,
        thread_id: body.thread_id,
        title: body.title,
        prompt: body.prompt,
        schedule: body.schedule,
        bind_to_current_thread: body.bind_to_current_thread,
        enabled: body.enabled,
        signature: body.signature,
        client_request_id: body.client_request_id,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND,
            request,
        )
        .await?,
    ))
}

/// `GET /api/webchat/v2/schedules` — webhook URLs masked.
pub async fn schedules_list(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let view = ORCHESTRATOR_SCHEDULES_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, serde_json::json!({}), None).await?,
    ))
}

/// `POST /api/webchat/v2/schedules/preview` — body:
/// [`OrchestratorSchedulePreviewRequest`]; stores nothing.
pub async fn schedule_preview(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<OrchestratorSchedulePreviewRequest>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let mut params = serde_json::json!({ "schedule": body.schedule });
    if let Some(prompt) = body.prompt {
        params["prompt"] = Value::String(prompt);
    }
    let view = ORCHESTRATOR_SCHEDULE_PREVIEW_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, params, None).await?,
    ))
}

/// `POST /api/webchat/v2/schedules` — body: [`ScheduleBody`] with `thread_id`.
pub async fn schedule_create(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<ScheduleBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    mutate(
        &state,
        caller,
        OrchestratorScheduleAction::Create,
        None,
        body,
    )
    .await
}

/// `POST /api/webchat/v2/schedules/{schedule_id}` — body: [`ScheduleBody`].
pub async fn schedule_update(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(schedule_id): Path<String>,
    Json(body): Json<ScheduleBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let action = OrchestratorScheduleAction::Update;
    mutate(&state, caller, action, Some(schedule_id), body).await
}

/// `DELETE /api/webchat/v2/schedules/{schedule_id}`
pub async fn schedule_delete(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(schedule_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let action = OrchestratorScheduleAction::Delete;
    mutate(
        &state,
        caller,
        action,
        Some(schedule_id),
        ScheduleBody::default(),
    )
    .await
}

/// `POST /api/webchat/v2/schedules/{schedule_id}/run`
pub async fn schedule_run(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(schedule_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let action = OrchestratorScheduleAction::RunNow;
    mutate(
        &state,
        caller,
        action,
        Some(schedule_id),
        ScheduleBody::default(),
    )
    .await
}

/// `POST /api/webchat/v2/schedules/{schedule_id}/rotate-secret`
pub async fn schedule_rotate_secret(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(schedule_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let action = OrchestratorScheduleAction::RotateSecret;
    mutate(
        &state,
        caller,
        action,
        Some(schedule_id),
        ScheduleBody::default(),
    )
    .await
}

/// `GET /api/webchat/v2/schedules/{schedule_id}/secret` — the full webhook
/// URL and signing secret, for the desktop's explicit Reveal/Copy.
pub async fn schedule_secret(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(schedule_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "schedule_id": schedule_id });
    let view = ORCHESTRATOR_SCHEDULE_SECRET_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, params, None).await?,
    ))
}
