//! Text embeddings: the provider-neutral [`EmbeddingProvider`] contract and
//! its implementations.
//!
//! Embeddings are a **different trait** from [`crate::LlmProvider`] (the same
//! split as `transcription`/`speech`): an embedding call takes a batch of
//! texts and returns one fixed-dimension vector per text, with none of the
//! chat vocabulary. Every implementation:
//!
//! - validates and bounds the request before any I/O
//!   ([`EmbeddingRequest::validate`] against the provider's
//!   [`EmbeddingLimits`]);
//! - sends through `ironclaw_network`'s policy egress (never a raw client),
//!   carrying the caller's [`ResourceScope`];
//! - receives its credential from the caller (composition reads it through
//!   the operator secret store, the same path the chat providers use) and
//!   places it only in the `authorization` header, which the network carrier
//!   zeroizes on drop;
//! - validates the response shape (one vector per input, each exactly
//!   [`EmbeddingProvider::dimension`] finite floats) before returning it.
//!
//! A wrapper around `dyn EmbeddingProvider` must delegate **every** trait
//! method, including `dimension`, `limits` and `fingerprint` — a wrapper that
//! forgets them silently changes the vector dimension, the request bounds, or
//! the vector-space identity of the provider it wraps.

mod openai_compatible;

use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::resource::ResourceScope;
use secrecy::SecretString;

use crate::registry::{ProviderProtocol, ProviderRegistry};

pub use self::openai_compatible::OpenAiCompatibleEmbeddings;

/// Hard ceiling on the number of inputs in one request, whatever a provider
/// declares (OpenAI's own limit is 2048).
pub const MAX_EMBEDDING_INPUTS: usize = 2048;
/// Hard ceiling on one input's UTF-8 byte length, whatever a provider declares
/// (~8191 tokens for the OpenAI embedding family).
pub const MAX_EMBEDDING_INPUT_BYTES: usize = 32_000;
/// Hard ceiling on a vector's dimension.
pub const MAX_EMBEDDING_DIMENSION: u32 = 8192;

/// Errors from an embedding call. Messages never carry credential material or
/// raw provider bodies.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("invalid embedding request: {reason}")]
    InvalidRequest { reason: String },
    #[error("invalid embedding configuration: {reason}")]
    InvalidConfig { reason: String },
    #[error("embedding provider `{provider_id}` requires an API key and none is stored")]
    MissingCredential { provider_id: String },
    #[error("embedding credential could not be read: {reason}")]
    CredentialUnavailable { reason: String },
    #[error("embedding provider rejected the credential (HTTP {status})")]
    Unauthorized { status: u16 },
    #[error("embedding provider rate limited the request")]
    RateLimited { retry_after: Option<Duration> },
    #[error("embedding provider returned HTTP {status}")]
    Http { status: u16 },
    #[error("embedding transport failed: {reason}")]
    Transport { reason: String },
    #[error("embedding provider returned an invalid response: {reason}")]
    InvalidResponse { reason: String },
}

impl EmbeddingError {
    fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: reason.into(),
        }
    }

    fn invalid_config(reason: impl Into<String>) -> Self {
        Self::InvalidConfig {
            reason: reason.into(),
        }
    }

    fn invalid_response(reason: impl Into<String>) -> Self {
        Self::InvalidResponse {
            reason: reason.into(),
        }
    }
}

/// Request bounds a provider accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingLimits {
    /// Maximum inputs per request.
    pub max_inputs: usize,
    /// Maximum UTF-8 bytes per input (`str::len`, not characters).
    pub max_input_bytes: usize,
}

impl Default for EmbeddingLimits {
    fn default() -> Self {
        Self {
            max_inputs: MAX_EMBEDDING_INPUTS,
            max_input_bytes: MAX_EMBEDDING_INPUT_BYTES,
        }
    }
}

/// One batched embedding request.
#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    /// The caller the network egress attributes the call to.
    pub scope: ResourceScope,
    /// The texts to embed, in order.
    pub inputs: Vec<String>,
}

impl EmbeddingRequest {
    pub fn new(scope: ResourceScope, inputs: Vec<String>) -> Self {
        Self { scope, inputs }
    }

    /// Reject an empty batch, an oversized batch, an empty input, or an input
    /// over the byte budget — before anything leaves the process.
    pub fn validate(&self, limits: EmbeddingLimits) -> Result<(), EmbeddingError> {
        let max_inputs = limits.max_inputs.min(MAX_EMBEDDING_INPUTS);
        let max_bytes = limits.max_input_bytes.min(MAX_EMBEDDING_INPUT_BYTES);
        if self.inputs.is_empty() {
            return Err(EmbeddingError::invalid_request("no inputs"));
        }
        if self.inputs.len() > max_inputs {
            return Err(EmbeddingError::invalid_request(format!(
                "{} inputs exceed the limit of {max_inputs}",
                self.inputs.len()
            )));
        }
        for (index, input) in self.inputs.iter().enumerate() {
            if input.trim().is_empty() {
                return Err(EmbeddingError::invalid_request(format!(
                    "input {index} is empty"
                )));
            }
            if input.len() > max_bytes {
                return Err(EmbeddingError::invalid_request(format!(
                    "input {index} is {} bytes, over the limit of {max_bytes}",
                    input.len()
                )));
            }
        }
        Ok(())
    }
}

/// Token accounting reported by the provider (zero when it reports none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

/// One vector per request input, in input order.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub usage: EmbeddingUsage,
}

impl EmbeddingResponse {
    /// Check a provider response against the request it answers: one vector
    /// per input, each exactly `dimension` finite floats.
    pub fn validate(&self, expected_inputs: usize, dimension: u32) -> Result<(), EmbeddingError> {
        if self.embeddings.len() != expected_inputs {
            return Err(EmbeddingError::invalid_response(format!(
                "{} vectors for {expected_inputs} inputs",
                self.embeddings.len()
            )));
        }
        for (index, vector) in self.embeddings.iter().enumerate() {
            if vector.len() != dimension as usize {
                return Err(EmbeddingError::invalid_response(format!(
                    "vector {index} has dimension {}, expected {dimension}",
                    vector.len()
                )));
            }
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(EmbeddingError::invalid_response(format!(
                    "vector {index} holds a non-finite value"
                )));
            }
        }
        Ok(())
    }
}

/// Where an embedding provider gets its API key, read per call so a key the
/// operator sets or rotates applies without a restart. Composition implements
/// it over the operator secret store; `Ok(None)` means no key is stored.
#[async_trait]
pub trait EmbeddingCredentialSource: Send + Sync {
    async fn api_key(&self) -> Result<Option<SecretString>, EmbeddingError>;
}

/// A fixed credential (or none), for keyless local endpoints and tests.
pub struct StaticEmbeddingCredential(pub Option<SecretString>);

#[async_trait]
impl EmbeddingCredentialSource for StaticEmbeddingCredential {
    async fn api_key(&self) -> Result<Option<SecretString>, EmbeddingError> {
        Ok(self.0.clone())
    }
}

/// A text-embedding model behind one provider endpoint.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// The model id sent to the provider (and reported to API callers).
    fn model_name(&self) -> &str;

    /// Every vector this provider returns has exactly this many floats.
    fn dimension(&self) -> u32;

    /// Request bounds this provider accepts.
    fn limits(&self) -> EmbeddingLimits;

    /// Identity of the vector space this provider produces: provider, model,
    /// dimension, and every request parameter that changes the vectors. Two
    /// providers with equal fingerprints produce comparable vectors; vectors
    /// stored under one fingerprint must never be ranked against a query
    /// embedded under another (two 1536-dim models are different spaces).
    fn fingerprint(&self) -> String;

    /// Embed a validated batch. Implementations call
    /// [`EmbeddingRequest::validate`] before I/O and
    /// [`EmbeddingResponse::validate`] before returning.
    async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse, EmbeddingError>;
}

/// Operator configuration for the deployment's embedding model: which
/// registry provider serves it, which model, and optionally its dimension and
/// a base-URL override. Absent configuration means no embedding provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingsConfig {
    /// Registry provider id (or alias) whose OpenAI-compatible endpoint and
    /// stored API key serve embeddings, e.g. `openai`, `ollama`.
    pub provider_id: String,
    /// Embedding model id, e.g. `text-embedding-3-small`.
    pub model: String,
    /// Vector dimension. Inferred for well-known models when absent; required
    /// otherwise.
    pub dimension: Option<u32>,
    /// Base URL override (the OpenAI-compatible API root, ending in `/v1` or
    /// the provider's equivalent). Defaults to the provider's registry base.
    pub base_url: Option<String>,
}

/// A configured embedding endpoint after registry resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEmbeddingEndpoint {
    /// Canonical registry id (aliases folded).
    pub provider_id: String,
    pub model: String,
    pub dimension: u32,
    /// API root without a trailing slash; requests go to `{base_url}/embeddings`.
    /// Must be `https`, or `http` only to a loopback host
    /// ([`validate_embedding_base_url`]); the provider constructor re-checks.
    pub base_url: String,
    /// Whether the registry says this provider needs an API key.
    pub api_key_required: bool,
    /// Sent upstream as the OpenAI `dimensions` parameter: set when the
    /// configured dimension differs from the model's native one (only models
    /// that support shortened vectors accept that), `None` otherwise.
    pub request_dimensions: Option<u32>,
}

impl ResolvedEmbeddingEndpoint {
    /// The vector-space identity of this endpoint (see
    /// [`EmbeddingProvider::fingerprint`]). The base URL is deliberately not
    /// part of it: the same model behind another host is the same space.
    pub fn fingerprint(&self) -> String {
        let parameters = self
            .request_dimensions
            .map_or_else(|| "native".to_string(), |dims| format!("dimensions={dims}"));
        format!(
            "{}:{}:{}:{parameters}",
            self.provider_id, self.model, self.dimension
        )
    }
}

/// Check an embedding API root before anything is sent to it: `http(s)`, no
/// userinfo, query or fragment, and plaintext `http` only to a loopback host
/// (a local Ollama / LM Studio) — the request carries the provider's API key
/// in a header, which must never cross the network unencrypted.
pub fn validate_embedding_base_url(base_url: &str) -> Result<url::Url, EmbeddingError> {
    let url = url::Url::parse(base_url)
        .map_err(|error| EmbeddingError::invalid_config(format!("base URL: {error}")))?;
    match url.scheme() {
        "https" => {}
        "http" if url.host().is_some_and(|host| is_loopback_url_host(&host)) => {}
        "http" => {
            return Err(EmbeddingError::invalid_config(
                "base URL must be https (plain http is allowed only for a loopback host)",
            ));
        }
        _ => {
            return Err(EmbeddingError::invalid_config(
                "base URL must be http or https",
            ));
        }
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EmbeddingError::invalid_config(
            "base URL must not carry credentials, a query, or a fragment",
        ));
    }
    Ok(url)
}

fn is_loopback_url_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

impl EmbeddingsConfig {
    /// Resolve against the provider registry: the provider must speak an
    /// OpenAI-compatible protocol, and a base URL and dimension must be known.
    pub fn resolve(
        &self,
        registry: &ProviderRegistry,
    ) -> Result<ResolvedEmbeddingEndpoint, EmbeddingError> {
        let model = self.model.trim();
        if model.is_empty() {
            return Err(EmbeddingError::invalid_config("model is empty"));
        }
        let definition = registry.find(self.provider_id.trim()).ok_or_else(|| {
            EmbeddingError::invalid_config(format!(
                "unknown provider `{}`",
                self.provider_id.trim()
            ))
        })?;
        let protocol_base = match definition.protocol {
            ProviderProtocol::OpenAiCompletions => definition
                .default_base_url
                .clone()
                .or_else(|| (definition.id == "openai").then(|| OPENAI_API_ROOT.to_string())),
            // Ollama serves the OpenAI-compatible surface under `/v1`.
            ProviderProtocol::Ollama => definition
                .default_base_url
                .as_deref()
                .map(|base| format!("{}/v1", base.trim_end_matches('/'))),
            ProviderProtocol::OpenRouter => Some(OPENROUTER_API_ROOT.to_string()),
            other => {
                return Err(EmbeddingError::invalid_config(format!(
                    "provider `{}` speaks {other:?}, which has no OpenAI-compatible \
                     embeddings endpoint",
                    definition.id
                )));
            }
        };
        let base_url = self
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(ToString::to_string)
            .or(protocol_base)
            .ok_or_else(|| {
                EmbeddingError::invalid_config(format!(
                    "provider `{}` has no default base URL; set one",
                    definition.id
                ))
            })?;
        let base_url = base_url.trim_end_matches('/').to_string();
        validate_embedding_base_url(&base_url)?;
        let native = known_model_dimension(model);
        let dimension = match self.dimension {
            Some(dimension) => dimension,
            None => native.ok_or_else(|| {
                EmbeddingError::invalid_config(format!(
                    "dimension is unknown for model `{model}`; set it"
                ))
            })?,
        };
        if dimension == 0 || dimension > MAX_EMBEDDING_DIMENSION {
            return Err(EmbeddingError::invalid_config(format!(
                "dimension {dimension} is outside 1..={MAX_EMBEDDING_DIMENSION}"
            )));
        }
        // A dimension other than the model's native one must be requested
        // upstream, or every response comes back native-sized and fails
        // validation. Only models with shortenable vectors accept that.
        let request_dimensions = match native {
            Some(native) if native != dimension => {
                if !supports_shortened_dimensions(model) {
                    return Err(EmbeddingError::invalid_config(format!(
                        "model `{model}` always returns {native}-dimension vectors; \
                         dimension {dimension} cannot be requested"
                    )));
                }
                if dimension > native {
                    return Err(EmbeddingError::invalid_config(format!(
                        "dimension {dimension} exceeds model `{model}`'s native {native}"
                    )));
                }
                Some(dimension)
            }
            _ => None,
        };
        Ok(ResolvedEmbeddingEndpoint {
            provider_id: definition.id.clone(),
            model: model.to_string(),
            dimension,
            base_url,
            api_key_required: definition.api_key_required,
            request_dimensions,
        })
    }
}

const OPENAI_API_ROOT: &str = "https://api.openai.com/v1";
const OPENROUTER_API_ROOT: &str = "https://openrouter.ai/api/v1";

fn bare_model_name(model: &str) -> &str {
    let name = model.rsplit('/').next().unwrap_or(model);
    name.split(':').next().unwrap_or(name)
}

/// Models that accept the OpenAI `dimensions` request parameter (shortened
/// vectors). Every other known model returns its native dimension only.
fn supports_shortened_dimensions(model: &str) -> bool {
    matches!(
        bare_model_name(model),
        "text-embedding-3-small" | "text-embedding-3-large"
    )
}

/// Native dimension of well-known embedding models.
pub fn known_model_dimension(model: &str) -> Option<u32> {
    match bare_model_name(model) {
        "text-embedding-3-small" | "text-embedding-ada-002" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        "nomic-embed-text" => Some(768),
        "mxbai-embed-large" => Some(1024),
        "all-minilm" => Some(384),
        "bge-m3" => Some(1024),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;

    fn scope() -> ResourceScope {
        ResourceScope::system()
    }

    fn config(provider: &str, model: &str) -> EmbeddingsConfig {
        EmbeddingsConfig {
            provider_id: provider.to_string(),
            model: model.to_string(),
            dimension: None,
            base_url: None,
        }
    }

    #[test]
    fn request_validation_rejects_empty_oversized_and_blank_inputs() {
        let limits = EmbeddingLimits {
            max_inputs: 2,
            max_input_bytes: 8,
        };
        assert!(
            EmbeddingRequest::new(scope(), vec![])
                .validate(limits)
                .is_err()
        );
        assert!(
            EmbeddingRequest::new(scope(), vec!["a".into(), "b".into(), "c".into()])
                .validate(limits)
                .is_err()
        );
        assert!(
            EmbeddingRequest::new(scope(), vec!["  ".into()])
                .validate(limits)
                .is_err()
        );
        assert!(
            EmbeddingRequest::new(scope(), vec!["123456789".into()])
                .validate(limits)
                .is_err()
        );
        assert!(
            EmbeddingRequest::new(scope(), vec!["fine".into()])
                .validate(limits)
                .is_ok()
        );
    }

    #[test]
    fn provider_limits_cannot_exceed_the_hard_ceilings() {
        let generous = EmbeddingLimits {
            max_inputs: usize::MAX,
            max_input_bytes: usize::MAX,
        };
        let too_many = vec!["x".to_string(); MAX_EMBEDDING_INPUTS + 1];
        assert!(
            EmbeddingRequest::new(scope(), too_many)
                .validate(generous)
                .is_err()
        );
        let too_long = vec!["x".repeat(MAX_EMBEDDING_INPUT_BYTES + 1)];
        assert!(
            EmbeddingRequest::new(scope(), too_long)
                .validate(generous)
                .is_err()
        );
    }

    #[test]
    fn response_validation_checks_count_dimension_and_finiteness() {
        let ok = EmbeddingResponse {
            embeddings: vec![vec![0.1, 0.2]],
            usage: EmbeddingUsage::default(),
        };
        assert!(ok.validate(1, 2).is_ok());
        assert!(ok.validate(2, 2).is_err());
        assert!(ok.validate(1, 3).is_err());
        let nan = EmbeddingResponse {
            embeddings: vec![vec![f32::NAN, 0.0]],
            usage: EmbeddingUsage::default(),
        };
        assert!(nan.validate(1, 2).is_err());
    }

    #[test]
    fn resolve_openai_infers_root_and_dimension() {
        let registry = ProviderRegistry::load_from_path(None);
        let endpoint = config("openai", "text-embedding-3-small")
            .resolve(&registry)
            .expect("openai resolves");
        assert_eq!(endpoint.base_url, "https://api.openai.com/v1");
        assert_eq!(endpoint.dimension, 1536);
        assert!(endpoint.api_key_required);
    }

    #[test]
    fn resolve_ollama_appends_v1_and_honours_explicit_dimension() {
        let registry = ProviderRegistry::load_from_path(None);
        let mut cfg = config("ollama", "custom-embedder");
        cfg.dimension = Some(512);
        let endpoint = cfg.resolve(&registry).expect("ollama resolves");
        assert_eq!(endpoint.base_url, "http://localhost:11434/v1");
        assert_eq!(endpoint.dimension, 512);
    }

    #[test]
    fn resolve_fails_closed_on_unknown_or_non_openai_providers_and_bad_urls() {
        let registry = ProviderRegistry::load_from_path(None);
        assert!(config("nope", "m").resolve(&registry).is_err());
        assert!(
            config("anthropic", "text-embedding-3-small")
                .resolve(&registry)
                .is_err()
        );
        // Unknown model with no explicit dimension.
        assert!(config("openai", "mystery").resolve(&registry).is_err());
        let mut bad_url = config("openai", "text-embedding-3-small");
        bad_url.base_url = Some("https://user:pw@example.com/v1".to_string());
        assert!(bad_url.resolve(&registry).is_err());
        let mut zero = config("openai", "text-embedding-3-small");
        zero.dimension = Some(0);
        assert!(zero.resolve(&registry).is_err());
    }

    #[test]
    fn plaintext_http_is_rejected_for_any_non_loopback_host() {
        let registry = ProviderRegistry::load_from_path(None);
        for url in [
            "http://api.example.com/v1",
            "http://203.0.113.7/v1",
            "http://localhost.example.com/v1",
        ] {
            let mut cfg = config("openai", "text-embedding-3-small");
            cfg.base_url = Some(url.to_string());
            assert!(cfg.resolve(&registry).is_err(), "{url} must be rejected");
        }
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080/v1",
            "https://api.example.com/v1",
        ] {
            let mut cfg = config("openai", "text-embedding-3-small");
            cfg.base_url = Some(url.to_string());
            assert!(cfg.resolve(&registry).is_ok(), "{url} must be accepted");
        }
    }

    #[test]
    fn base_url_with_a_fragment_is_rejected() {
        let registry = ProviderRegistry::load_from_path(None);
        let mut cfg = config("openai", "text-embedding-3-small");
        cfg.base_url = Some("https://api.example.com/v1#x".to_string());
        assert!(cfg.resolve(&registry).is_err());
        assert!(validate_embedding_base_url("https://api.example.com/v1#").is_err());
    }

    #[test]
    fn a_shortened_dimension_is_requested_upstream_or_rejected() {
        let registry = ProviderRegistry::load_from_path(None);
        let mut short = config("openai", "text-embedding-3-small");
        short.dimension = Some(512);
        let endpoint = short.resolve(&registry).expect("3-small shortens");
        assert_eq!(endpoint.dimension, 512);
        assert_eq!(endpoint.request_dimensions, Some(512));

        let mut native = config("openai", "text-embedding-3-small");
        native.dimension = Some(1536);
        assert_eq!(
            native.resolve(&registry).unwrap().request_dimensions,
            None,
            "the native dimension is never sent"
        );
        // A model with no `dimensions` support, or a dimension above native,
        // fails startup instead of failing every request.
        let mut ada = config("openai", "text-embedding-ada-002");
        ada.dimension = Some(512);
        assert!(ada.resolve(&registry).is_err());
        let mut larger = config("openai", "text-embedding-3-small");
        larger.dimension = Some(2048);
        assert!(larger.resolve(&registry).is_err());
        // An unknown model's configured dimension is its native one.
        let mut custom = config("ollama", "custom-embedder");
        custom.dimension = Some(512);
        assert_eq!(custom.resolve(&registry).unwrap().request_dimensions, None);
    }

    #[test]
    fn fingerprint_separates_models_dimensions_and_parameters() {
        let registry = ProviderRegistry::load_from_path(None);
        let small = config("openai", "text-embedding-3-small")
            .resolve(&registry)
            .unwrap();
        let ada = config("openai", "text-embedding-ada-002")
            .resolve(&registry)
            .unwrap();
        let mut short_cfg = config("openai", "text-embedding-3-small");
        short_cfg.dimension = Some(512);
        let short = short_cfg.resolve(&registry).unwrap();
        // Same dimension, different model: different spaces.
        assert_eq!(small.dimension, ada.dimension);
        assert_ne!(small.fingerprint(), ada.fingerprint());
        assert_ne!(small.fingerprint(), short.fingerprint());
        // The host is not part of the space.
        let mut moved_cfg = config("openai", "text-embedding-3-small");
        moved_cfg.base_url = Some("https://proxy.example.com/v1".to_string());
        assert_eq!(
            moved_cfg.resolve(&registry).unwrap().fingerprint(),
            small.fingerprint()
        );
    }

    #[test]
    fn known_dimensions_fold_namespaces_and_tags() {
        assert_eq!(
            known_model_dimension("openai/text-embedding-3-large"),
            Some(3072)
        );
        assert_eq!(known_model_dimension("nomic-embed-text:latest"), Some(768));
        assert_eq!(known_model_dimension("unknown"), None);
    }
}
