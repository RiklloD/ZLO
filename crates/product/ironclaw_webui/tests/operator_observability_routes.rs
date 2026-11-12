//! `/api/ready` and `/api/operator/metrics` through the fully composed
//! `webui_v2_app`: the public readiness probe's status mapping and redaction,
//! the descriptor-driven per-peer rate limit and body refusal in front of it,
//! and the scrape route's bearer + operator gate.
//!
//! Crate tier on purpose: the readiness snapshot is composition's and the
//! metrics registry is `ironclaw_operator`'s, both with their own tests; what
//! only this tier proves is the HTTP contract of the two routes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use chrono::Utc;
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_product_contracts::operator_service::OperatorMetricsService;
use ironclaw_product_contracts::product_wire::{
    RebornOperatorStatusCheck, RebornOperatorStatusResponse, RebornOperatorStatusSeverity,
    RebornOperatorStatusState,
};
use ironclaw_webui::{
    OPERATOR_METRICS_PATH, READINESS_PATH, WebuiAuthentication, WebuiAuthenticator,
    WebuiServeConfig, operator_metrics_route_mount, webui_v2_app,
};
use serde_json::Value;
use tower::ServiceExt;

#[path = "support/harness.rs"]
mod harness;
use harness::{AGENT, PROJECT, StubServices, TENANT};

const OPERATOR: &str = "operator-token-0123456789";
const MEMBER: &str = "member-token-0123456789";

struct TwoBearers;

#[async_trait]
impl WebuiAuthenticator for TwoBearers {
    async fn authenticate(&self, token: &str) -> Option<WebuiAuthentication> {
        match token {
            OPERATOR => Some(WebuiAuthentication::operator(
                UserId::new("operator").expect("user"),
            )),
            MEMBER => Some(WebuiAuthentication::user(
                UserId::new("member").expect("user"),
            )),
            _ => None,
        }
    }

    fn mounts_operator_webui_config_routes(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct CountingMetrics {
    renders: AtomicUsize,
}

impl OperatorMetricsService for CountingMetrics {
    fn render_prometheus(&self) -> String {
        self.renders.fetch_add(1, Ordering::SeqCst);
        "# TYPE ironclaw_turn_runs_total counter\nironclaw_turn_runs_total{outcome=\"completed\"} 3\n"
            .to_string()
    }
}

fn snapshot(overall: RebornOperatorStatusState) -> RebornOperatorStatusResponse {
    RebornOperatorStatusResponse {
        generated_at: Utc::now(),
        overall,
        checks: vec![RebornOperatorStatusCheck {
            id: "runtime".to_string(),
            status: overall,
            severity: RebornOperatorStatusSeverity::Warning,
            summary: "storage at postgres://admin:hunter2@db.internal/reborn".to_string(),
            remediation: Some("edit /etc/ironclaw/secret.toml".to_string()),
        }],
    }
}

fn app(readiness: Option<RebornOperatorStatusResponse>, metrics: Arc<CountingMetrics>) -> Router {
    let mut config = WebuiServeConfig::new(
        TenantId::new(TENANT).expect("tenant"),
        Arc::new(TwoBearers),
        vec![HeaderValue::from_static("http://localhost:1234")],
    )
    .with_default_agent_id(AgentId::new(AGENT).expect("agent"))
    .with_default_project_id(ProjectId::new(PROJECT).expect("project"))
    .with_protected_route_mount(operator_metrics_route_mount(metrics));
    if let Some(readiness) = readiness {
        config = config.with_readiness(readiness);
    }
    webui_v2_app(Arc::new(StubServices::default()), config).expect("app composes")
}

fn request(method: Method, uri: &str, bearer: Option<&str>, peer: [u8; 4]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut request = builder.body(Body::empty()).expect("request");
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((peer, 4000))));
    request
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

const PEER: [u8; 4] = [10, 0, 0, 7];

#[tokio::test]
async fn readiness_is_200_only_when_the_roll_up_is_ready() {
    let metrics = Arc::new(CountingMetrics::default());
    let ready = app(
        Some(snapshot(RebornOperatorStatusState::Ready)),
        Arc::clone(&metrics),
    );
    let (status, _, body) = send(&ready, request(Method::GET, READINESS_PATH, None, PEER)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["status"], "ready");
    assert_eq!(body["overall"], "ready");
    assert_eq!(body["checks"][0]["id"], "runtime");

    for overall in [
        RebornOperatorStatusState::Degraded,
        RebornOperatorStatusState::Blocked,
        RebornOperatorStatusState::NotConfigured,
    ] {
        let app = app(Some(snapshot(overall)), Arc::clone(&metrics));
        let (status, _, body) = send(&app, request(Method::GET, READINESS_PATH, None, PEER)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{overall:?}: {body}"
        );
        let body: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(body["status"], "not_ready");
    }

    let unconfigured = app(None, metrics);
    let (status, _, body) = send(
        &unconfigured,
        request(Method::GET, READINESS_PATH, None, PEER),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["overall"], "not_configured");
}

#[tokio::test]
async fn readiness_body_carries_no_prose_secrets_or_backend_urls() {
    let app = app(
        Some(snapshot(RebornOperatorStatusState::Degraded)),
        Arc::new(CountingMetrics::default()),
    );
    let (_, _, body) = send(&app, request(Method::GET, READINESS_PATH, None, PEER)).await;
    for leaked in [
        "hunter2",
        "postgres://",
        "db.internal",
        "/etc/",
        "summary",
        "remediation",
    ] {
        assert!(!body.contains(leaked), "{leaked} leaked: {body}");
    }
}

#[tokio::test]
async fn readiness_rides_the_descriptor_stack_rate_limit_and_body_refusal() {
    let app = app(
        Some(snapshot(RebornOperatorStatusState::Ready)),
        Arc::new(CountingMetrics::default()),
    );
    let mut with_body = Request::builder()
        .method(Method::GET)
        .uri(READINESS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{\"probe\":true}"))
        .expect("request");
    with_body
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((PEER, 4000))));
    let (status, _, _) = send(&app, with_body).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a body is refused before the handler"
    );

    // 120 per minute per peer; the 121st from the same peer is limited while
    // another peer is unaffected.
    let limited_peer = [10, 0, 0, 8];
    for _ in 0..120 {
        let (status, _, _) = send(
            &app,
            request(Method::GET, READINESS_PATH, None, limited_peer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _, _) = send(
        &app,
        request(Method::GET, READINESS_PATH, None, limited_peer),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let (status, _, _) = send(&app, request(Method::GET, READINESS_PATH, None, PEER)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn health_stays_always_healthy_beside_readiness() {
    let app = app(
        Some(snapshot(RebornOperatorStatusState::Blocked)),
        Arc::new(CountingMetrics::default()),
    );
    let (status, _, body) = send(&app, request(Method::GET, "/api/health", None, PEER)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("healthy"));
}

#[tokio::test]
async fn metrics_scrape_is_bearer_and_operator_only() {
    let metrics = Arc::new(CountingMetrics::default());
    let app = app(None, Arc::clone(&metrics));

    let (status, _, _) = send(
        &app,
        request(Method::GET, OPERATOR_METRICS_PATH, None, PEER),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, body) = send(
        &app,
        request(Method::GET, OPERATOR_METRICS_PATH, Some(MEMBER), PEER),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("operator_required"));
    assert_eq!(
        metrics.renders.load(Ordering::SeqCst),
        0,
        "no refused scrape reached the registry"
    );

    let (status, content_type, body) = send(
        &app,
        request(Method::GET, OPERATOR_METRICS_PATH, Some(OPERATOR), PEER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_type.as_deref(),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    assert!(body.contains("ironclaw_turn_runs_total{outcome=\"completed\"} 3"));
    assert_eq!(metrics.renders.load(Ordering::SeqCst), 1);
}
