//! The thread repository and console routes through the fully composed
//! `webui_v2_app`: operator-only (no bearer 401, a member's token 403 before
//! the surface is reached), each request shaped into its one
//! `ironclaw_product_contracts::thread_repo` descriptor, and a body that
//! tries to name a folder refused before the surface sees it.
//!
//! Crate tier on purpose: what git and the console do lives behind
//! `ProductSurface` (the orchestrator, with its own repository and console
//! tests, and the decorator's operator/ownership tests in
//! `ironclaw_assistant`); this tier proves the HTTP contract.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_product_contracts::thread_repo::{
    THREAD_CONSOLE_CONTROL_COMMAND_ID, THREAD_CONSOLE_VIEW, THREAD_GIT_CHANGE_COMMAND_ID,
    THREAD_GIT_VIEW,
};
use ironclaw_webui::{WebuiAuthentication, WebuiAuthenticator, WebuiServeConfig, webui_v2_app};
use serde_json::{Value, json};
use tower::ServiceExt;

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

/// Records every call with its caller and input; answers `{"ok": true}`.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, ProductSurfaceCaller, Value)>>,
}

impl Recorder {
    fn calls(&self) -> Vec<(String, ProductSurfaceCaller, Value)> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ProductSurface for Recorder {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        self.calls.lock().expect("lock").push((
            request.operation_id.as_str().to_string(),
            caller,
            request.input,
        ));
        Ok(ProductSurfaceInvokeResponse {
            output: json!({"ok": true}),
        })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        self.calls
            .lock()
            .expect("lock")
            .push((request.view_id, caller, request.input));
        Ok(ProductSurfaceQueryPage {
            items: vec![json!({"ok": true})],
            next_cursor: None,
        })
    }

    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

fn app(surface: Arc<Recorder>) -> Router {
    let config = WebuiServeConfig::new(
        TenantId::new("tenant.test").expect("tenant"),
        Arc::new(TwoBearers),
        vec![HeaderValue::from_static("http://localhost:1234")],
    )
    .with_default_agent_id(AgentId::new("agent.test").expect("agent"))
    .with_default_project_id(ProjectId::new("project.test").expect("project"));
    webui_v2_app(surface, config).expect("app composes")
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

const GIT: &str = "/api/webchat/v2/threads/chat-1/git";
const CONSOLE: &str = "/api/webchat/v2/threads/chat-1/console";

#[tokio::test]
async fn repository_and_console_routes_are_operator_only() {
    let surface = Arc::new(Recorder::default());
    let app = app(Arc::clone(&surface));
    let stage = json!({"action": "stage", "paths": ["a.txt"]});
    let start = json!({"action": "start", "command": "ls"});
    for (method, path, body) in [
        (Method::GET, GIT, None),
        (Method::POST, GIT, Some(stage)),
        (Method::GET, CONSOLE, None),
        (Method::POST, CONSOLE, Some(start)),
    ] {
        let (status, _) = send(&app, request(method.clone(), path, None, body.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, _) = send(&app, request(method.clone(), path, Some(MEMBER), body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} as a member");
    }
    assert!(surface.calls().is_empty(), "a member reached the surface");
}

#[tokio::test]
async fn each_route_calls_its_one_descriptor_for_the_operator() {
    let surface = Arc::new(Recorder::default());
    let app = app(Arc::clone(&surface));
    let cases = [
        (
            request(
                Method::GET,
                &format!("{GIT}?view=diff&scope=staged&path=src%2Fa.rs"),
                Some(OPERATOR),
                None,
            ),
            THREAD_GIT_VIEW.id,
            json!({"thread_id": "chat-1", "view": "diff", "scope": "staged", "path": "src/a.rs"}),
        ),
        (
            request(Method::GET, GIT, Some(OPERATOR), None),
            THREAD_GIT_VIEW.id,
            json!({"thread_id": "chat-1", "view": "status"}),
        ),
        (
            request(
                Method::POST,
                GIT,
                Some(OPERATOR),
                Some(json!({"action": "discard", "path": "a.txt"})),
            ),
            THREAD_GIT_CHANGE_COMMAND_ID,
            json!({"thread_id": "chat-1", "action": "discard", "path": "a.txt"}),
        ),
        (
            request(
                Method::GET,
                &format!("{CONSOLE}?after=7"),
                Some(OPERATOR),
                None,
            ),
            THREAD_CONSOLE_VIEW.id,
            json!({"thread_id": "chat-1", "after": 7}),
        ),
        (
            request(
                Method::POST,
                CONSOLE,
                Some(OPERATOR),
                Some(json!({"action": "cancel"})),
            ),
            THREAD_CONSOLE_CONTROL_COMMAND_ID,
            json!({"thread_id": "chat-1", "action": "cancel"}),
        ),
    ];
    for (n, (request, id, input)) in cases.into_iter().enumerate() {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::OK, "case {n}: {body}");
        let calls = surface.calls();
        let (called, caller, got) = calls.last().expect("a call");
        assert_eq!(called, id, "case {n}");
        assert_eq!(got, &input, "case {n}");
        assert!(
            caller.operator_config,
            "case {n}: the caller carries operator authority"
        );
    }
}

#[tokio::test]
async fn a_body_that_names_a_folder_is_refused_before_the_surface() {
    let surface = Arc::new(Recorder::default());
    let app = app(Arc::clone(&surface));
    for body in [
        json!({"action": "stage", "paths": ["a"], "repo": "/etc"}),
        json!({"action": "checkpoint", "workdir": "/"}),
        json!({"action": "rm", "path": "a"}),
    ] {
        let (status, _) = send(&app, request(Method::POST, GIT, Some(OPERATOR), Some(body))).await;
        assert!(status.is_client_error(), "{status}");
    }
    let (status, _) = send(
        &app,
        request(
            Method::POST,
            CONSOLE,
            Some(OPERATOR),
            Some(json!({"action": "start", "command": "ls", "cwd": "/"})),
        ),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    let (status, _) = send(
        &app,
        request(
            Method::GET,
            &format!("{GIT}?folder=/etc"),
            Some(OPERATOR),
            None,
        ),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    assert!(surface.calls().is_empty());
}
