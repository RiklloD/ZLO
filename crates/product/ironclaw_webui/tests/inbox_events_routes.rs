//! Route, authentication, device-scope, and SSE delivery contract for
//! `GET /api/webchat/v2/inbox/events` through the composed `webui_v2_app`.

#![deny(clippy::let_underscore_must_use)]
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_product_contracts::environment_access::{DeviceGrant, DeviceId, DeviceScope};
use ironclaw_product_contracts::inbox::{
    INBOX_CHANGED_EVENT_KIND, INBOX_EVENTS_STREAM_ID, InboxChangedEvent,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceEventSubscription,
    ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse, ProductSurfaceQueryPage,
    ProductSurfaceQueryRequest, ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_webui::{WebuiAuthentication, WebuiAuthenticator, WebuiServeConfig, webui_v2_app};
use tokio::sync::mpsc;
use tower::ServiceExt;

const OPERATOR_TOKEN: &str = "operator-token-0123456789";
const READ_DEVICE_TOKEN: &str = "device-read-token-0123456";
const FS_ONLY_DEVICE_TOKEN: &str = "device-fs-only-token-0123";

struct ScopedAuth;

fn grant(id: &str, scope: DeviceScope) -> DeviceGrant {
    DeviceGrant {
        device_id: DeviceId::new(id).expect("device id"),
        label: id.to_string(),
        scopes: vec![scope],
        peer_link: None,
    }
}

#[async_trait]
impl WebuiAuthenticator for ScopedAuth {
    async fn authenticate(&self, token: &str) -> Option<WebuiAuthentication> {
        let user = UserId::new("alice").expect("user");
        match token {
            OPERATOR_TOKEN => Some(WebuiAuthentication::operator(user)),
            READ_DEVICE_TOKEN => {
                Some(WebuiAuthentication::operator(user).with_device_grant(grant(
                    "0123456789abcdef0123456789abcde1",
                    DeviceScope::OrchestrationRead,
                )))
            }
            FS_ONLY_DEVICE_TOKEN => {
                Some(WebuiAuthentication::operator(user).with_device_grant(grant(
                    "0123456789abcdef0123456789abcde2",
                    DeviceScope::FilesystemRead,
                )))
            }
            _ => None,
        }
    }

    fn mounts_operator_webui_config_routes(&self) -> bool {
        true
    }
}

type Hint = Result<ProductSurfaceStreamResponse, ProductSurfaceError>;

struct WiredSurface {
    rx: tokio::sync::Mutex<Option<mpsc::Receiver<Hint>>>,
}

#[async_trait]
impl ProductSurface for WiredSurface {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }

    async fn query(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }

    async fn stream_events(
        &self,
        _caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        assert_eq!(request.stream_id.as_deref(), Some(INBOX_EVENTS_STREAM_ID));
        let rx = self
            .rx
            .lock()
            .await
            .take()
            .expect("single stream open per test");
        Ok(ProductSurfaceStreamResponse {
            events: Vec::new(),
            next_cursor: None,
            subscription: Some(ProductSurfaceEventSubscription::new(rx)),
        })
    }
}

fn app_with_stream() -> (Router, mpsc::Sender<Hint>) {
    let (tx, rx) = mpsc::channel(4);
    let surface = Arc::new(WiredSurface {
        rx: tokio::sync::Mutex::new(Some(rx)),
    });
    let config = WebuiServeConfig::new(
        TenantId::new("tenant-test").expect("tenant"),
        Arc::new(ScopedAuth),
        vec![HeaderValue::from_static("http://localhost:1234")],
    )
    .with_default_agent_id(AgentId::new("agent-test").expect("agent"))
    .with_default_project_id(ProjectId::new("project-test").expect("project"));
    let router = webui_v2_app(surface, config).expect("app composes");
    (router, tx)
}

fn req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut request = builder.body(Body::empty()).expect("request");
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40_001))));
    request
}

#[tokio::test]
async fn unauthenticated_request_is_rejected_with_401() {
    let (router, _tx) = app_with_stream();
    let response = router
        .oneshot(req("/api/webchat/v2/inbox/events", None))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn device_token_without_orchestration_read_is_rejected_with_403() {
    let (router, _tx) = app_with_stream();
    let response = router
        .oneshot(req(
            "/api/webchat/v2/inbox/events",
            Some(FS_ONLY_DEVICE_TOKEN),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(json["error"], "scope_required");
    assert_eq!(json["scope"], "orchestration:read");
}

#[tokio::test]
async fn authorized_device_token_receives_inbox_changed_sse_event() {
    let (router, tx) = app_with_stream();
    let response = router
        .oneshot(req("/api/webchat/v2/inbox/events", Some(READ_DEVICE_TOKEN)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected text/event-stream, got {content_type}"
    );

    let event = InboxChangedEvent::new(vec!["thread-42".into()]);
    tx.send(Ok(ProductSurfaceStreamResponse {
        events: vec![serde_json::to_value(&event).expect("json")],
        next_cursor: None,
        subscription: None,
    }))
    .await
    .expect("send hint");

    let mut body = response.into_body();
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    collected.push_str(&String::from_utf8_lossy(data));
                    if collected.contains(INBOX_CHANGED_EVENT_KIND) {
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(
        collected.contains("event: inbox_changed"),
        "expected SSE event name in: {collected}"
    );
    assert!(
        collected.contains(r#""kind":"inbox_changed""#),
        "expected inbox_changed payload in: {collected}"
    );
    assert!(
        collected.contains("thread-42"),
        "expected thread_id in: {collected}"
    );
}

#[tokio::test]
async fn query_token_shim_authenticates_inbox_events_stream() {
    let (router, _tx) = app_with_stream();
    let uri = format!("/api/webchat/v2/inbox/events?token={OPERATOR_TOKEN}");
    let response = router.oneshot(req(&uri, None)).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}
