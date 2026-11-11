//! `POST /v1/embeddings` through the production HTTP stack.
//!
//! Drives composition's real `/v1/embeddings` adapter
//! (`ironclaw_composition::openai_compat_embeddings_port`) over a
//! deterministic `ConceptEmbeddings` provider — the fake at the provider seam,
//! no network — mounted on the real `ironclaw_openai_compat` router behind
//! `ironclaw_webui`'s `webui_v2_app`, so the same bearer-auth middleware and
//! the descriptor-driven body-limit and per-caller rate-limit layers that
//! guard `/v1/chat/completions` guard this route. Asserted at the seam: the
//! wire response, and exactly which batches reached the provider.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use base64::Engine as _;
use ironclaw_assistant::rejecting_product_surface_error;
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_host_ingress::ProtectedRouteMount;
use ironclaw_llm::testing::embeddings::ConceptEmbeddings;
use ironclaw_openai_compat::{
    OpenAiCompatRouterState, openai_compat_router_with_state, openai_compat_routes,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_webui::{WebuiAuthentication, WebuiAuthenticator, WebuiServeConfig, webui_v2_app};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "embeddings-test-token";
const MODEL: &str = "test-concept-embedder";

struct OneToken;

#[async_trait]
impl WebuiAuthenticator for OneToken {
    async fn authenticate(&self, token: &str) -> Option<WebuiAuthentication> {
        (token == TOKEN).then(|| WebuiAuthentication::user(UserId::new("user-emb").expect("user")))
    }
}

/// The embeddings route never touches the product surface; every method
/// rejects so an unexpected dispatch fails loudly.
struct RejectingSurface;

#[async_trait]
impl ProductSurface for RejectingSurface {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(rejecting_product_surface_error())
    }

    async fn query(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(rejecting_product_surface_error())
    }

    async fn stream_events(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(rejecting_product_surface_error())
    }
}

fn app(provider: Arc<ConceptEmbeddings>) -> Router {
    let port = ironclaw_composition::openai_compat_embeddings_port(provider);
    let mount = ProtectedRouteMount::new(
        openai_compat_router_with_state(
            OpenAiCompatRouterState::not_wired().with_embeddings_service(port),
        ),
        openai_compat_routes(),
    );
    let config = WebuiServeConfig::new(
        TenantId::new("tenant-emb").expect("tenant"),
        Arc::new(OneToken),
        vec![HeaderValue::from_static("http://localhost:3000")],
    )
    .with_default_agent_id(AgentId::new("agent-emb").expect("agent"))
    .with_default_project_id(ProjectId::new("project-emb").expect("project"))
    .with_protected_route_mount(mount);
    webui_v2_app(Arc::new(RejectingSurface), config).expect("webui v2 app")
}

fn embedder() -> Arc<ConceptEmbeddings> {
    Arc::new(ConceptEmbeddings::new(8).with_concept(["car", "automobile"]))
}

async fn post(app: &Router, path: &str, token: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("json")
}

#[tokio::test]
async fn embeddings_answer_openai_shape_behind_webui_auth_on_both_paths() {
    let provider = embedder();
    let app = app(Arc::clone(&provider));

    // No bearer: the host middleware answers before the route or provider.
    let (status, _) = post(
        &app,
        "/v1/embeddings",
        None,
        body(json!({"model": MODEL, "input": "car"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        provider.calls().is_empty(),
        "unauthenticated call reached the provider"
    );

    for path in ["/v1/embeddings", "/api/v1/embeddings"] {
        let (status, response) = post(
            &app,
            path,
            Some(TOKEN),
            body(json!({"model": MODEL, "input": ["my car", "an automobile"]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {response}");
        assert_eq!(response["object"], "list");
        assert_eq!(response["model"], MODEL);
        assert_eq!(response["data"][0]["object"], "embedding");
        assert_eq!(response["data"][1]["index"], 1);
        let first: Vec<f32> =
            serde_json::from_value(response["data"][0]["embedding"].clone()).expect("float vector");
        assert_eq!(first, provider.vector("my car"), "{path}");
        assert_eq!(first.len(), 8);
        assert_eq!(response["usage"]["total_tokens"], 4);
    }

    // `encoding_format: base64` (the OpenAI SDKs' default) is little-endian f32.
    let (status, response) = post(
        &app,
        "/v1/embeddings",
        Some(TOKEN),
        body(json!({"model": MODEL, "input": "car", "encoding_format": "base64"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            response["data"][0]["embedding"]
                .as_str()
                .expect("base64 string"),
        )
        .expect("valid base64");
    let decoded: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();
    assert_eq!(decoded, provider.vector("car"));

    // Exactly the three authenticated batches reached the provider.
    assert_eq!(
        provider.calls(),
        vec![
            vec!["my car".to_string(), "an automobile".to_string()],
            vec!["my car".to_string(), "an automobile".to_string()],
            vec!["car".to_string()],
        ]
    );
}

#[tokio::test]
async fn invalid_and_oversized_payloads_are_refused_before_the_provider() {
    let provider = embedder();
    let app = app(Arc::clone(&provider));

    let cases = [
        (
            json!({"model": "other-model", "input": "car"}),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({"model": MODEL, "input": [1, 2, 3]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"model": MODEL, "input": []}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"model": MODEL, "input": "car", "dimensions": 9}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"model": MODEL, "input": vec!["x"; 2049]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"model": MODEL, "input": "x".repeat(32_001)}),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (request, expected) in cases {
        let (status, response) = post(&app, "/v1/embeddings", Some(TOKEN), body(request)).await;
        assert_eq!(status, expected, "{response}");
        assert!(response["error"]["message"].is_string(), "{response}");
    }

    // Over the route's 2 MiB descriptor body limit: refused by the host layer.
    let oversized = body(json!({"model": MODEL, "input": "x".repeat(3 * 1024 * 1024)}));
    let (status, _) = post(&app, "/v1/embeddings", Some(TOKEN), oversized).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    assert!(
        provider.calls().is_empty(),
        "no invalid payload may reach the provider: {:?}",
        provider.calls().len()
    );
}

#[tokio::test]
async fn embeddings_share_the_per_caller_create_rate_limit() {
    let provider = embedder();
    let app = app(Arc::clone(&provider));
    let request = || body(json!({"model": MODEL, "input": "car"}));
    for attempt in 0..60 {
        let (status, response) = post(&app, "/v1/embeddings", Some(TOKEN), request()).await;
        assert_eq!(status, StatusCode::OK, "attempt {attempt}: {response}");
    }
    let (status, _) = post(&app, "/v1/embeddings", Some(TOKEN), request()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(provider.calls().len(), 60);
}
