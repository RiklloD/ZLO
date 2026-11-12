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
//!
//! The last test drives native memory through the PRODUCTION embedding chain
//! instead (`[embeddings]` section → real `OpenAiCompatibleEmbeddings` over
//! the production network egress → bound operator key store → native memory),
//! against a loopback stub of the OpenAI wire format: asserted at the wire
//! (the stored key in `authorization`, `dimensions` sent) and at recall.

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
use secrecy::SecretString;
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

/// Each embeddings path is its own per-caller rate-limit bucket (60/min), the
/// same per-route, per-caller limit every OpenAI-compatible create route gets:
/// the host gateway keys buckets by route, so `/v1/embeddings` and
/// `/api/v1/embeddings` (and the chat routes) do not share one. A caller can
/// therefore make up to 60/min on each embeddings path.
#[tokio::test]
async fn each_embeddings_path_has_its_own_per_caller_rate_limit() {
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

    // The alias path is a separate bucket: still open after `/v1` is spent.
    let (status, response) = post(&app, "/api/v1/embeddings", Some(TOKEN), request()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        provider.calls().len(),
        61,
        "the 429 never reached the provider"
    );
}

// ---------------------------------------------------------------------------
// Native memory over the production embedding chain (no fake at the provider
// seam): a loopback stub speaks the OpenAI embeddings wire format.
// ---------------------------------------------------------------------------

/// One request the stub saw: its `authorization` header and JSON body.
type StubRequest = (Option<String>, serde_json::Value);

/// In-memory operator secret store (the production `LlmKeyStore` writes and
/// reads through it).
#[derive(Default)]
struct OperatorKeys(std::sync::Mutex<std::collections::HashMap<String, String>>);

#[async_trait]
impl ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore for OperatorKeys {
    async fn put(
        &self,
        handle: &ironclaw_host_api::ids::SecretHandle,
        value: SecretString,
    ) -> Result<(), ironclaw_product_contracts::operator_secrets::OperatorSecretValueStoreError>
    {
        use secrecy::ExposeSecret;
        self.0.lock().unwrap().insert(
            handle.as_str().to_string(),
            value.expose_secret().to_string(),
        );
        Ok(())
    }
    async fn contains(
        &self,
        handle: &ironclaw_host_api::ids::SecretHandle,
    ) -> Result<bool, ironclaw_product_contracts::operator_secrets::OperatorSecretValueStoreError>
    {
        Ok(self.0.lock().unwrap().contains_key(handle.as_str()))
    }
    async fn handles(
        &self,
    ) -> Result<
        Vec<ironclaw_host_api::ids::SecretHandle>,
        ironclaw_product_contracts::operator_secrets::OperatorSecretValueStoreError,
    > {
        Ok(Vec::new())
    }
    async fn read(
        &self,
        handle: &ironclaw_host_api::ids::SecretHandle,
    ) -> Result<
        Option<SecretString>,
        ironclaw_product_contracts::operator_secrets::OperatorSecretValueStoreError,
    > {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(handle.as_str())
            .map(|value| SecretString::from(value.clone())))
    }
    async fn delete(
        &self,
        handle: &ironclaw_host_api::ids::SecretHandle,
    ) -> Result<bool, ironclaw_product_contracts::operator_secrets::OperatorSecretValueStoreError>
    {
        Ok(self.0.lock().unwrap().remove(handle.as_str()).is_some())
    }
}

/// A loopback OpenAI-compatible `/v1/embeddings` stub: records every
/// request's `authorization` header and JSON body; requests numbered at or
/// after `fail_from` get HTTP 500. "car"/"automobile" embed to `[1, 0]`,
/// everything else to `[0, 1]`.
struct StubEmbeddingServer {
    base_url: String,
    requests: Arc<std::sync::Mutex<Vec<StubRequest>>>,
    fail_from: Arc<std::sync::atomic::AtomicUsize>,
}

impl StubEmbeddingServer {
    async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fail_from = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        let (log, fail) = (Arc::clone(&requests), Arc::clone(&fail_from));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut buf = [0_u8; 8192];
                let header_end = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break None;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(end + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_string())
                    })
                };
                let length: usize = header("content-length").unwrap().parse().unwrap();
                while raw.len() < header_end + length {
                    let n = socket.read(&mut buf).await.unwrap();
                    raw.extend_from_slice(&buf[..n]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&raw[header_end..header_end + length]).unwrap();
                let number = {
                    let mut log = log.lock().unwrap();
                    log.push((header("authorization"), body.clone()));
                    log.len() - 1
                };
                let (status, reply) = if number >= fail.load(std::sync::atomic::Ordering::SeqCst) {
                    (
                        "500 Internal Server Error",
                        serde_json::json!({"error": "down"}),
                    )
                } else {
                    let data: Vec<_> = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .map(|(index, text)| {
                            let text = text.as_str().unwrap().to_ascii_lowercase();
                            let vector = if text.contains("car") || text.contains("automobile") {
                                [1.0, 0.0]
                            } else {
                                [0.0, 1.0]
                            };
                            serde_json::json!({"index": index, "embedding": vector})
                        })
                        .collect();
                    (
                        "200 OK",
                        serde_json::json!({"object": "list", "data": data}),
                    )
                };
                let reply = reply.to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self {
            base_url,
            requests,
            fail_from,
        }
    }

    fn requests(&self) -> Vec<StubRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn memory_invocation() -> ironclaw_memory::MemoryInvocation {
    ironclaw_memory::MemoryInvocation {
        scope: ironclaw_host_api::resource::ResourceScope {
            tenant_id: ironclaw_host_api::ids::TenantId::new("t1").unwrap(),
            user_id: ironclaw_host_api::ids::UserId::new("alice").unwrap(),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: Some(ironclaw_host_api::ids::ThreadId::new("convo").unwrap()),
            invocation_id: ironclaw_host_api::ids::InvocationId::new(),
        },
        correlation_id: ironclaw_host_api::ids::CorrelationId::new(),
    }
}

/// The host's per-run recorder: writes (and indexes) `threads/convo/<run>.md`.
async fn record(service: &dyn ironclaw_memory::MemoryService, run: &str, content: &str) {
    let recorded = service
        .record_interaction(
            memory_invocation(),
            ironclaw_memory::MemoryServiceRecordRequest {
                messages: vec![ironclaw_memory::MemoryInteractionMessage {
                    role: ironclaw_memory::MemoryInteractionRole::User,
                    content: content.to_string(),
                    name: None,
                }],
                turn_run_id: Some(run.to_string()),
                metadata: serde_json::json!({}),
            },
        )
        .await
        .expect("recording succeeds whatever the embedder does");
    assert!(recorded.recorded);
}

/// Prompt recall's short-term lane: the paths it would inject.
async fn recall(service: &dyn ironclaw_memory::MemoryService, query: &str) -> Vec<String> {
    let mut paths: Vec<String> = service
        .read_short_term(
            memory_invocation(),
            ironclaw_memory::MemoryServiceContextRequest {
                query: query.to_string(),
                max_snippets: 5,
                context_profile_id: ironclaw_memory::MemoryContextProfileId::new("default")
                    .unwrap(),
            },
        )
        .await
        .expect("recall")
        .into_iter()
        .map(|snippet| snippet.relative_path)
        .collect();
    paths.dedup();
    paths
}

/// The production `[embeddings]` → memory chain end to end —
/// `RebornHostBindings::with_embeddings_section` → the real
/// `OpenAiCompatibleEmbeddings` over the production network egress →
/// `bind_key_store` → `ResolvedMemoryProvider::with_native_embedder` →
/// the resolver's native service, driven through the host-managed recall
/// path (`record_interaction` / `read_short_term`). Asserted at the wire: the operator key
/// stored in the bound key store reaches the `authorization` header, and a
/// configured shortened dimension is sent as `dimensions`. Then the
/// provider starts failing after earlier calls succeeded: the write still
/// succeeds (stored text-only) and search degrades to full text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_embedding_chain_sends_the_bound_key_and_degrades_on_failure() {
    const KEY: &str = "sk-bound-store-key-42";
    let server = StubEmbeddingServer::start().await;
    let section = ironclaw_config::EmbeddingsSection {
        provider: "openai".to_string(),
        model: "text-embedding-3-small".to_string(),
        dimension: Some(2),
        base_url: Some(server.base_url.clone()),
    };
    let service = ironclaw_composition::test_support::semantic_native_memory_for_test(
        &section,
        Arc::new(OperatorKeys::default()),
        SecretString::from(KEY),
        Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
    )
    .await
    .expect("the production chain builds");

    record(service.as_ref(), "run-1", "my car is teal").await;
    assert_eq!(
        recall(service.as_ref(), "automobile").await,
        vec!["threads/convo/run-1.md".to_string()],
        "meaning-only recall through the production provider"
    );
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one chunk batch, one query: {requests:?}"
    );
    for (authorization, body) in &requests {
        assert_eq!(
            authorization.as_deref(),
            Some(format!("Bearer {KEY}").as_str())
        );
        assert_eq!(body["model"], "text-embedding-3-small");
        assert_eq!(body["dimensions"], 2);
    }

    // The provider goes down after those calls succeeded.
    server
        .fail_from
        .store(requests.len(), std::sync::atomic::Ordering::SeqCst);
    record(service.as_ref(), "run-2", "a banana for lunch").await;
    assert_eq!(
        recall(service.as_ref(), "banana").await,
        vec!["threads/convo/run-2.md".to_string()],
        "the failed document is stored text-only and full text finds it"
    );
    assert_eq!(
        recall(service.as_ref(), "teal").await,
        vec!["threads/convo/run-1.md".to_string()],
        "a failed query embedding degrades recall to full text"
    );
    assert!(
        server.requests().len() > requests.len(),
        "the failing calls were attempted"
    );
}
