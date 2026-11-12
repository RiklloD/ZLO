//! OpenAI-compatible `POST {base}/embeddings` provider over
//! `ironclaw_network`'s policy egress.
//!
//! Serves every registry provider that speaks the OpenAI embeddings shape
//! (OpenAI, Ollama's `/v1`, OpenRouter, Together, Mistral, LM Studio, …).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::action::{NetworkMethod, NetworkPolicy, NetworkTargetPattern};
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpRequest, PolicyNetworkHttpEgress, ReqwestNetworkTransport,
    network_target_for_url,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use super::{
    EmbeddingCredentialSource, EmbeddingError, EmbeddingLimits, EmbeddingProvider,
    EmbeddingRequest, EmbeddingResponse, EmbeddingUsage, ResolvedEmbeddingEndpoint,
};

/// Per-request timeout. Embedding calls are one-shot, small, and not gated by
/// the runner lease.
const EMBEDDING_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Response cap. A JSON float is budgeted at [`JSON_BYTES_PER_FLOAT`] bytes,
/// so the full 2048-input batch does NOT fit at large dimensions (2048 × 3072
/// floats is ~75–150 MB of JSON); [`limits_for_dimension`] instead derives
/// the per-request input count from the dimension so every admitted batch's
/// response fits under this cap.
const EMBEDDING_RESPONSE_BODY_LIMIT: u64 = 64 * 1024 * 1024;
/// Worst-case JSON bytes per float (`-1.2345678e-05,` plus headroom).
const JSON_BYTES_PER_FLOAT: u64 = 24;
/// Per-item JSON overhead (`{"object":"embedding","index":N,"embedding":[]}`).
const JSON_BYTES_PER_ITEM: u64 = 128;
/// Envelope overhead (`object`, `model`, `usage`).
const JSON_ENVELOPE_BYTES: u64 = 64 * 1024;

/// Request bounds whose worst-case response fits [`EMBEDDING_RESPONSE_BODY_LIMIT`].
fn limits_for_dimension(dimension: u32) -> EmbeddingLimits {
    let per_item = u64::from(dimension) * JSON_BYTES_PER_FLOAT + JSON_BYTES_PER_ITEM;
    let fits = (EMBEDDING_RESPONSE_BODY_LIMIT - JSON_ENVELOPE_BYTES) / per_item.max(1);
    let max_inputs = usize::try_from(fits)
        .unwrap_or(usize::MAX)
        .clamp(1, super::MAX_EMBEDDING_INPUTS);
    EmbeddingLimits {
        max_inputs,
        ..EmbeddingLimits::default()
    }
}

/// OpenAI-compatible embeddings client.
pub struct OpenAiCompatibleEmbeddings {
    egress: Arc<dyn NetworkHttpEgress>,
    endpoint: ResolvedEmbeddingEndpoint,
    credentials: Arc<dyn EmbeddingCredentialSource>,
    policy: NetworkPolicy,
    url: String,
    limits: EmbeddingLimits,
}

impl std::fmt::Debug for OpenAiCompatibleEmbeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleEmbeddings")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleEmbeddings {
    /// Build over an explicit egress (tests inject a recording egress here).
    ///
    /// The network policy admits exactly the configured host. Private and
    /// loopback addresses are denied unless the operator configured a
    /// loopback host itself (a local Ollama / LM Studio), in which case that
    /// single host:port is the only target admitted.
    ///
    /// The base URL is re-validated here, not only in
    /// [`EmbeddingsConfig::resolve`](super::EmbeddingsConfig::resolve):
    /// [`ResolvedEmbeddingEndpoint`] is publicly constructible, and the
    /// request carries the API key, so a plaintext `http` URL to a
    /// non-loopback host (or one with a query/fragment/userinfo) is refused
    /// before any provider exists to send it.
    pub fn new(
        endpoint: ResolvedEmbeddingEndpoint,
        credentials: Arc<dyn EmbeddingCredentialSource>,
        egress: Arc<dyn NetworkHttpEgress>,
    ) -> Result<Self, EmbeddingError> {
        super::validate_embedding_base_url(&endpoint.base_url)?;
        if endpoint.dimension == 0 || endpoint.dimension > super::MAX_EMBEDDING_DIMENSION {
            return Err(EmbeddingError::InvalidConfig {
                reason: format!(
                    "dimension {} is outside 1..={}",
                    endpoint.dimension,
                    super::MAX_EMBEDDING_DIMENSION
                ),
            });
        }
        let target = network_target_for_url(&endpoint.base_url).map_err(|error| {
            EmbeddingError::InvalidConfig {
                reason: format!("base URL: {error}"),
            }
        })?;
        let loopback = is_loopback_host(&target.host);
        let policy = NetworkPolicy {
            allowed_targets: vec![NetworkTargetPattern {
                scheme: Some(target.scheme),
                host_pattern: target.host,
                port: target.port,
            }],
            deny_private_ip_ranges: !loopback,
            max_egress_bytes: None,
        };
        let url = format!("{}/embeddings", endpoint.base_url);
        let limits = limits_for_dimension(endpoint.dimension);
        Ok(Self {
            egress,
            endpoint,
            credentials,
            policy,
            url,
            limits,
        })
    }

    /// Build over the hardened production egress (pinned reqwest transport
    /// behind the network policy).
    pub fn production(
        endpoint: ResolvedEmbeddingEndpoint,
        credentials: Arc<dyn EmbeddingCredentialSource>,
    ) -> Result<Self, EmbeddingError> {
        let egress = Arc::new(PolicyNetworkHttpEgress::new(ReqwestNetworkTransport::new(
            EMBEDDING_REQUEST_TIMEOUT,
        )));
        Self::new(endpoint, credentials, egress)
    }

    fn http_request(
        &self,
        scope: ironclaw_host_api::resource::ResourceScope,
        api_key: Option<&SecretString>,
        body: Vec<u8>,
    ) -> NetworkHttpRequest {
        let mut headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        if let Some(key) = api_key {
            headers.push((
                "authorization".to_string(),
                format!("Bearer {}", key.expose_secret()),
            ));
        }
        NetworkHttpRequest {
            scope,
            method: NetworkMethod::Post,
            url: self.url.clone(),
            headers,
            body,
            policy: self.policy.clone(),
            response_body_limit: Some(EMBEDDING_RESPONSE_BODY_LIMIT),
            timeout_ms: u32::try_from(EMBEDDING_REQUEST_TIMEOUT.as_millis()).ok(),
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
    /// OpenAI's shortened-vector parameter; omitted for the native dimension.
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

/// A body parse failure described without any of the body: serde's own
/// messages quote offending values (`invalid type: string "…"`), which may be
/// an echoed input or credential, so only the category and position are kept.
fn safe_parse_failure(error: &serde_json::Error) -> String {
    let category = match error.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "unexpected shape",
        serde_json::error::Category::Eof => "truncated",
    };
    format!(
        "body is not an embeddings list ({category} at line {}, column {})",
        error.line(),
        error.column()
    )
}

/// Put `data` in input order. `index` is authoritative when the provider
/// sends it, but only as a complete permutation: either every item omits it
/// (wire order is input order) or every item carries a distinct index in
/// `0..len`. Mixed presence or duplicates could attach a vector to the wrong
/// input, so they are rejected.
fn order_by_index(mut data: Vec<WireEmbedding>) -> Result<Vec<WireEmbedding>, EmbeddingError> {
    let indexed = data.iter().filter(|item| item.index.is_some()).count();
    if indexed == 0 {
        return Ok(data);
    }
    if indexed != data.len() {
        return Err(EmbeddingError::InvalidResponse {
            reason: "embedding indexes are present on some items but not others".to_string(),
        });
    }
    data.sort_by_key(|item| item.index);
    let permutation = data
        .iter()
        .enumerate()
        .all(|(position, item)| item.index == Some(position));
    if !permutation {
        return Err(EmbeddingError::InvalidResponse {
            reason: "embedding indexes are not a permutation of the inputs".to_string(),
        });
    }
    Ok(data)
}

#[derive(Deserialize)]
struct WireResponse {
    data: Vec<WireEmbedding>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireEmbedding {
    #[serde(default)]
    index: Option<usize>,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[async_trait]
impl EmbeddingProvider for OpenAiCompatibleEmbeddings {
    fn model_name(&self) -> &str {
        &self.endpoint.model
    }

    fn dimension(&self) -> u32 {
        self.endpoint.dimension
    }

    fn limits(&self) -> EmbeddingLimits {
        self.limits
    }

    fn fingerprint(&self) -> String {
        self.endpoint.fingerprint()
    }

    async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse, EmbeddingError> {
        request.validate(self.limits)?;
        let api_key = self.credentials.api_key().await?;
        if self.endpoint.api_key_required && api_key.is_none() {
            return Err(EmbeddingError::MissingCredential {
                provider_id: self.endpoint.provider_id.clone(),
            });
        }
        let body = serde_json::to_vec(&WireRequest {
            model: &self.endpoint.model,
            input: &request.inputs,
            encoding_format: "float",
            dimensions: self.endpoint.request_dimensions,
        })
        .map_err(|error| EmbeddingError::InvalidRequest {
            reason: format!("request body: {error}"),
        })?;
        let expected = request.inputs.len();
        let response = self
            .egress
            .execute(self.http_request(request.scope, api_key.as_ref(), body))
            .await
            .map_err(|error| EmbeddingError::Transport {
                reason: error.to_string(),
            })?;
        match response.status {
            200..=299 => {}
            401 | 403 => {
                return Err(EmbeddingError::Unauthorized {
                    status: response.status,
                });
            }
            429 => {
                return Err(EmbeddingError::RateLimited {
                    retry_after: retry_after(&response.headers),
                });
            }
            status => return Err(EmbeddingError::Http { status }),
        }
        let wire: WireResponse = serde_json::from_slice(&response.body).map_err(|error| {
            EmbeddingError::InvalidResponse {
                reason: safe_parse_failure(&error),
            }
        })?;
        // Providers may return `data` out of order; `index` is authoritative
        // when present.
        let data = order_by_index(wire.data)?;
        let result = EmbeddingResponse {
            embeddings: data.into_iter().map(|item| item.embedding).collect(),
            usage: wire
                .usage
                .map(|usage| EmbeddingUsage {
                    prompt_tokens: usage.prompt_tokens,
                    total_tokens: usage.total_tokens,
                })
                .unwrap_or_default(),
        };
        result.validate(expected, self.endpoint.dimension)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironclaw_host_api::resource::ResourceScope;
    use ironclaw_network::{NetworkHttpError, NetworkHttpResponse, NetworkUsage};

    use super::*;
    use crate::embeddings::StaticEmbeddingCredential;

    fn key(value: &str) -> Arc<dyn EmbeddingCredentialSource> {
        Arc::new(StaticEmbeddingCredential(Some(SecretString::from(value))))
    }

    fn no_key() -> Arc<dyn EmbeddingCredentialSource> {
        Arc::new(StaticEmbeddingCredential(None))
    }

    /// (url, headers, body, policy) of one captured request.
    type Captured = (String, Vec<(String, String)>, Vec<u8>, NetworkPolicy);

    #[derive(Default)]
    struct RecordingEgress {
        requests: Mutex<Vec<Captured>>,
        response: Mutex<Option<NetworkHttpResponse>>,
    }

    impl RecordingEgress {
        fn answering(status: u16, body: serde_json::Value) -> Arc<Self> {
            let egress = Self::default();
            *egress.response.lock().unwrap() = Some(NetworkHttpResponse {
                status,
                headers: vec![("retry-after".to_string(), "7".to_string())],
                body: serde_json::to_vec(&body).unwrap(),
                usage: NetworkUsage::default(),
            });
            Arc::new(egress)
        }
    }

    #[async_trait]
    impl NetworkHttpEgress for RecordingEgress {
        async fn execute(
            &self,
            request: NetworkHttpRequest,
        ) -> Result<NetworkHttpResponse, NetworkHttpError> {
            self.requests.lock().unwrap().push((
                request.url.clone(),
                request.headers.clone(),
                request.body.clone(),
                request.policy.clone(),
            ));
            Ok(self.response.lock().unwrap().clone().unwrap())
        }
    }

    fn endpoint(base_url: &str, dimension: u32) -> ResolvedEmbeddingEndpoint {
        ResolvedEmbeddingEndpoint {
            provider_id: "openai".to_string(),
            model: "text-embedding-3-small".to_string(),
            dimension,
            base_url: base_url.to_string(),
            api_key_required: true,
            request_dimensions: None,
        }
    }

    fn request(inputs: &[&str]) -> EmbeddingRequest {
        EmbeddingRequest::new(
            ResourceScope::system(),
            inputs.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[tokio::test]
    async fn sends_openai_shape_with_bearer_and_pinned_policy_and_reorders_by_index() {
        let egress = RecordingEgress::answering(
            200,
            serde_json::json!({
                "object": "list",
                "data": [
                    {"object": "embedding", "index": 1, "embedding": [0.0, 1.0]},
                    {"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}
                ],
                "model": "text-embedding-3-small",
                "usage": {"prompt_tokens": 4, "total_tokens": 4}
            }),
        );
        let provider = OpenAiCompatibleEmbeddings::new(
            endpoint("https://api.example.com/v1", 2),
            key("sk-test"),
            egress.clone(),
        )
        .unwrap();
        let response = provider.embed(request(&["a", "b"])).await.unwrap();
        assert_eq!(response.embeddings, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert_eq!(response.usage.total_tokens, 4);

        let requests = egress.requests.lock().unwrap();
        let (url, headers, body, policy) = &requests[0];
        assert_eq!(url, "https://api.example.com/v1/embeddings");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer sk-test")
        );
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["model"], "text-embedding-3-small");
        assert_eq!(body["input"], serde_json::json!(["a", "b"]));
        assert_eq!(policy.allowed_targets.len(), 1);
        assert_eq!(policy.allowed_targets[0].host_pattern, "api.example.com");
        assert!(policy.deny_private_ip_ranges);
    }

    #[tokio::test]
    async fn loopback_base_url_is_the_only_private_target_admitted() {
        let egress =
            RecordingEgress::answering(200, serde_json::json!({"data": [{"embedding": [1.0]}]}));
        let mut local = endpoint("http://localhost:11434/v1", 1);
        local.api_key_required = false;
        let provider = OpenAiCompatibleEmbeddings::new(local, no_key(), egress.clone()).unwrap();
        provider.embed(request(&["a"])).await.unwrap();
        let requests = egress.requests.lock().unwrap();
        let (_, headers, _, policy) = &requests[0];
        assert!(!headers.iter().any(|(k, _)| k == "authorization"));
        assert!(!policy.deny_private_ip_ranges);
        assert_eq!(policy.allowed_targets[0].host_pattern, "localhost");
        assert_eq!(policy.allowed_targets[0].port, Some(11434));
    }

    #[tokio::test]
    async fn invalid_requests_never_reach_the_egress() {
        let egress = RecordingEgress::answering(200, serde_json::json!({"data": []}));
        let provider = OpenAiCompatibleEmbeddings::new(
            endpoint("https://api.example.com/v1", 2),
            key("sk-test"),
            egress.clone(),
        )
        .unwrap();
        assert!(matches!(
            provider.embed(request(&[])).await,
            Err(EmbeddingError::InvalidRequest { .. })
        ));
        assert!(egress.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn maps_status_codes_and_rejects_malformed_bodies() {
        let cases = [
            (401, serde_json::json!({}), "unauthorized"),
            (429, serde_json::json!({}), "rate"),
            (500, serde_json::json!({}), "http"),
            (200, serde_json::json!({"nope": 1}), "invalid"),
            (
                200,
                serde_json::json!({"data": [{"embedding": [1.0]}]}),
                "invalid",
            ),
        ];
        for (status, body, expected) in cases {
            let provider = OpenAiCompatibleEmbeddings::new(
                endpoint("https://api.example.com/v1", 2),
                key("sk-test"),
                RecordingEgress::answering(status, body),
            )
            .unwrap();
            let error = provider.embed(request(&["a"])).await.unwrap_err();
            let matched = match &error {
                EmbeddingError::Unauthorized { .. } => expected == "unauthorized",
                EmbeddingError::RateLimited { retry_after } => {
                    expected == "rate" && *retry_after == Some(Duration::from_secs(7))
                }
                EmbeddingError::Http { status } => expected == "http" && *status == 500,
                EmbeddingError::InvalidResponse { .. } => expected == "invalid",
                _ => false,
            };
            assert!(matched, "status {status}: unexpected {error:?}");
            assert!(!error.to_string().contains("sk-test"));
        }
    }

    #[test]
    fn plaintext_or_malformed_base_urls_are_refused_before_any_egress() {
        let egress = RecordingEgress::answering(200, serde_json::json!({}));
        for url in [
            "http://api.example.com/v1",
            "http://8.8.8.8/v1",
            "https://api.example.com/v1#x",
            "https://api.example.com/v1?key=1",
            "https://user:pw@api.example.com/v1",
        ] {
            let built =
                OpenAiCompatibleEmbeddings::new(endpoint(url, 2), key("sk-test"), egress.clone());
            assert!(
                matches!(built, Err(EmbeddingError::InvalidConfig { .. })),
                "{url} must be refused"
            );
        }
        assert!(egress.requests.lock().unwrap().is_empty());
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn malformed_response_errors_quote_neither_the_key_nor_the_input() {
        const SECRET: &str = "sk-live-SECRET-0123456789";
        const MARKER: &str = "SENSITIVE-INPUT-MARKER";
        let echoed = format!("{SECRET} {MARKER}");
        for body in [
            serde_json::json!({"data": [{"embedding": echoed}]}),
            serde_json::json!({"data": [{"index": echoed, "embedding": [1.0, 0.0]}]}),
            serde_json::json!({"data": echoed}),
        ] {
            let provider = OpenAiCompatibleEmbeddings::new(
                endpoint("https://api.example.com/v1", 2),
                key(SECRET),
                RecordingEgress::answering(200, body),
            )
            .unwrap();
            let error = provider.embed(request(&[MARKER])).await.unwrap_err();
            assert!(matches!(error, EmbeddingError::InvalidResponse { .. }));
            tracing::debug!(%error, "embedding request failed");
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(!rendered.contains(SECRET), "{rendered}");
                assert!(!rendered.contains(MARKER), "{rendered}");
            }
        }
        assert!(logs_contain("embedding request failed"));
        assert!(!logs_contain(SECRET));
        assert!(!logs_contain(MARKER));
    }

    #[tokio::test]
    async fn mixed_or_duplicate_indexes_are_rejected() {
        for data in [
            serde_json::json!([
                {"index": 1, "embedding": [0.0, 1.0]},
                {"embedding": [1.0, 0.0]}
            ]),
            serde_json::json!([
                {"embedding": [1.0, 0.0]},
                {"index": 0, "embedding": [0.0, 1.0]}
            ]),
            serde_json::json!([
                {"index": 0, "embedding": [1.0, 0.0]},
                {"index": 0, "embedding": [0.0, 1.0]}
            ]),
        ] {
            let provider = OpenAiCompatibleEmbeddings::new(
                endpoint("https://api.example.com/v1", 2),
                key("sk-test"),
                RecordingEgress::answering(200, serde_json::json!({ "data": data })),
            )
            .unwrap();
            let result = provider.embed(request(&["a", "b"])).await;
            assert!(
                matches!(result, Err(EmbeddingError::InvalidResponse { .. })),
                "{data}: {result:?}"
            );
        }
        // All absent: wire order is input order.
        let provider = OpenAiCompatibleEmbeddings::new(
            endpoint("https://api.example.com/v1", 2),
            key("sk-test"),
            RecordingEgress::answering(
                200,
                serde_json::json!({"data": [{"embedding": [1.0, 0.0]}, {"embedding": [0.0, 1.0]}]}),
            ),
        )
        .unwrap();
        let response = provider.embed(request(&["a", "b"])).await.unwrap();
        assert_eq!(response.embeddings, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    #[tokio::test]
    async fn a_shortened_dimension_is_sent_as_the_dimensions_parameter() {
        let egress = RecordingEgress::answering(
            200,
            serde_json::json!({"data": [{"embedding": [1.0, 0.0]}]}),
        );
        let mut short = endpoint("https://api.example.com/v1", 2);
        short.request_dimensions = Some(2);
        let provider =
            OpenAiCompatibleEmbeddings::new(short, key("sk-test"), egress.clone()).unwrap();
        provider.embed(request(&["a"])).await.unwrap();
        let native = OpenAiCompatibleEmbeddings::new(
            endpoint("https://api.example.com/v1", 2),
            key("sk-test"),
            egress.clone(),
        )
        .unwrap();
        native.embed(request(&["a"])).await.unwrap();
        let requests = egress.requests.lock().unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&requests[0].2).unwrap();
        assert_eq!(sent["dimensions"], 2);
        let sent: serde_json::Value = serde_json::from_slice(&requests[1].2).unwrap();
        assert!(sent.get("dimensions").is_none(), "{sent}");
        assert_ne!(provider.fingerprint(), native.fingerprint());
    }

    #[test]
    fn the_largest_admitted_batch_fits_the_response_cap() {
        for dimension in [
            1,
            384,
            1536,
            3072,
            crate::embeddings::MAX_EMBEDDING_DIMENSION,
        ] {
            let limits = limits_for_dimension(dimension);
            let worst = limits.max_inputs as u64
                * (u64::from(dimension) * JSON_BYTES_PER_FLOAT + JSON_BYTES_PER_ITEM)
                + JSON_ENVELOPE_BYTES;
            assert!(
                worst <= EMBEDDING_RESPONSE_BODY_LIMIT,
                "dimension {dimension}"
            );
            assert!(limits.max_inputs >= 1);
        }
        assert!(limits_for_dimension(3072).max_inputs < crate::embeddings::MAX_EMBEDDING_INPUTS);
    }

    #[tokio::test]
    async fn missing_required_key_fails_closed_before_egress() {
        let egress = RecordingEgress::answering(200, serde_json::json!({}));
        let provider = OpenAiCompatibleEmbeddings::new(
            endpoint("https://api.example.com/v1", 2),
            no_key(),
            egress.clone(),
        )
        .unwrap();
        let error = provider.embed(request(&["a"])).await.unwrap_err();
        assert!(matches!(error, EmbeddingError::MissingCredential { .. }));
        assert!(egress.requests.lock().unwrap().is_empty());
    }
}
