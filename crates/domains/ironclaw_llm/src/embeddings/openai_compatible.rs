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
/// Response cap: 2048 inputs × 3072 dims of JSON floats fits well under this.
const EMBEDDING_RESPONSE_BODY_LIMIT: u64 = 64 * 1024 * 1024;

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
    pub fn new(
        endpoint: ResolvedEmbeddingEndpoint,
        credentials: Arc<dyn EmbeddingCredentialSource>,
        egress: Arc<dyn NetworkHttpEgress>,
    ) -> Result<Self, EmbeddingError> {
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
        Ok(Self {
            egress,
            endpoint,
            credentials,
            policy,
            url,
            limits: EmbeddingLimits::default(),
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
                reason: format!("body is not an embeddings list: {error}"),
            }
        })?;
        // Providers may return `data` out of order; `index` is authoritative
        // when present.
        let mut data = wire.data;
        if data.iter().all(|item| item.index.is_some()) {
            data.sort_by_key(|item| item.index);
            let in_order = data
                .iter()
                .enumerate()
                .all(|(position, item)| item.index == Some(position));
            if !in_order {
                return Err(EmbeddingError::InvalidResponse {
                    reason: "embedding indexes are not a permutation of the inputs".to_string(),
                });
            }
        }
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
