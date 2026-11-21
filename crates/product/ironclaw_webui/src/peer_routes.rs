//! Routes for several machines working together
//! (`ironclaw_product_contracts::peer`).
//!
//! Receiving side (a linked environment calls in with its peer-link token):
//! - `POST /api/peer/v1/tools/{tool}` — run one orchestrator tool here. The
//!   caller must hold a peer-link grant; its access is narrowed by the
//!   calling agent's own mode (`mode_limit`), never widened.
//! - `GET /api/peer/v1/resources` — CPU/memory headroom, for load balancing.
//!
//! Linking side (this machine's own clients, `access:manage`):
//! - `GET|POST /api/peer-links`, `DELETE /api/peer-links/{environment_id}`,
//!   `POST /api/peer-links/{environment_id}/preference`.
//!
//! Every client of this machine (any device it paired):
//! - `GET /api/agent-requests`, `POST /api/agent-requests/{request_id}` —
//!   secret and link requests agents raised, answerable from any device.
//!   Answering a link request also needs `access:manage`.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ingress::BodyLimitPolicy;
use ironclaw_host_ingress::SplitRouteMount;
use ironclaw_product_contracts::environment_access::DeviceScope;
use ironclaw_product_contracts::peer::{
    AGENT_REQUEST_PATH, AGENT_REQUESTS_PATH, AgentRequestAnswer, AgentRequestError,
    AgentRequestService, LinkEnvironmentRequest, PEER_LINK_PATH, PEER_LINK_PREFERENCE_PATH,
    PEER_LINKS_PATH, PEER_RESOURCES_PATH, PEER_TOOL_PATH, PeerCaller, PeerLinkError, PeerLinkList,
    PeerLinkPreferenceRequest, PeerLinkService, PeerToolHandler, PeerToolRequest,
};
use serde::Serialize;

use crate::device_scopes::{CallerGrant, scope_forbidden};
use crate::environment_access::{descriptor, operator_policy};

const TOOL_BODY_LIMIT: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(512 * 1024).expect("512 KiB != 0"); // safety: const-evaluated, literal non-zero
const SMALL_BODY_LIMIT: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(80 * 1024).expect("80 KiB != 0"); // safety: const-evaluated, literal non-zero
const PEER_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(600).expect("600 != 0"); // safety: const-evaluated, literal non-zero
const READ_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(240).expect("240 != 0"); // safety: const-evaluated, literal non-zero
const MUTATION_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero

/// What the multi-machine routes call. A `None` part answers `404`.
#[derive(Clone, Default)]
pub struct PeerRouteConfig {
    pub tools: Option<Arc<dyn PeerToolHandler>>,
    pub links: Option<Arc<dyn PeerLinkService>>,
    pub requests: Option<Arc<dyn AgentRequestService>>,
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

fn error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: code,
            message: message.into(),
        }),
    )
        .into_response()
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "Not found.")
}

fn bad_body() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "The request body is not valid.",
    )
}

fn link_error(e: PeerLinkError) -> Response {
    let status = match &e {
        PeerLinkError::NotLinked(_) => StatusCode::NOT_FOUND,
        PeerLinkError::Revoked(_) | PeerLinkError::Refused(_) => StatusCode::FORBIDDEN,
        PeerLinkError::Unreachable(_) => StatusCode::BAD_GATEWAY,
        PeerLinkError::Invalid(_) => StatusCode::BAD_REQUEST,
        PeerLinkError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    error(status, e.code(), e.to_string())
}

fn request_error(e: AgentRequestError) -> Response {
    let (status, code) = match &e {
        AgentRequestError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        AgentRequestError::Closed => (StatusCode::CONFLICT, "request_closed"),
        AgentRequestError::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_answer"),
        AgentRequestError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    error(status, code, e.to_string())
}

fn grant_of(grant: Option<Extension<CallerGrant>>) -> CallerGrant {
    grant.map(|Extension(g)| g).unwrap_or(CallerGrant(None))
}

/// Build the mount (every route bearer-authenticated).
pub fn peer_route_mount(config: PeerRouteConfig) -> SplitRouteMount {
    let state = Arc::new(config);
    let protected = Router::new()
        .route(PEER_TOOL_PATH, post(peer_tool))
        .route(PEER_RESOURCES_PATH, get(peer_resources))
        .route(PEER_LINKS_PATH, get(list_links).post(link))
        .route(PEER_LINK_PATH, delete(forget_link))
        .route(PEER_LINK_PREFERENCE_PATH, post(set_preference))
        .route(AGENT_REQUESTS_PATH, get(list_requests))
        .route(AGENT_REQUEST_PATH, post(answer_request))
        .with_state(state);
    SplitRouteMount::new(protected, Router::new(), route_descriptors())
}

async fn peer_tool(
    State(state): State<Arc<PeerRouteConfig>>,
    grant: Option<Extension<CallerGrant>>,
    Path(tool): Path<String>,
    body: Result<Json<PeerToolRequest>, JsonRejection>,
) -> Response {
    let Some(tools) = state.tools.as_ref() else {
        return not_found();
    };
    let Some(device) = grant_of(grant).0 else {
        return error(
            StatusCode::FORBIDDEN,
            "peer_link_required",
            "Only a linked environment calls this route.",
        );
    };
    let Some(link_access) = device.peer_link else {
        return error(
            StatusCode::FORBIDDEN,
            "peer_link_required",
            "This device is not a peer link. Link the environment instead.",
        );
    };
    let Ok(Json(request)) = body else {
        return bad_body();
    };
    if !tool.starts_with("t3_") || tool.len() > 64 {
        return not_found();
    }
    let caller = PeerCaller {
        device_id: device.device_id.clone(),
        label: device.label.clone(),
        // The agent's own mode only ever narrows the link's access.
        access: link_access.min(request.mode_limit),
        caller_key: request.caller_key.chars().take(64).collect(),
    };
    Json(tools.call_peer_tool(caller, &tool, request.input).await).into_response()
}

async fn peer_resources(
    State(state): State<Arc<PeerRouteConfig>>,
    grant: Option<Extension<CallerGrant>>,
) -> Response {
    let Some(tools) = state.tools.as_ref() else {
        return not_found();
    };
    let grant = grant_of(grant);
    let peer = grant.0.as_ref().is_some_and(|g| g.peer_link.is_some());
    if !peer && !grant.allows(DeviceScope::OrchestrationRead) {
        return scope_forbidden(DeviceScope::OrchestrationRead);
    }
    Json(tools.resources().await).into_response()
}

async fn list_links(State(state): State<Arc<PeerRouteConfig>>) -> Response {
    let Some(links) = state.links.as_ref() else {
        return not_found();
    };
    match links.links().await {
        Ok(links) => Json(PeerLinkList { links }).into_response(),
        Err(e) => link_error(e),
    }
}

async fn link(
    State(state): State<Arc<PeerRouteConfig>>,
    grant: Option<Extension<CallerGrant>>,
    body: Result<Json<LinkEnvironmentRequest>, JsonRejection>,
) -> Response {
    let Some(links) = state.links.as_ref() else {
        return not_found();
    };
    if grant_of(grant).0.is_some_and(|g| g.peer_link.is_some()) {
        return error(
            StatusCode::FORBIDDEN,
            "peer_link_refused",
            "A linked environment cannot use this machine's own links.",
        );
    }
    let Ok(Json(request)) = body else {
        return bad_body();
    };
    match links.link(request).await {
        Ok(link) => Json(link).into_response(),
        Err(e) => link_error(e),
    }
}

async fn forget_link(
    State(state): State<Arc<PeerRouteConfig>>,
    Path(environment_id): Path<String>,
) -> Response {
    let Some(links) = state.links.as_ref() else {
        return not_found();
    };
    match links.forget(&environment_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => link_error(e),
    }
}

async fn set_preference(
    State(state): State<Arc<PeerRouteConfig>>,
    Path(environment_id): Path<String>,
    body: Result<Json<PeerLinkPreferenceRequest>, JsonRejection>,
) -> Response {
    let Some(links) = state.links.as_ref() else {
        return not_found();
    };
    let Ok(Json(request)) = body else {
        return bad_body();
    };
    match links
        .set_preference(&environment_id, request.preference)
        .await
    {
        Ok(link) => Json(link).into_response(),
        Err(e) => link_error(e),
    }
}

async fn list_requests(State(state): State<Arc<PeerRouteConfig>>) -> Response {
    let Some(requests) = state.requests.as_ref() else {
        return not_found();
    };
    match requests.pending_requests().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => request_error(e),
    }
}

async fn answer_request(
    State(state): State<Arc<PeerRouteConfig>>,
    grant: Option<Extension<CallerGrant>>,
    Path(request_id): Path<String>,
    body: Result<Json<AgentRequestAnswer>, JsonRejection>,
) -> Response {
    let Some(requests) = state.requests.as_ref() else {
        return not_found();
    };
    let Ok(Json(answer)) = body else {
        return bad_body();
    };
    let grant = grant_of(grant);
    if grant.0.as_ref().is_some_and(|g| g.peer_link.is_some()) {
        return error(
            StatusCode::FORBIDDEN,
            "peer_link_refused",
            "Only this machine's own devices answer its requests.",
        );
    }
    if matches!(answer, AgentRequestAnswer::Link { .. }) && !grant.allows(DeviceScope::AccessManage)
    {
        return scope_forbidden(DeviceScope::AccessManage);
    }
    match requests.answer_request(&request_id, answer).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => request_error(e),
    }
}

fn route_descriptors() -> Vec<ironclaw_host_api::ingress::IngressRouteDescriptor> {
    let limited = |max_bytes| BodyLimitPolicy::Limited { max_bytes };
    vec![
        descriptor(
            "webui.peer.tool",
            NetworkMethod::Post,
            PEER_TOOL_PATH,
            operator_policy(limited(TOOL_BODY_LIMIT), PEER_MAX_REQUESTS),
        ),
        descriptor(
            "webui.peer.resources",
            NetworkMethod::Get,
            PEER_RESOURCES_PATH,
            operator_policy(BodyLimitPolicy::NoBody, READ_MAX_REQUESTS),
        ),
        descriptor(
            "webui.peer.links_list",
            NetworkMethod::Get,
            PEER_LINKS_PATH,
            operator_policy(BodyLimitPolicy::NoBody, READ_MAX_REQUESTS),
        ),
        descriptor(
            "webui.peer.link",
            NetworkMethod::Post,
            PEER_LINKS_PATH,
            operator_policy(limited(SMALL_BODY_LIMIT), MUTATION_MAX_REQUESTS),
        ),
        descriptor(
            "webui.peer.link_forget",
            NetworkMethod::Delete,
            PEER_LINK_PATH,
            operator_policy(BodyLimitPolicy::NoBody, MUTATION_MAX_REQUESTS),
        ),
        descriptor(
            "webui.peer.link_preference",
            NetworkMethod::Post,
            PEER_LINK_PREFERENCE_PATH,
            operator_policy(limited(SMALL_BODY_LIMIT), MUTATION_MAX_REQUESTS),
        ),
        descriptor(
            "webui.agent_requests.list",
            NetworkMethod::Get,
            AGENT_REQUESTS_PATH,
            operator_policy(BodyLimitPolicy::NoBody, READ_MAX_REQUESTS),
        ),
        descriptor(
            "webui.agent_requests.answer",
            NetworkMethod::Post,
            AGENT_REQUEST_PATH,
            operator_policy(limited(SMALL_BODY_LIMIT), MUTATION_MAX_REQUESTS),
        ),
    ]
}
