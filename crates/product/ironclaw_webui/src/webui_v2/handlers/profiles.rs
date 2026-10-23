//! Profile routes (stage 3): list, read, create, update and archive
//! profiles, create a thread on one, and the one-time Hermes import. Each
//! handler forwards to one `ironclaw_product_contracts::profiles` descriptor;
//! profiles are always the authenticated caller's own.

use axum::Json;
use axum::extract::{Extension, Path, State};
use ironclaw_product_contracts::profiles::*;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use serde::Deserialize;
use serde_json::Value;

use super::{invoke_product_command, query_product_view};
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// `GET /api/webchat/v2/profiles` — most recently used first.
pub async fn profiles_list(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let view = PROFILES_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, serde_json::json!({}), None).await?,
    ))
}

/// `GET /api/webchat/v2/profiles/{profile_id}`
pub async fn profile_get(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(profile_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "profile_id": profile_id });
    let view = PROFILE_VIEW.descriptor();
    Ok(Json(
        query_product_view(state.services(), caller, view, params, None).await?,
    ))
}

/// `POST /api/webchat/v2/profiles` — body: [`ProfileWriteRequest`].
pub async fn profile_create(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Json(body): Json<ProfileWriteRequest>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(state.services(), caller, PROFILE_CREATE_COMMAND, body).await?,
    ))
}

/// `POST /api/webchat/v2/profiles/{profile_id}` — body: [`ProfileSettings`].
pub async fn profile_update(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(profile_id): Path<String>,
    Json(settings): Json<ProfileSettings>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ProfileWriteRequest {
        profile_id,
        settings,
    };
    Ok(Json(
        invoke_product_command(state.services(), caller, PROFILE_UPDATE_COMMAND, request).await?,
    ))
}

/// `DELETE /api/webchat/v2/profiles/{profile_id}` — archives the profile.
pub async fn profile_delete(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(profile_id): Path<String>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ProfileRequest { profile_id };
    Ok(Json(
        invoke_product_command(state.services(), caller, PROFILE_DELETE_COMMAND, request).await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileThreadBody {
    #[serde(default)]
    pub client_action_id: Option<String>,
}

/// `POST /api/webchat/v2/profiles/{profile_id}/threads` — a new thread that
/// belongs to the profile for life.
pub async fn profile_thread_create(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Path(profile_id): Path<String>,
    Json(body): Json<ProfileThreadBody>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let request = ProfileThreadCreateRequest {
        profile_id,
        client_action_id: body.client_action_id,
    };
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            PROFILE_THREAD_CREATE_COMMAND,
            request,
        )
        .await?,
    ))
}

/// `POST /api/webchat/v2/profiles/import/hermes` — copies Hermes profiles;
/// re-running imports nothing new.
pub async fn profiles_import_hermes(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    Ok(Json(
        invoke_product_command(
            state.services(),
            caller,
            PROFILE_IMPORT_HERMES_COMMAND,
            HermesImportRequest {},
        )
        .await?,
    ))
}
