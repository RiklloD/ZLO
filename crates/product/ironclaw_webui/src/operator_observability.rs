//! Operator observability routes: readiness and the metrics scrape.
//!
//! - `GET /api/ready` — **public**, rate-limited per peer. `200` when the
//!   host's composed readiness rolls up to `ready`, `503` otherwise. The body
//!   carries only the stable readiness vocabulary — the roll-up and each
//!   check's id, state and severity — never the checks' prose, remediation,
//!   host paths, backend URLs, or secrets. A host that supplied no readiness
//!   snapshot answers `503` with `overall: "not_configured"`. `/api/health`
//!   stays the always-healthy liveness probe. The snapshot is the
//!   **boot-time configuration readiness** composition computed while
//!   assembling the runtime — never re-evaluated, so dependencies failing
//!   after boot do not change the answer, and development profiles (which
//!   roll up to `degraded`) always answer `503`.
//! - `GET /api/operator/metrics` — bearer-authenticated, **operator-only**,
//!   Prometheus text exposition rendered by the
//!   [`OperatorMetricsService`] port (implemented by `ironclaw_operator`).
//!
//! Both are descriptor-backed (body limit, rate limit, and — for the scrape —
//! bearer auth run before the handler) and take no body.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ingress::{
    AllowedEffectPath, AuditTraceClass, BodyLimitPolicy, CorsPolicy, IngressAuthPolicy,
    IngressAuthScheme, IngressJustification, IngressPolicy, IngressPolicyParts,
    IngressRouteDescriptor, IngressScopeSource, ListenerClass, RateLimitPolicy, RateLimitScope,
    StreamingMode, WebSocketOriginPolicy,
};
use ironclaw_host_ingress::{ProtectedRouteMount, PublicRouteMount};
use ironclaw_product_contracts::operator_service::OperatorMetricsService;
use ironclaw_product_contracts::product_wire::{
    RebornOperatorStatusResponse, RebornOperatorStatusSeverity, RebornOperatorStatusState,
};
use serde::Serialize;

use crate::webui_v2::WebUiV2Capabilities;

pub const READINESS_PATH: &str = "/api/ready";
pub const OPERATOR_METRICS_PATH: &str = "/api/operator/metrics";

const RATE_WINDOW_SECONDS: std::num::NonZeroU32 = std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero
/// Orchestrator probes run every few seconds, possibly several per peer.
const READINESS_MAX_REQUESTS: std::num::NonZeroU32 =
    std::num::NonZeroU32::new(120).expect("120 != 0"); // safety: const-evaluated, literal non-zero
/// Prometheus scrapes every 15–60 s; leave room for ad-hoc reads.
const METRICS_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
/// Longest check id echoed; ids are stable snake_case composition literals.
const MAX_CHECK_ID_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReadinessVerdict {
    Ready,
    NotReady,
}

#[derive(Debug, Serialize)]
struct ReadinessCheckBody {
    id: String,
    status: RebornOperatorStatusState,
    severity: RebornOperatorStatusSeverity,
}

#[derive(Debug, Serialize)]
struct ReadinessBody {
    status: ReadinessVerdict,
    overall: RebornOperatorStatusState,
    checks: Vec<ReadinessCheckBody>,
}

/// Public readiness mount over the host's composed readiness snapshot.
pub(crate) fn readiness_route_mount(
    readiness: Option<RebornOperatorStatusResponse>,
) -> PublicRouteMount {
    let router = Router::new()
        .route(READINESS_PATH, get(get_readiness))
        .with_state(Arc::new(readiness));
    PublicRouteMount::new(router, vec![readiness_descriptor()])
}

async fn get_readiness(
    State(readiness): State<Arc<Option<RebornOperatorStatusResponse>>>,
) -> Response {
    let body = readiness_body(readiness.as_ref().as_ref());
    let status = match body.status {
        ReadinessVerdict::Ready => StatusCode::OK,
        ReadinessVerdict::NotReady => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(body)).into_response()
}

fn readiness_body(readiness: Option<&RebornOperatorStatusResponse>) -> ReadinessBody {
    let Some(readiness) = readiness else {
        return ReadinessBody {
            status: ReadinessVerdict::NotReady,
            overall: RebornOperatorStatusState::NotConfigured,
            checks: Vec::new(),
        };
    };
    let status = if readiness.overall == RebornOperatorStatusState::Ready {
        ReadinessVerdict::Ready
    } else {
        ReadinessVerdict::NotReady
    };
    ReadinessBody {
        status,
        overall: readiness.overall,
        checks: readiness
            .checks
            .iter()
            .map(|check| ReadinessCheckBody {
                id: public_check_id(&check.id),
                status: check.status,
                severity: check.severity,
            })
            .collect(),
    }
}

/// Check ids are composition literals (`runtime`, `readiness_<component>`),
/// but this route is public, so anything outside the lowercase snake-case
/// vocabulary is replaced rather than echoed.
fn public_check_id(id: &str) -> String {
    let well_formed = !id.is_empty()
        && id.chars().count() <= MAX_CHECK_ID_CHARS
        && id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        });
    if well_formed {
        id.to_string()
    } else {
        "unrecognized_check".to_string()
    }
}

/// Bearer-authed, operator-only Prometheus scrape over `service`.
pub fn operator_metrics_route_mount(
    service: Arc<dyn OperatorMetricsService>,
) -> ProtectedRouteMount {
    let router = Router::new()
        .route(OPERATOR_METRICS_PATH, get(get_metrics))
        .with_state(service);
    ProtectedRouteMount::new(router, vec![metrics_descriptor()])
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: &'static str,
}

async fn get_metrics(
    State(service): State<Arc<dyn OperatorMetricsService>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
) -> Response {
    let is_operator = capabilities.is_some_and(|Extension(caps)| caps.operator_webui_config);
    if !is_operator {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody {
                error: "operator_required",
                message: "Operator privileges required.",
            }),
        )
            .into_response();
    }
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(PROMETHEUS_CONTENT_TYPE),
        )],
        service.render_prometheus(),
    )
        .into_response()
}

fn readiness_descriptor() -> IngressRouteDescriptor {
    let policy = IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: IngressAuthPolicy::Public {
            justification: IngressJustification::new(
                "webui readiness probe",
                "container orchestrators probe readiness without credentials; the \
                 response carries only the stable readiness vocabulary (check ids, \
                 states, severities) and never prose, paths, URLs, or secrets",
            )
            .expect("readiness justification literal must validate"), // safety: non-empty, no leading/trailing whitespace.
        },
        scope_source: IngressScopeSource::PublicRoute,
        body_limit: BodyLimitPolicy::NoBody,
        rate_limit: RateLimitPolicy::Limited {
            scope: RateLimitScope::PerIp,
            max_requests: READINESS_MAX_REQUESTS,
            window_seconds: RATE_WINDOW_SECONDS,
        },
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit: AuditTraceClass::HealthCheck,
        effect_path: AllowedEffectPath::NoEffect,
    })
    .expect("readiness policy must validate"); // safety: LocalGateway + Public + NoEffect is permitted; rate-limit window/max are non-zero by construction.
    IngressRouteDescriptor::new(
        "webui.readiness".to_string(),
        NetworkMethod::Get,
        READINESS_PATH.to_string(),
        policy,
    )
    .expect("readiness route descriptor must validate at startup") // safety: id/pattern are crate-local literals.
}

fn metrics_descriptor() -> IngressRouteDescriptor {
    let policy = IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: IngressAuthPolicy::Required {
            schemes: vec![IngressAuthScheme::BearerToken],
        },
        scope_source: IngressScopeSource::AuthenticatedCaller,
        body_limit: BodyLimitPolicy::NoBody,
        rate_limit: RateLimitPolicy::Limited {
            scope: RateLimitScope::PerCaller,
            max_requests: METRICS_MAX_REQUESTS,
            window_seconds: RATE_WINDOW_SECONDS,
        },
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit: AuditTraceClass::InternalControl,
        effect_path: AllowedEffectPath::NoEffect,
    })
    .expect("metrics policy must validate"); // safety: the authenticated local shape the self-update operator routes use, with no effect.
    IngressRouteDescriptor::new(
        "webui.operator.metrics".to_string(),
        NetworkMethod::Get,
        OPERATOR_METRICS_PATH.to_string(),
        policy,
    )
    .expect("metrics route descriptor must validate at startup") // safety: id/pattern are crate-local literals.
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ironclaw_product_contracts::product_wire::RebornOperatorStatusCheck;

    fn snapshot(overall: RebornOperatorStatusState) -> RebornOperatorStatusResponse {
        RebornOperatorStatusResponse {
            generated_at: Utc::now(),
            overall,
            checks: vec![
                RebornOperatorStatusCheck {
                    id: "runtime".to_string(),
                    status: overall,
                    severity: RebornOperatorStatusSeverity::Info,
                    summary: "postgres://admin:hunter2@db.internal/reborn".to_string(),
                    remediation: Some("edit /etc/ironclaw/secret.toml".to_string()),
                },
                RebornOperatorStatusCheck {
                    id: "Bad Id /etc/passwd".to_string(),
                    status: RebornOperatorStatusState::Ready,
                    severity: RebornOperatorStatusSeverity::Info,
                    summary: String::new(),
                    remediation: None,
                },
            ],
        }
    }

    #[test]
    fn readiness_body_drops_prose_and_replaces_unrecognized_ids() {
        let body = serde_json::to_value(readiness_body(Some(&snapshot(
            RebornOperatorStatusState::Degraded,
        ))))
        .expect("serializes");
        let text = body.to_string();
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["overall"], "degraded");
        assert_eq!(body["checks"][0]["id"], "runtime");
        assert_eq!(body["checks"][1]["id"], "unrecognized_check");
        for leaked in ["hunter2", "db.internal", "/etc/", "summary", "remediation"] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
    }

    #[test]
    fn missing_snapshot_is_not_ready_and_says_so() {
        let body = serde_json::to_value(readiness_body(None)).expect("serializes");
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["overall"], "not_configured");
    }
}
