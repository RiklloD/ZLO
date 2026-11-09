//! A thread's repository and console on the backend's machine, for a client
//! on another computer: git reads and changes, and one console command at a
//! time. Each handler forwards one `ironclaw_product_contracts::thread_repo`
//! descriptor; the routes are operator-only (classified in
//! `is_webui_v2_operator_webui_config_route_id`), and the product surface
//! checks operator authority and thread ownership again. No request names a
//! folder: the repository is the thread's own on the backend.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use ironclaw_product_contracts::thread_repo::{
    THREAD_CONSOLE_CONTROL_COMMAND, THREAD_CONSOLE_VIEW, THREAD_GIT_CHANGE_COMMAND,
    THREAD_GIT_VIEW, ThreadConsoleAction, ThreadConsoleControl, ThreadGitAction, ThreadGitChange,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// `?view=status|diff|log|commit&scope=&path=&limit=&sha=`; the product
/// surface decodes and validates the combination.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGitParams {
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub sha: Option<String>,
}

/// `GET /api/webchat/v2/threads/{thread_id}/git`
pub async fn thread_git(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Query(params): Query<ThreadGitParams>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let mut input = json!({
        "thread_id": thread_id,
        "view": params.view.as_deref().unwrap_or("status"),
    });
    for (key, value) in [
        ("scope", params.scope.map(Value::from)),
        ("path", params.path.map(Value::from)),
        ("limit", params.limit.map(Value::from)),
        ("sha", params.sha.map(Value::from)),
    ] {
        if let (Some(value), Some(map)) = (value, input.as_object_mut()) {
            map.insert(key.to_string(), value);
        }
    }
    let descriptor = THREAD_GIT_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, descriptor, input, None).await?,
    ))
}

/// `POST /api/webchat/v2/threads/{thread_id}/git` — `{action: stage|unstage
/// (paths), discard (path), checkpoint, undo_turn, keep_turn}`; answers the
/// status after the change.
pub async fn thread_git_change(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(action): Json<ThreadGitAction>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ThreadGitChange { thread_id, action };
    Ok(Json(
        invoke_product_command(state.services(), caller, THREAD_GIT_CHANGE_COMMAND, request)
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadConsoleParams {
    #[serde(default)]
    pub after: Option<u64>,
}

/// `GET /api/webchat/v2/threads/{thread_id}/console?after=` — the lines
/// after the cursor and the command's state (`available: false` where this
/// backend runs no console).
pub async fn thread_console(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Query(params): Query<ThreadConsoleParams>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let mut input = json!({ "thread_id": thread_id });
    if let (Some(after), Some(map)) = (params.after, input.as_object_mut()) {
        map.insert("after".to_string(), Value::from(after));
    }
    let descriptor = THREAD_CONSOLE_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, descriptor, input, None).await?,
    ))
}

/// `POST /api/webchat/v2/threads/{thread_id}/console` — `{action: start
/// (command), cancel, clear}`.
pub async fn thread_console_control(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(thread_id): Path<String>,
    Json(action): Json<ThreadConsoleAction>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ThreadConsoleControl { thread_id, action };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            THREAD_CONSOLE_CONTROL_COMMAND,
            request,
        )
        .await?,
    ))
}
