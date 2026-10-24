//! Per-thread provider routing for IronClaw's own loop.
//!
//! [`RebornProviderRoutes`] is both halves of it: the source the model
//! gateway's `ProviderRouter` asks for a provider's client, and the list of
//! routable providers with their models that the model picker shows. A
//! provider is routable when the operator stored a key for it.
//!
//! Clients and model lists are cached for [`CACHE_TTL`], so a key or catalog
//! edit reaches new turns within that window without a restart.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironclaw_config::{LlmSlotSelection, RebornBootConfig};
use ironclaw_llm::provider_routing::RoutedProviderSource;
use ironclaw_llm::reasoning_effort;
use ironclaw_llm::registry::{ProviderDefinition, ProviderProtocol, ProviderRegistry};
use ironclaw_llm::{LlmError, LlmProvider, SessionManager};
use ironclaw_product_contracts::operator_llm::{NativeModelRoute, NativeModelRoutes};

use crate::LlmKeyStore;
use crate::llm_admin::llm_catalog::{
    apply_stored_api_key, resolve_llm_selection_allow_missing_key,
};

const CACHE_TTL: Duration = Duration::from_secs(600);
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(8);

type Cache<T> = Mutex<HashMap<String, (Instant, T)>>;

pub struct RebornProviderRoutes {
    providers_path: PathBuf,
    config_path: PathBuf,
    keys: LlmKeyStore,
    session: Arc<SessionManager>,
    clients: Cache<Arc<dyn LlmProvider>>,
    models: Cache<Vec<String>>,
}

impl RebornProviderRoutes {
    pub fn new(boot: &RebornBootConfig, keys: LlmKeyStore, session: Arc<SessionManager>) -> Self {
        Self {
            providers_path: boot.home().providers_file_path(),
            config_path: boot.home().config_file_path(),
            keys,
            session,
            clients: Mutex::default(),
            models: Mutex::default(),
        }
    }

    fn registry(&self) -> Result<ProviderRegistry, String> {
        ProviderRegistry::try_load_from_path(Some(self.providers_path.as_path()))
            .map_err(|error| error.to_string())
    }

    async fn route_for(&self, def: &ProviderDefinition) -> NativeModelRoute {
        let name = def
            .setup
            .as_ref()
            .map(|setup| setup.display_name().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| def.description.clone());
        let models = self.models_for(def).await;
        let (efforts, model_efforts) = efforts_for(def, &models);
        NativeModelRoute {
            provider_id: def.id.clone(),
            name,
            models,
            efforts,
            model_efforts,
        }
    }

    async fn models_for(&self, def: &ProviderDefinition) -> Vec<String> {
        if let Some(hit) = cached(&self.models, &def.id) {
            return hit;
        }
        let listed = match self.provider(&def.id).await {
            Ok(client) => tokio::time::timeout(LIST_MODELS_TIMEOUT, client.list_models()).await,
            Err(error) => Ok(Err(error)),
        };
        let mut models = match listed {
            Ok(Ok(models)) => models,
            Ok(Err(error)) => {
                tracing::debug!(provider_id = %def.id, %error, "model list unavailable; using the default model");
                Vec::new()
            }
            Err(_) => {
                tracing::debug!(provider_id = %def.id, "model list timed out; using the default model");
                Vec::new()
            }
        };
        if !models.contains(&def.default_model) {
            models.insert(0, def.default_model.clone());
        }
        store(&self.models, &def.id, models.clone());
        models
    }
}

/// The efforts of a provider's default model, and of each listed model
/// (`ironclaw_llm` knows which levels each model takes on each protocol).
fn efforts_for(
    def: &ProviderDefinition,
    models: &[String],
) -> (Vec<String>, BTreeMap<String, Vec<String>>) {
    let of = |model: &str| reasoning_effort::efforts_for(def.protocol, &def.id, model);
    let per_model = models.iter().map(|m| (m.clone(), of(m))).collect();
    (of(&def.default_model), per_model)
}

fn cached<T: Clone>(cache: &Cache<T>, key: &str) -> Option<T> {
    let map = cache.lock().unwrap_or_else(PoisonError::into_inner);
    map.get(key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, value)| value.clone())
}

fn store<T>(cache: &Cache<T>, key: &str, value: T) {
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key.to_string(), (Instant::now(), value));
}

fn route_error(provider_id: &str, reason: impl Into<String>) -> LlmError {
    LlmError::RequestFailed {
        provider: provider_id.to_string(),
        reason: reason.into(),
    }
}

#[async_trait]
impl RoutedProviderSource for RebornProviderRoutes {
    async fn provider(&self, provider_id: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
        if let Some(hit) = cached(&self.clients, provider_id) {
            return Ok(hit);
        }
        let selection = LlmSlotSelection {
            provider_id: Some(provider_id.to_string()),
            ..Default::default()
        };
        let mut config = resolve_llm_selection_allow_missing_key(
            &selection,
            Some(self.providers_path.as_path()),
        )
        .map_err(|error| route_error(provider_id, error.to_string()))?;
        let key = self
            .keys
            .read(provider_id)
            .await
            .map_err(|error| route_error(provider_id, error.to_string()))?;
        if let Some(key) = key {
            apply_stored_api_key(&mut config, key);
        }
        let (client, ..) =
            ironclaw_llm::build_provider_chain(&config, Arc::clone(&self.session)).await?;
        store(&self.clients, provider_id, Arc::clone(&client));
        Ok(client)
    }
}

#[async_trait]
impl NativeModelRoutes for RebornProviderRoutes {
    async fn routes(&self) -> Vec<NativeModelRoute> {
        // silent-ok: the picker shows the active provider alone when the
        // catalog or the key store cannot be read; the cause is logged.
        let ids = match self.keys.stored_provider_ids().await {
            Ok(ids) => ids,
            Err(error) => {
                tracing::debug!(%error, "routable providers: key store unavailable");
                return Vec::new();
            }
        };
        let registry = match self.registry() {
            Ok(registry) => registry,
            Err(error) => {
                tracing::debug!(%error, "routable providers: catalog unavailable");
                return Vec::new();
            }
        };
        let mut routes = Vec::new();
        // `all()` keeps both a built-in and a `providers.json` entry with the
        // same id; `find` returns the one that wins, so each id lists once.
        let mut seen = std::collections::HashSet::new();
        let defs = registry
            .all()
            .iter()
            .filter(|def| ids.contains(&def.id) && seen.insert(def.id.clone()))
            .filter_map(|def| registry.find(&def.id));
        for def in defs {
            routes.push(self.route_for(def).await);
        }
        routes.sort_by_key(|route| route.name.to_lowercase());
        routes
    }

    async fn primary(&self) -> Option<NativeModelRoute> {
        // silent-ok: the picker offers the active provider's default model
        // alone when its config or the catalog cannot be read.
        let active = ironclaw_config::RebornConfigFile::load(&self.config_path)
            .ok()
            .flatten()
            .and_then(|file| file.default_llm_slot().cloned())?;
        let registry = self.registry().ok()?;
        let def = registry.find(active.provider_id.as_deref()?)?;
        if def.protocol == ProviderProtocol::OpenAiCodex {
            // A second plan client could refresh (and rotate) the OAuth
            // token under the live one; the plan's list needs no client.
            let configured = active.model.as_deref().unwrap_or(&def.default_model);
            let models = ironclaw_llm::openai_codex_provider::plan_models(configured).await;
            let (efforts, model_efforts) = efforts_for(def, &models);
            return Some(NativeModelRoute {
                provider_id: def.id.clone(),
                name: def.description.clone(),
                models,
                efforts,
                model_efforts,
            });
        }
        Some(self.route_for(def).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_llm::provider_routing::ModelRoute;

    #[test]
    fn each_listed_model_carries_the_levels_it_takes() {
        let registry = ProviderRegistry::try_load_from_path(None).unwrap();
        let anthropic = registry.find("anthropic").unwrap();
        let models = vec![
            "claude-opus-5-5".to_string(),
            "claude-3-5-haiku".to_string(),
        ];
        let (default_efforts, per_model) = efforts_for(anthropic, &models);
        assert!(
            default_efforts.is_empty(),
            "the catalog default (Sonnet 4) has no effort knob"
        );
        assert_eq!(
            per_model["claude-opus-5-5"],
            ["low", "medium", "high", "max"]
        );
        assert!(per_model["claude-3-5-haiku"].is_empty());
    }

    /// The product side encodes the turn model, the LLM router decodes it;
    /// this crate sees both, so it pins that they agree.
    #[test]
    fn the_product_encoding_is_what_the_router_decodes() {
        let encoded = NativeModelRoute::turn_model(Some("glm"), Some("glm-5.3"), None).unwrap();
        let route = ModelRoute::parse(&encoded).expect("router decodes the product encoding");
        assert_eq!(
            (route.provider_id(), route.model()),
            (Some("glm"), Some("glm-5.3"))
        );
        assert_eq!(route.encode(), encoded);
        assert_eq!(NativeModelRoute::plain_model(&encoded), Some("glm-5.3"));

        let effort = NativeModelRoute::turn_model(None, None, Some("high")).unwrap();
        let route = ModelRoute::parse(&effort).expect("router decodes an effort-only pick");
        assert_eq!((route.provider_id(), route.model()), (None, None));
        assert_eq!(route.effort(), Some("high"));
        assert_eq!(NativeModelRoute::plain_model(&effort), None);
    }
}
