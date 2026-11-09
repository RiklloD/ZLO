//! Self-update routes ("update all machines";
//! `docs/internal/plans/2026-11-08-zlo-remote-environments.md`, "Self-update").
//!
//! - `GET /api/operator/update` — [`UpdateStatus`]: method, running build,
//!   what the last check found, and the last update job (read from disk, so
//!   it survives the backend restarting itself).
//! - `POST /api/operator/update/check` — fetch the source, answer the
//!   refreshed status.
//! - `POST /api/operator/update` — start the update as a detached job;
//!   `202` with the status, `409 update_running` while one runs.
//!
//! - `POST /api/operator/update/enable` — turn self-update on from the
//!   checkout this backend was built from; `409 no_checkout` when there is
//!   none.
//!
//! All four are bearer-authenticated and operator-only, and none takes a
//! body (`BodyLimitPolicy::NoBody`, so a body is refused before the
//! handler): the repository, branch and build steps come only from the
//! host's local `[update]` config, behind [`SelfUpdateService`]. The binary
//! implements the port; this module owns HTTP and the operator check.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ingress::{
    AllowedEffectPath, AuditTraceClass, BodyLimitPolicy, CorsPolicy, IngressAuthPolicy,
    IngressAuthScheme, IngressPolicy, IngressPolicyParts, IngressRouteDescriptor,
    IngressScopeSource, ListenerClass, RateLimitPolicy, RateLimitScope, StreamingMode,
    WebSocketOriginPolicy,
};
use ironclaw_host_ingress::ProtectedRouteMount;
use ironclaw_product_contracts::self_update::{
    OPERATOR_UPDATE_CHECK_PATH, OPERATOR_UPDATE_ENABLE_PATH, OPERATOR_UPDATE_PATH, UpdateErrorCode,
    UpdateStatus,
};
use serde::Serialize;

use crate::webui_v2::WebUiV2Capabilities;

const RATE_WINDOW_SECONDS: std::num::NonZeroU32 = std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero
/// ZLO polls every few seconds while a machine updates.
const STATUS_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(120).expect("120 != 0"); // safety: const-evaluated, literal non-zero
/// Each check runs `git fetch`.
const CHECK_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(10).expect("10 != 0"); // safety: const-evaluated, literal non-zero
const START_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(5).expect("5 != 0"); // safety: const-evaluated, literal non-zero
const ENABLE_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(5).expect("5 != 0"); // safety: const-evaluated, literal non-zero

/// Why a self-update call was refused. The transport maps each variant to a
/// fixed status and code; detail for the operator travels in `message`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelfUpdateError {
    /// `[update] method` is `none`.
    #[error("self-update is not set up on this machine")]
    NotConfigured,
    /// Another update holds the lock.
    #[error("an update is already running")]
    AlreadyRunning,
    /// The fetch or the checkout refused the check (dirty, diverged,
    /// unreachable remote). The message is operator-facing and holds no
    /// secret; the implementor logged the rest.
    #[error("update check failed: {0}")]
    CheckFailed(String),
    /// Self-update cannot be turned on from here: no source checkout this
    /// backend was built from, or it is not a usable checkout. The message
    /// is operator-facing and holds no secret.
    #[error("self-update cannot be turned on: {0}")]
    NoCheckout(String),
    /// The update records or the job could not be read or started; the
    /// implementor logged the cause.
    #[error("self-update is unavailable")]
    Unavailable,
}

/// The host's self-update machinery: config, git, the detached job and its
/// records. Declared here, where its only caller is; implemented by the
/// `ironclaw` binary, which alone knows how this host was installed. No
/// method takes request input.
#[async_trait]
pub trait SelfUpdateService: Send + Sync {
    /// Current state, from the records on disk. No network.
    async fn status(&self) -> Result<UpdateStatus, SelfUpdateError>;
    /// Fetch the configured branch and record what is available.
    async fn check(&self) -> Result<UpdateStatus, SelfUpdateError>;
    /// Start the update job, detached from this process, and answer the
    /// status with `last_run.state = running`.
    async fn start(&self) -> Result<UpdateStatus, SelfUpdateError>;
    /// Turn self-update on from the checkout this backend was built from
    /// (already on: a no-op) and answer the status.
    async fn enable(&self) -> Result<UpdateStatus, SelfUpdateError>;
}

/// Bearer-authed mount of the four routes over `service`.
pub fn self_update_route_mount(service: Arc<dyn SelfUpdateService>) -> ProtectedRouteMount {
    let router = Router::new()
        .route(OPERATOR_UPDATE_PATH, get(get_status).post(start_update))
        .route(OPERATOR_UPDATE_CHECK_PATH, post(check_update))
        .route(OPERATOR_UPDATE_ENABLE_PATH, post(enable_update))
        .with_state(service);
    ProtectedRouteMount::new(router, route_descriptors())
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

fn error_response(status: StatusCode, error: &'static str, message: String) -> Response {
    (status, Json(ErrorBody { error, message })).into_response()
}

fn service_error(error: SelfUpdateError) -> Response {
    match error {
        SelfUpdateError::NotConfigured => error_response(
            StatusCode::CONFLICT,
            UpdateErrorCode::NotConfigured.as_str(),
            "Self-update is not set up here. Run `zlo update enable` on this machine (add \
             `--repo /path/to/checkout` if it does not find its source checkout)."
                .into(),
        ),
        SelfUpdateError::NoCheckout(message) => error_response(
            StatusCode::CONFLICT,
            UpdateErrorCode::NoCheckout.as_str(),
            message,
        ),
        SelfUpdateError::AlreadyRunning => error_response(
            StatusCode::CONFLICT,
            UpdateErrorCode::UpdateRunning.as_str(),
            "An update is already running on this machine.".into(),
        ),
        SelfUpdateError::CheckFailed(message) => error_response(
            StatusCode::BAD_GATEWAY,
            UpdateErrorCode::CheckFailed.as_str(),
            message,
        ),
        SelfUpdateError::Unavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Update records are temporarily unavailable.".into(),
        ),
    }
}

fn operator_required() -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "operator_required",
        "Operator privileges required.".into(),
    )
}

fn is_operator(capabilities: Option<Extension<WebUiV2Capabilities>>) -> bool {
    capabilities.is_some_and(|Extension(caps)| caps.operator_webui_config)
}

async fn get_status(
    State(service): State<Arc<dyn SelfUpdateService>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    match service.status().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => service_error(error),
    }
}

async fn check_update(
    State(service): State<Arc<dyn SelfUpdateService>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    match service.check().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => service_error(error),
    }
}

async fn start_update(
    State(service): State<Arc<dyn SelfUpdateService>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    match service.start().await {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err(error) => service_error(error),
    }
}

async fn enable_update(
    State(service): State<Arc<dyn SelfUpdateService>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    match service.enable().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => service_error(error),
    }
}

fn route_descriptors() -> Vec<IngressRouteDescriptor> {
    vec![
        descriptor(
            "webui.operator.update_status",
            NetworkMethod::Get,
            OPERATOR_UPDATE_PATH,
            STATUS_MAX_REQUESTS,
        ),
        descriptor(
            "webui.operator.update_check",
            NetworkMethod::Post,
            OPERATOR_UPDATE_CHECK_PATH,
            CHECK_MAX_REQUESTS,
        ),
        descriptor(
            "webui.operator.update_start",
            NetworkMethod::Post,
            OPERATOR_UPDATE_PATH,
            START_MAX_REQUESTS,
        ),
        descriptor(
            "webui.operator.update_enable",
            NetworkMethod::Post,
            OPERATOR_UPDATE_ENABLE_PATH,
            ENABLE_MAX_REQUESTS,
        ),
    ]
}

fn descriptor(
    route_id: &str,
    method: NetworkMethod,
    pattern: &str,
    max_requests: std::num::NonZeroU32,
) -> IngressRouteDescriptor {
    IngressRouteDescriptor::new(
        route_id.to_string(),
        method,
        pattern.to_string(),
        operator_policy(max_requests),
    )
    .expect("self-update route descriptor must validate at startup") // safety: ids/patterns are crate-local literals; the policy comes from operator_policy below.
}

fn operator_policy(max_requests: std::num::NonZeroU32) -> IngressPolicy {
    IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: IngressAuthPolicy::Required {
            schemes: vec![IngressAuthScheme::BearerToken],
        },
        scope_source: IngressScopeSource::AuthenticatedCaller,
        body_limit: BodyLimitPolicy::NoBody,
        rate_limit: RateLimitPolicy::Limited {
            scope: RateLimitScope::PerCaller,
            max_requests,
            window_seconds: RATE_WINDOW_SECONDS,
        },
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit: AuditTraceClass::UserAction,
        effect_path: AllowedEffectPath::ProductSurface,
    })
    .expect("self-update operator policy must validate") // safety: same authenticated local shape the environment-access operator routes use.
}
