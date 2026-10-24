//! The loopback MCP route coding-agent CLIs reach the orchestrator tools
//! through. One route, one bearer token per CLI process (minted by the
//! driver, revoked at turn end); loopback peers only. The JSON-RPC handling
//! and the call's thread scope live in `ironclaw_cli_providers::bridge`.
//!
//! Beside it, the loopback webhook route of `t3_schedule` webhook schedules
//! (`ironclaw_orchestrator::schedule_webhooks` decides everything: token,
//! signature, prompt scan, run).

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ironclaw_cli_providers::bridge::{CliBridge, serve_mcp};
use ironclaw_host_api::ingress::{
    AllowedEffectPath, AuditTraceClass, BodyLimitPolicy, CorsPolicy, IngressAuthPolicy,
    IngressAuthScheme, IngressPolicy, IngressPolicyParts, IngressRouteDescriptor,
    IngressScopeSource, ListenerClass, RateLimitPolicy, RateLimitScope, StreamingMode,
    WebSocketOriginPolicy,
};
use ironclaw_host_api::{action::NetworkMethod, host_port::HostPortId};
use ironclaw_host_ingress::PublicRouteMount;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::schedule_webhooks::{
    MAX_WEBHOOK_BODY_BYTES, WEBHOOK_PATH_PREFIX, WebhookRequest,
};

use crate::RebornBuildError;

pub(crate) const CLI_BRIDGE_MCP_PATH: &str = "/api/cli-bridge/mcp";
const CLI_BRIDGE_MCP_ROUTE_ID: &str = "cli_bridge.mcp";
const WEBHOOK_ROUTE_ID: &str = "orchestrator.schedule_webhook";
const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct CliBridgeRouteState {
    bridge: Arc<CliBridge>,
    engine: Arc<Engine>,
}

impl CliBridgeRouteState {
    pub(crate) fn new(bridge: Arc<CliBridge>, engine: Arc<Engine>) -> Self {
        Self { bridge, engine }
    }
}

/// The mount, and the bridge learns the route's loopback URL so the next
/// CLI turn gets an `--mcp-config`.
pub(crate) fn cli_bridge_route_mount(
    state: CliBridgeRouteState,
    listener: SocketAddr,
) -> Result<PublicRouteMount, RebornBuildError> {
    let port = listener.port();
    state
        .bridge
        .set_mcp_url(format!("http://127.0.0.1:{port}{CLI_BRIDGE_MCP_PATH}"));
    state
        .engine
        .set_webhook_base(format!("http://127.0.0.1:{port}"));
    let webhook_path = format!("{WEBHOOK_PATH_PREFIX}{{key}}");
    Ok(PublicRouteMount::new(
        Router::new()
            .route(CLI_BRIDGE_MCP_PATH, post(cli_bridge_mcp_handler))
            .route(&webhook_path, post(schedule_webhook_handler))
            .with_state(state),
        vec![
            IngressRouteDescriptor::new(
                CLI_BRIDGE_MCP_ROUTE_ID,
                NetworkMethod::Post,
                CLI_BRIDGE_MCP_PATH,
                cli_bridge_policy()?,
            )?,
            IngressRouteDescriptor::new(
                WEBHOOK_ROUTE_ID,
                NetworkMethod::Post,
                &webhook_path,
                cli_bridge_policy()?,
            )?,
        ],
    ))
}

fn cli_bridge_policy() -> Result<IngressPolicy, RebornBuildError> {
    let invalid = |what: &str| RebornBuildError::InvalidConfig {
        reason: format!("CLI bridge {what} must be non-zero"),
    };
    Ok(IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::InternalWorker,
        auth: IngressAuthPolicy::Required {
            schemes: vec![IngressAuthScheme::InternalToken],
        },
        scope_source: IngressScopeSource::HostResolved,
        body_limit: BodyLimitPolicy::Limited {
            max_bytes: NonZeroU64::new(MAX_BODY_BYTES as u64)
                .ok_or_else(|| invalid("body limit"))?,
        },
        rate_limit: RateLimitPolicy::Limited {
            scope: RateLimitScope::Global,
            max_requests: NonZeroU32::new(600).ok_or_else(|| invalid("rate limit"))?,
            window_seconds: NonZeroU32::new(60).ok_or_else(|| invalid("rate window"))?,
        },
        cors: CorsPolicy::NotApplicable,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit: AuditTraceClass::InternalControl,
        effect_path: AllowedEffectPath::HostPort {
            id: HostPortId::new("host.orchestrator.cli_bridge")?,
        },
    })?)
}

async fn cli_bridge_mcp_handler(
    State(state): State<CliBridgeRouteState>,
    request: Request,
) -> Response {
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
    let (parts, body) = request.into_parts();
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    // The descriptor's body limit already ran; this bound is the same.
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    match serve_mcp(&state.bridge, &state.engine, loopback, authorization, &body).await {
        Ok(Some(reply)) => Json(reply).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(status) => StatusCode::from_u16(status)
            .unwrap_or(StatusCode::FORBIDDEN)
            .into_response(),
    }
}

/// One webhook schedule run per request; loopback peers only, the URL's
/// token (and the schedule's signature, if any) authenticate it.
async fn schedule_webhook_handler(
    State(state): State<CliBridgeRouteState>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
    if !loopback {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_WEBHOOK_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let webhook = WebhookRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        headers: parts
            .headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect(),
        query: url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect(),
        body: body.to_vec(),
    };
    match state.engine.fire_webhook(&key, &webhook).await {
        Ok(reply) => (StatusCode::ACCEPTED, Json(reply)).into_response(),
        Err(refusal) => StatusCode::from_u16(refusal.status())
            .unwrap_or(StatusCode::NOT_FOUND)
            .into_response(),
    }
}

#[cfg(test)]
#[path = "cli_bridge_serve_tests.rs"]
mod tests;
