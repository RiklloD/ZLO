//! The "Triage an issue" route through the fully composed `webui_v2_app`:
//! bearer auth, the operator check, the descriptor-driven body limit, the
//! client bound, and the HTTP mapping of the `TriageService` port.
//!
//! Crate tier on purpose: the port's real implementation (session files,
//! source ref, background clone) lives in the `ironclaw` binary with its own
//! tests; what only this tier proves is the HTTP contract.

#![deny(clippy::let_underscore_must_use)]
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_product_contracts::triage::{
    OPERATOR_TRIAGE_PATH, PrepareTriageResponse, TRIAGE_PROMPT_FILE_NAME, TriageClient,
};
use ironclaw_webui::{
    TriageError, TriageService, WebuiAuthentication, WebuiAuthenticator, WebuiServeConfig,
    triage_route_mount, webui_v2_app,
};
use serde_json::{Value, json};
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

/// Records every client it is handed; fails when told to.
#[derive(Default)]
struct FakeTriage {
    calls: Mutex<Vec<TriageClient>>,
    fail: bool,
}

#[async_trait]
impl TriageService for FakeTriage {
    async fn prepare(&self, client: TriageClient) -> Result<PrepareTriageResponse, TriageError> {
        self.calls.lock().expect("lock").push(client);
        if self.fail {
            return Err(TriageError::SessionUnwritable("disk full".into()));
        }
        Ok(PrepareTriageResponse {
            prompt: format!(
                "Read [@{TRIAGE_PROMPT_FILE_NAME}](file:///home/u/.zlo/triage/t/{TRIAGE_PROMPT_FILE_NAME}) and follow it. "
            ),
            server_version: "1.2.0".into(),
        })
    }
}

fn app(service: Arc<FakeTriage>) -> Router {
    let config = WebuiServeConfig::new(
        TenantId::new(TENANT).expect("tenant"),
        Arc::new(TwoBearers),
        vec![HeaderValue::from_static("http://localhost:1234")],
    )
    .with_default_agent_id(AgentId::new(AGENT).expect("agent"))
    .with_default_project_id(ProjectId::new(PROJECT).expect("project"))
    .with_protected_route_mount(triage_route_mount(service));
    webui_v2_app(Arc::new(StubServices::default()), config).expect("app composes")
}

fn request(bearer: Option<&str>, body: Option<String>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(OPERATOR_TRIAGE_PATH);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(text) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(text)
        }
        None => Body::empty(),
    };
    let mut request = builder.body(body).expect("request");
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))));
    request
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn body(value: Value) -> Option<String> {
    Some(value.to_string())
}

#[tokio::test]
async fn triage_is_operator_only() {
    let service = Arc::new(FakeTriage::default());
    let app = app(Arc::clone(&service));
    let (status, _) = send(&app, request(None, body(json!({})))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, reply) = send(&app, request(Some(MEMBER), body(json!({})))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(reply["error"], "operator_required");
    assert!(service.calls.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn prepare_hands_the_bounded_client_to_the_port_and_answers_the_prompt() {
    let service = Arc::new(FakeTriage::default());
    let app = app(Arc::clone(&service));
    let (status, reply) = send(
        &app,
        request(
            Some(OPERATOR),
            body(json!({"client": {
                "label": "ZLO\ndesktop",
                "app_version": "0.1.0",
                "os": "x".repeat(500),
            }})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let prepared: PrepareTriageResponse = serde_json::from_value(reply).expect("shape");
    assert!(prepared.prompt.contains(TRIAGE_PROMPT_FILE_NAME));
    assert!(prepared.prompt.ends_with(' '), "ready for the description");
    let calls = service.calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].label.as_deref(), Some("ZLO desktop"));
    assert_eq!(calls[0].app_version.as_deref(), Some("0.1.0"));
    assert_eq!(calls[0].os.as_ref().map(|os| os.chars().count()), Some(80));

    // An empty object is a request with no client facts.
    let (status, _) = send(&app, request(Some(OPERATOR), body(json!({})))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn malformed_and_oversized_bodies_never_reach_the_port() {
    let service = Arc::new(FakeTriage::default());
    let app = app(Arc::clone(&service));
    let (status, reply) = send(&app, request(Some(OPERATOR), Some("{not json".into()))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    let huge = json!({"client": {"label": "x".repeat(8 * 1024)}});
    let (status, _) = send(&app, request(Some(OPERATOR), body(huge))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(service.calls.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn an_unwritable_session_says_to_use_the_terminal() {
    let service = Arc::new(FakeTriage {
        fail: true,
        ..FakeTriage::default()
    });
    let app = app(service);
    let (status, reply) = send(&app, request(Some(OPERATOR), body(json!({})))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply["error"], "triage_unavailable");
    assert!(
        reply["message"]
            .as_str()
            .is_some_and(|m| m.contains("zlo triage"))
    );
    assert!(
        !reply.to_string().contains("disk full"),
        "the cause stays server-side"
    );
}
