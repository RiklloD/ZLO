//! Assembly of the deployment's embedding provider and the adapters that hand
//! it to its consumers (the OpenAI-compatible `/v1/embeddings` port today).
//!
//! Deployment shape only: the `[embeddings]` section picks a registry provider
//! and model; `ironclaw_llm` resolves and implements it; the API key is the
//! operator-stored key of that provider, read through `LlmKeyStore` per call
//! (cached briefly). No section means no provider and no consumer changes
//! behavior.
//!
//! The memory provider is resolved before durable storage exists, so the key
//! store is *bound late*: [`EmbeddingAssembly::bind_key_store`] runs once the
//! runtime's secret store is built. A call before that fails with
//! `CredentialUnavailable` — never with a silently keyless request.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironclaw_host_api::ids::InvocationId;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_llm::ProviderRegistry;
use ironclaw_llm::embeddings::{
    EmbeddingCredentialSource, EmbeddingError, EmbeddingProvider, EmbeddingRequest,
    EmbeddingsConfig, OpenAiCompatibleEmbeddings,
};
use ironclaw_openai_compat::{
    OpenAiCompatAuthenticatedCaller, OpenAiCompatEmbeddingBatch, OpenAiCompatEmbeddingModel,
    OpenAiCompatEmbeddings, OpenAiCompatErrorKind, OpenAiCompatHttpError,
};
use ironclaw_operator::LlmKeyStore;
use secrecy::SecretString;

use crate::RebornBuildError;

const LOG_TARGET: &str = "ironclaw_reborn::embeddings";
/// How long a read key is reused before the store is consulted again.
const KEY_CACHE_TTL: Duration = Duration::from_secs(30);

/// The built embedding provider plus the late-bound key slot it reads from.
#[derive(Clone)]
pub(crate) struct EmbeddingAssembly {
    provider: Arc<dyn EmbeddingProvider>,
    keys: Arc<OnceLock<LlmKeyStore>>,
}

impl std::fmt::Debug for EmbeddingAssembly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingAssembly")
            .field("model", &self.provider.model_name())
            .field("dimension", &self.provider.dimension())
            .field("key_store_bound", &self.keys.get().is_some())
            .finish()
    }
}

impl EmbeddingAssembly {
    pub(crate) fn provider(&self) -> Arc<dyn EmbeddingProvider> {
        Arc::clone(&self.provider)
    }

    /// Bind the operator key store once the runtime's secret store exists.
    /// Idempotent: a second bind is ignored.
    pub(crate) fn bind_key_store(&self, keys: LlmKeyStore) {
        let _ = self.keys.set(keys); // silent-ok: first bind wins; every build binds the same runtime secret store
    }
}

/// Resolve `[embeddings]` against the provider registry and build the
/// production provider. A section that does not resolve fails startup.
pub(crate) fn build_embedding_assembly(
    section: &ironclaw_config::EmbeddingsSection,
    providers_file: Option<&Path>,
) -> Result<EmbeddingAssembly, RebornBuildError> {
    let registry = ProviderRegistry::load_from_path(providers_file);
    let config = EmbeddingsConfig {
        provider_id: section.provider.clone(),
        model: section.model.clone(),
        dimension: section.dimension,
        base_url: section.base_url.clone(),
    };
    let invalid = |error: EmbeddingError| RebornBuildError::InvalidConfig {
        reason: format!("[embeddings]: {error}"),
    };
    let endpoint = config.resolve(&registry).map_err(invalid)?;
    let keys = Arc::new(OnceLock::new());
    let credentials = Arc::new(OperatorEmbeddingKeySource {
        provider_id: endpoint.provider_id.clone(),
        keys: Arc::clone(&keys),
        cache: Mutex::new(None),
    });
    tracing::debug!(
        target: LOG_TARGET,
        provider = %endpoint.provider_id,
        model = %endpoint.model,
        dimension = endpoint.dimension,
        "embedding provider configured"
    );
    let provider =
        OpenAiCompatibleEmbeddings::production(endpoint, credentials).map_err(invalid)?;
    Ok(EmbeddingAssembly {
        provider: Arc::new(provider),
        keys,
    })
}

struct CachedKey {
    read_at: Instant,
    key: Option<SecretString>,
}

/// The operator-stored API key of the embedding provider's registry id.
struct OperatorEmbeddingKeySource {
    provider_id: String,
    keys: Arc<OnceLock<LlmKeyStore>>,
    cache: Mutex<Option<CachedKey>>,
}

#[async_trait]
impl EmbeddingCredentialSource for OperatorEmbeddingKeySource {
    async fn api_key(&self) -> Result<Option<SecretString>, EmbeddingError> {
        {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = cache.as_ref()
                && cached.read_at.elapsed() < KEY_CACHE_TTL
            {
                return Ok(cached.key.clone());
            }
        }
        let Some(keys) = self.keys.get() else {
            return Err(EmbeddingError::CredentialUnavailable {
                reason: "the secret store is not ready".to_string(),
            });
        };
        let key = keys.read(&self.provider_id).await.map_err(|error| {
            EmbeddingError::CredentialUnavailable {
                reason: error.to_string(),
            }
        })?;
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = Some(CachedKey {
            read_at: Instant::now(),
            key: key.clone(),
        });
        Ok(key)
    }
}

/// Native memory's embedding port over the deployment's embedding provider.
struct MemoryEmbedderAdapter {
    provider: Arc<dyn EmbeddingProvider>,
    fingerprint: ironclaw_memory_native::MemoryEmbeddingFingerprint,
}

/// The memory port over `provider`. Fails startup when the provider's
/// vector-space fingerprint is not storable (empty, too long, control chars).
pub(crate) fn memory_embedder(
    provider: Arc<dyn EmbeddingProvider>,
) -> Result<Arc<dyn ironclaw_memory_native::MemoryEmbedder>, RebornBuildError> {
    let fingerprint = ironclaw_memory_native::MemoryEmbeddingFingerprint::new(
        provider.fingerprint(),
    )
    .map_err(|error| RebornBuildError::InvalidConfig {
        reason: format!("[embeddings]: {error}"),
    })?;
    Ok(Arc::new(MemoryEmbedderAdapter {
        provider,
        fingerprint,
    }))
}

fn memory_resource_scope(
    scope: &ironclaw_memory::MemoryDocumentScope,
) -> Result<ResourceScope, ironclaw_memory_native::MemoryEmbeddingError> {
    use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
    let invalid = |error: ironclaw_host_api::error::HostApiError| {
        ironclaw_memory_native::MemoryEmbeddingError::new(format!("memory scope: {error}"))
    };
    Ok(ResourceScope {
        tenant_id: TenantId::new(scope.tenant_id()).map_err(invalid)?,
        user_id: UserId::new(scope.user_id()).map_err(invalid)?,
        agent_id: scope
            .agent_id()
            .map(AgentId::new)
            .transpose()
            .map_err(invalid)?,
        project_id: scope
            .project_id()
            .map(ProjectId::new)
            .transpose()
            .map_err(invalid)?,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    })
}

#[async_trait]
impl ironclaw_memory_native::MemoryEmbedder for MemoryEmbedderAdapter {
    fn dimension(&self) -> u32 {
        self.provider.dimension()
    }

    fn max_batch(&self) -> usize {
        self.provider.limits().max_inputs
    }

    fn max_input_bytes(&self) -> usize {
        self.provider.limits().max_input_bytes
    }

    fn fingerprint(&self) -> ironclaw_memory_native::MemoryEmbeddingFingerprint {
        self.fingerprint.clone()
    }

    async fn embed(
        &self,
        scope: &ironclaw_memory::MemoryDocumentScope,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, ironclaw_memory_native::MemoryEmbeddingError> {
        let scope = memory_resource_scope(scope)?;
        self.provider
            .embed(EmbeddingRequest::new(scope, texts))
            .await
            .map(|response| response.embeddings)
            .map_err(|error| ironclaw_memory_native::MemoryEmbeddingError::new(error.to_string()))
    }
}

/// `POST /v1/embeddings` port over the deployment's embedding provider.
pub(crate) struct OpenAiCompatEmbeddingsAdapter {
    provider: Arc<dyn EmbeddingProvider>,
}

impl OpenAiCompatEmbeddingsAdapter {
    pub(crate) fn new(provider: Arc<dyn EmbeddingProvider>) -> Self {
        Self { provider }
    }
}

/// The `/v1/embeddings` port over an embedding provider — what
/// `build_openai_compat_route_mount` mounts, exposed so the integration tier
/// can mount the same adapter over a deterministic provider.
pub fn openai_compat_embeddings_port(
    provider: Arc<dyn EmbeddingProvider>,
) -> Arc<dyn OpenAiCompatEmbeddings> {
    Arc::new(OpenAiCompatEmbeddingsAdapter::new(provider))
}

fn caller_resource_scope(caller: &OpenAiCompatAuthenticatedCaller) -> ResourceScope {
    let scope = caller.scope();
    ResourceScope {
        tenant_id: scope.tenant_id().clone(),
        user_id: scope.user_id().clone(),
        agent_id: scope.agent_id().cloned(),
        project_id: scope.project_id().cloned(),
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

/// Provider failures are the deployment's, not the caller's: they map to
/// sanitized 503/429 errors, with the cause kept server-side in a debug log.
fn map_embedding_error(error: EmbeddingError) -> OpenAiCompatHttpError {
    tracing::debug!(target: LOG_TARGET, %error, "embedding request failed");
    match error {
        EmbeddingError::InvalidRequest { .. } => OpenAiCompatHttpError::invalid_request(None),
        EmbeddingError::RateLimited { .. } => {
            OpenAiCompatHttpError::from_kind(429, true, OpenAiCompatErrorKind::RateLimited, None)
        }
        EmbeddingError::Http { .. }
        | EmbeddingError::Transport { .. }
        | EmbeddingError::InvalidResponse { .. } => OpenAiCompatHttpError::from_kind(
            503,
            true,
            OpenAiCompatErrorKind::ServiceUnavailable,
            None,
        ),
        EmbeddingError::MissingCredential { .. }
        | EmbeddingError::CredentialUnavailable { .. }
        | EmbeddingError::Unauthorized { .. }
        | EmbeddingError::InvalidConfig { .. } => OpenAiCompatHttpError::from_kind(
            503,
            false,
            OpenAiCompatErrorKind::ServiceUnavailable,
            None,
        ),
    }
}

#[async_trait]
impl OpenAiCompatEmbeddings for OpenAiCompatEmbeddingsAdapter {
    fn model(&self) -> OpenAiCompatEmbeddingModel {
        let limits = self.provider.limits();
        OpenAiCompatEmbeddingModel {
            id: self.provider.model_name().to_string(),
            dimension: self.provider.dimension(),
            max_inputs: limits.max_inputs,
            max_input_bytes: limits.max_input_bytes,
        }
    }

    async fn embed(
        &self,
        caller: &OpenAiCompatAuthenticatedCaller,
        inputs: Vec<String>,
    ) -> Result<OpenAiCompatEmbeddingBatch, OpenAiCompatHttpError> {
        let response = self
            .provider
            .embed(EmbeddingRequest::new(caller_resource_scope(caller), inputs))
            .await
            .map_err(map_embedding_error)?;
        Ok(OpenAiCompatEmbeddingBatch {
            embeddings: response.embeddings,
            prompt_tokens: response.usage.prompt_tokens,
            total_tokens: response.usage.total_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolvable_section_fails_startup_with_a_config_error() {
        let section = ironclaw_config::EmbeddingsSection {
            provider: "anthropic".to_string(),
            model: "text-embedding-3-small".to_string(),
            dimension: None,
            base_url: None,
        };
        let error = build_embedding_assembly(&section, None).unwrap_err();
        assert!(error.to_string().contains("[embeddings]"), "{error}");
    }

    #[tokio::test]
    async fn key_source_fails_closed_until_the_secret_store_is_bound() {
        let source = OperatorEmbeddingKeySource {
            provider_id: "openai".to_string(),
            keys: Arc::new(OnceLock::new()),
            cache: Mutex::new(None),
        };
        assert!(matches!(
            source.api_key().await,
            Err(EmbeddingError::CredentialUnavailable { .. })
        ));
    }

    #[test]
    fn provider_failures_never_surface_as_caller_errors() {
        let cases = [
            (EmbeddingError::RateLimited { retry_after: None }, 429),
            (EmbeddingError::Http { status: 500 }, 503),
            (EmbeddingError::Unauthorized { status: 401 }, 503),
            (
                EmbeddingError::MissingCredential {
                    provider_id: "openai".to_string(),
                },
                503,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(map_embedding_error(error).status_code(), status);
        }
    }
}
