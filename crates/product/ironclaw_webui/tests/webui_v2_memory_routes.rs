//! Memory page and hooks routes dispatch through `ProductSurface` with the
//! authenticated caller and the expected descriptor ids and inputs. A
//! recording surface stands in for product; the router is the real one.

#![deny(clippy::let_underscore_must_use)]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_webui::webui_v2::{
    DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER, WebUiV2Capabilities, WebUiV2State, webui_v2_router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Default)]
struct Recording {
    queries: Mutex<Vec<(String, Value, String)>>,
    invokes: Mutex<Vec<(String, Value, String)>>,
}

#[async_trait]
impl ProductSurface for Recording {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        self.invokes.lock().unwrap().push((
            request.operation_id.as_str().to_string(),
            request.input,
            caller.user_id.as_str().to_string(),
        ));
        let output = match request.operation_id.as_str() {
            "memory.documents.write" => json!({"outcome": "written"}),
            "memory.documents.archive" => {
                json!({"outcome": "archived", "archived_path": "archive/x/a.md"})
            }
            "memory.supermemory.delete_document" => json!({"id": "d1", "verified": true}),
            "memory.supermemory.forget" => json!({"id": "m1", "forgotten": true}),
            "memory.wiki.init" => {
                json!({"created": [], "kept": [], "status": {"available": true, "initialized": true, "pages": 3}})
            }
            "service_keys.set" | "service_keys.import" => {
                json!({"id": "supermemory_api_key", "stored": true, "check": "verified"})
            }
            "service_keys.remove" => json!({"id": "supermemory_api_key", "stored": false}),
            _ => return Err(ProductSurfaceError::not_found()),
        };
        Ok(ProductSurfaceInvokeResponse { output })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        self.queries.lock().unwrap().push((
            request.view_id.clone(),
            request.input,
            caller.user_id.as_str().to_string(),
        ));
        Ok(ProductSurfaceQueryPage {
            items: vec![json!({"view": request.view_id})],
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

fn router(surface: Arc<Recording>) -> Router {
    let caller = ProductSurfaceCaller::new(
        TenantId::new("tenant-memory").unwrap(),
        UserId::new("alice").unwrap(),
        None,
        None,
    );
    webui_v2_router(WebUiV2State::new(
        surface,
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER,
    ))
    .layer(axum::Extension(caller))
    .layer(axum::Extension(WebUiV2Capabilities::default()))
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn memory_reads_forward_their_views_with_query_inputs() {
    let surface = Arc::new(Recording::default());
    let router = router(Arc::clone(&surface));
    let cases = [
        (
            "/api/webchat/v2/memory/documents",
            "memory.documents.tree",
            json!({}),
        ),
        (
            "/api/webchat/v2/memory/documents/content?path=notes%2Fa.md",
            "memory.documents.read",
            json!({"path": "notes/a.md"}),
        ),
        (
            "/api/webchat/v2/memory/search?q=tea&limit=3",
            "memory.documents.search",
            json!({"query": "tea", "limit": 3}),
        ),
        (
            "/api/webchat/v2/memory/supermemory?profile_id=work",
            "memory.supermemory.status",
            json!({"profile_id": "work"}),
        ),
        (
            "/api/webchat/v2/memory/supermemory/documents?profile_id=work&page=2",
            "memory.supermemory.documents",
            json!({"profile_id": "work", "container": null, "page": 2, "limit": null}),
        ),
        (
            "/api/webchat/v2/memory/supermemory/search?q=job&container=career",
            "memory.supermemory.search",
            json!({"profile_id": null, "query": "job", "container": "career", "limit": null}),
        ),
        ("/api/webchat/v2/hooks", "hooks.catalog", json!({})),
    ];
    for (uri, view, input) in cases {
        let (status, body) = send(&router, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["view"], view, "{uri}");
        let (recorded_view, recorded_input, user) =
            surface.queries.lock().unwrap().last().cloned().unwrap();
        assert_eq!(recorded_view, view);
        assert_eq!(recorded_input, input, "{uri}");
        assert_eq!(user, "alice", "scope is the authenticated caller");
    }
}

#[tokio::test]
async fn memory_mutations_invoke_their_commands_and_return_evidence() {
    let surface = Arc::new(Recording::default());
    let router = router(Arc::clone(&surface));
    let cases = [
        (
            "/api/webchat/v2/memory/documents",
            "memory.documents.write",
            json!({"path": "a.md", "content": "hi"}),
            "outcome",
        ),
        (
            "/api/webchat/v2/memory/documents/archive",
            "memory.documents.archive",
            json!({"path": "a.md", "expected_content_hash": "sha256:00"}),
            "archived_path",
        ),
        (
            "/api/webchat/v2/memory/supermemory/documents/delete",
            "memory.supermemory.delete_document",
            json!({"id": "d1"}),
            "verified",
        ),
        (
            "/api/webchat/v2/memory/supermemory/memories/forget",
            "memory.supermemory.forget",
            json!({"container": "work", "id": "m1"}),
            "forgotten",
        ),
    ];
    for (uri, command, body, evidence) in cases {
        let (status, response) = send(&router, Method::POST, uri, Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!response[evidence].is_null(), "{uri}: {response}");
        let (recorded, _, user) = surface.invokes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(recorded, command);
        assert_eq!(user, "alice");
    }
}

#[tokio::test]
async fn memory_write_rejects_unknown_body_fields_before_the_surface() {
    let surface = Arc::new(Recording::default());
    let router = router(Arc::clone(&surface));
    let (status, _) = send(
        &router,
        Method::POST,
        "/api/webchat/v2/memory/documents",
        Some(json!({"path": "a.md", "content": "hi", "append": true})),
    )
    .await;
    assert!(status.is_client_error());
    assert!(surface.invokes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn keys_and_wiki_routes_forward_their_descriptors() {
    let surface = Arc::new(Recording::default());
    let router = router(Arc::clone(&surface));
    for (uri, view) in [
        ("/api/webchat/v2/keys", "service_keys.list"),
        ("/api/webchat/v2/memory/wiki", "memory.wiki.status"),
    ] {
        let (status, _) = send(&router, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let (recorded, _, user) = surface.queries.lock().unwrap().last().cloned().unwrap();
        assert_eq!((recorded.as_str(), user.as_str()), (view, "alice"));
    }
    for (uri, command, body) in [
        (
            "/api/webchat/v2/keys",
            "service_keys.set",
            json!({"id": "supermemory_api_key", "value": "sm_route_test"}),
        ),
        (
            "/api/webchat/v2/keys/import",
            "service_keys.import",
            json!({"id": "supermemory_api_key"}),
        ),
        (
            "/api/webchat/v2/keys/remove",
            "service_keys.remove",
            json!({"id": "supermemory_api_key"}),
        ),
        (
            "/api/webchat/v2/memory/wiki/init",
            "memory.wiki.init",
            json!({}),
        ),
    ] {
        let (status, response) = send(&router, Method::POST, uri, Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {response}");
        assert!(!response.to_string().contains("sm_route_test"));
        let (recorded, _, _) = surface.invokes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(recorded, command);
    }
}

#[tokio::test]
async fn a_malformed_key_body_is_refused_without_quoting_it() {
    let surface = Arc::new(Recording::default());
    let router = router(Arc::clone(&surface));
    for body in [
        json!({"id": "supermemory_api_key", "value": "sm_SECRET_ONE", "extra": "sm_SECRET_TWO"}),
        json!({"id": "supermemory_api_key", "value": ["sm_SECRET_ONE"]}),
        json!({"id": 7, "value": "sm_SECRET_ONE"}),
    ] {
        let (status, response) =
            send(&router, Method::POST, "/api/webchat/v2/keys", Some(body)).await;
        assert!(status.is_client_error(), "{response}");
        let text = response.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(response["field"], "request", "{text}");
    }
    assert!(surface.invokes.lock().unwrap().is_empty());
}
