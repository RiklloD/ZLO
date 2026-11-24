use std::sync::Arc;

use ironclaw_assistant::projection::LiveProjectionPublisher;
use ironclaw_extension_host::skill_learning::SkillLearnedNotifier;
use ironclaw_host_api::ids::UserId;
use ironclaw_turns::{TurnRunId, TurnScope};

use crate::runtime::RebornRuntimeError;

/// [`SkillLearnedNotifier`] over the runtime's live projection publisher —
/// emits a `SkillActivation` projection item rendered as a chat bubble.
///
/// The adapter lives here, not beside the port. `LiveProjectionPublisher` is
/// one of `ironclaw_assistant`'s *concrete* types, and naming it was the whole
/// of `ironclaw_extension_host::skill_learning`'s `ironclaw_assistant`
/// dependency — the last one that file carried, and the one that would have
/// blocked the crate's `products` → `loops` re-layer. The port's own doc always
/// said "composition implements it over the projection publisher"; this makes
/// that true (PROPOSAL §6.8.2 shed list, CHECKLIST WS2 strays row).
///
/// It sits in this module because composition's other skill-learning assembly
/// piece — `build_skill_learning_provider` below — already does. It is
/// deliberately *not* a crate-root `skill_learning` module: the whole module is
/// what #6616/#6691 moved out of composition, and
/// `reborn_composition_boundaries.rs` holds it out.
pub(crate) struct LiveSkillLearnedNotifier {
    publisher: Arc<LiveProjectionPublisher>,
}

impl LiveSkillLearnedNotifier {
    pub(crate) fn new(publisher: Arc<LiveProjectionPublisher>) -> Self {
        Self { publisher }
    }
}

impl SkillLearnedNotifier for LiveSkillLearnedNotifier {
    fn notify(
        &self,
        owner: &UserId,
        scope: &TurnScope,
        run_id: TurnRunId,
        skill_name: &str,
        feedback: &str,
    ) {
        self.publisher
            .publish_skill_learned(Some(owner), scope, run_id, skill_name, feedback);
    }
}

/// What the gateway needs to route a thread to another configured provider:
/// the boot config (for `providers.json`) and the operator key store.
pub(crate) struct ProviderRoutesInput {
    pub(crate) boot: ironclaw_config::RebornBootConfig,
    pub(crate) keys: ironclaw_operator::LlmKeyStore,
}

pub(crate) async fn build_production_model_gateway(
    provider_factory: Option<ironclaw_operator::RebornProviderFactory>,
    routes: Option<ProviderRoutesInput>,
    cheap_model: Option<String>,
) -> Result<
    (
        Arc<dyn ironclaw_loop_host::HostManagedModelGateway>,
        Option<ironclaw_loop_host::StaticModelCostTable>,
        Option<RebornLlmReloadParts>,
    ),
    RebornRuntimeError,
> {
    let LlmGatewayBundle {
        gateway, reload, ..
    } = build_placeholder_llm_gateway(provider_factory, routes, cheap_model).await?;
    Ok((gateway, None, Some(reload)))
}

pub(crate) async fn build_skill_learning_provider(
    config: &ironclaw_llm::LlmConfig,
) -> Option<(Arc<dyn ironclaw_llm::LlmProvider>, String)> {
    let model = std::env::var("IRONCLAW_SKILL_LEARNING_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())?;
    if !matches!(config.backend.as_str(), "nearai" | "near_ai" | "near") {
        tracing::debug!(
            backend = %config.backend,
            "skill-learning: learning model is only wired for the nearai backend; skill learning disabled"
        );
        return None;
    }
    let mut nearai = config.nearai.clone();
    nearai.model = model.clone();
    let session = ironclaw_llm::create_session_manager(config.session.clone()).await;
    match ironclaw_llm::create_llm_provider_with_config(
        &nearai,
        session,
        config.request_timeout_secs,
    ) {
        Ok(provider) => Some((provider, model)),
        Err(error) => {
            tracing::debug!(%error, "skill-learning: could not build the learning provider; skill learning disabled");
            None
        }
    }
}

pub(crate) struct LlmGatewayBundle {
    pub(crate) gateway: Arc<dyn ironclaw_loop_host::HostManagedModelGateway>,
    pub(crate) reload: RebornLlmReloadParts,
}

pub(crate) struct RebornLlmReloadParts {
    pub(crate) reload_handle: Arc<ironclaw_llm::LlmReloadHandle>,
    pub(crate) session: Arc<ironclaw_llm::SessionManager>,
    pub(crate) nearai_login_states:
        Arc<ironclaw_operator::llm_admin::llm_config_service::NearAiLoginStateStore>,
    /// Other configured providers a thread can run on; the model picker lists them.
    pub(crate) provider_routes: Option<Arc<ironclaw_operator::RebornProviderRoutes>>,
}

/// The single live `[llm.default]` owner for this runtime: the hot-swap
/// adapter wrapped in the lock and applied-slot record every apply path
/// shares. `None` without a boot config (no file to apply) or LLM seam.
pub(crate) fn live_llm_default(
    boot: Option<&ironclaw_config::RebornBootConfig>,
    parts: Option<&RebornLlmReloadParts>,
    secret_store: &Arc<dyn ironclaw_secrets::SecretStorePort>,
) -> Option<Arc<ironclaw_operator::LiveLlmDefault>> {
    let (boot, parts) = (boot?, parts?);
    let swap = ironclaw_operator::RebornLlmReloadAdapter::new(
        boot.clone(),
        Arc::clone(&parts.reload_handle),
        Arc::clone(&parts.session),
        ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
            Arc::clone(secret_store),
        )),
    );
    Some(Arc::new(ironclaw_operator::LiveLlmDefault::new(
        boot.home().config_file_path(),
        Arc::new(swap),
    )))
}

async fn build_placeholder_llm_gateway(
    provider_factory: Option<ironclaw_operator::RebornProviderFactory>,
    routes: Option<ProviderRoutesInput>,
    cheap_model: Option<String>,
) -> Result<LlmGatewayBundle, RebornRuntimeError> {
    let session =
        ironclaw_llm::create_session_manager(ironclaw_llm::SessionConfig::default()).await;
    let raw: Arc<dyn ironclaw_llm::LlmProvider> = Arc::new(PlaceholderLlmProvider);
    let routes = routes.map(|input| {
        Arc::new(ironclaw_operator::RebornProviderRoutes::new(
            &input.boot,
            input.keys,
            Arc::clone(&session),
        ))
    });
    wrap_swappable_gateway(raw, session, provider_factory, routes, cheap_model)
}

/// The cheap-slot model for lightweight subagent roles, from the boot LLM
/// config (`LLM_CHEAP_MODEL`, or `NEARAI_CHEAP_MODEL` on NEAR AI). `None` when
/// unset, blank, or when no LLM config was resolved at boot — the explorer and
/// planner profiles then run on the live active model. A configured but
/// invalid model id fails boot loudly rather than silently falling back.
pub(crate) fn boot_cheap_model(
    config: Option<&ironclaw_llm::LlmConfig>,
) -> Result<Option<String>, RebornRuntimeError> {
    let Some(config) = config else {
        return Ok(None);
    };
    ironclaw_loop_host::ActiveModelRouteSettings::cheap_from_llm_config(config)
        .map(|settings| settings.map(|settings| settings.model_id().to_string()))
        .map_err(|error| {
            RebornRuntimeError::LlmProvider(format!("invalid cheap model configuration: {error}"))
        })
}

/// Model profiles the production gateway serves. `interactive_model` always
/// runs on the live active model. `explorer_model` / `planner_model` (the
/// explorer and planner subagent flavors) run on `cheap_model` when one is
/// configured and otherwise fall back to the live active model, so an unset
/// `LLM_CHEAP_MODEL` changes nothing. Every other profile id stays denied
/// (fail closed). The cheap model is a model id on the active provider, read
/// at boot: after an operator switches provider, set `LLM_CHEAP_MODEL` for the
/// new provider (or unset it) and restart.
fn production_model_profile_policy(
    cheap_model: Option<String>,
) -> Result<ironclaw_loop_host::LlmModelProfilePolicy, RebornRuntimeError> {
    use ironclaw_loop_contracts::ModelProfileId;
    let profile = |id: &str| {
        ModelProfileId::new(id).map_err(|reason| {
            RebornRuntimeError::LlmProvider(format!("invalid model profile id `{id}`: {reason}"))
        })
    };
    let cheap_model = cheap_model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty());
    Ok(ironclaw_loop_host::LlmModelProfilePolicy::new()
        .allow_model_profile(profile("interactive_model")?, None)
        .allow_model_profile(profile("explorer_model")?, cheap_model.clone())
        .allow_model_profile(profile("planner_model")?, cheap_model))
}

/// Apply instrumentation outside the swappable provider so it survives reloads.
/// With `routes`, a turn whose model names another configured provider runs
/// there (`ProviderRouter`); instrumentation still wraps every call.
pub(crate) fn wrap_swappable_gateway(
    raw: Arc<dyn ironclaw_llm::LlmProvider>,
    session: Arc<ironclaw_llm::SessionManager>,
    provider_factory: Option<ironclaw_operator::RebornProviderFactory>,
    routes: Option<Arc<ironclaw_operator::RebornProviderRoutes>>,
    cheap_model: Option<String>,
) -> Result<LlmGatewayBundle, RebornRuntimeError> {
    use ironclaw_llm::provider_routing::ProviderRouter;
    use ironclaw_llm::{LlmProvider, LlmReloadHandle, SwappableLlmProvider};
    use ironclaw_loop_host::LlmProviderModelGateway;

    let swappable = Arc::new(SwappableLlmProvider::new(raw));
    let reload_handle = Arc::new(LlmReloadHandle::new(Arc::clone(&swappable), None));
    let swappable_provider: Arc<dyn LlmProvider> = match &routes {
        Some(routes) => Arc::new(ProviderRouter::new(swappable, Arc::clone(routes) as _)),
        None => swappable,
    };
    let provider: Arc<dyn LlmProvider> = match provider_factory {
        Some(factory) => factory(Arc::clone(&swappable_provider)),
        None => swappable_provider,
    };
    // Outermost, so every request the gateway makes is counted once whatever
    // the routed provider, and the wrap survives reloads with the rest.
    let provider = crate::observability::operator_metrics::metered_model_provider(
        provider,
        ironclaw_operator::operator_metrics(),
    );

    let policy = production_model_profile_policy(cheap_model)?;
    let gateway = LlmProviderModelGateway::new(provider, policy);
    Ok(LlmGatewayBundle {
        gateway: Arc::new(gateway),
        reload: RebornLlmReloadParts {
            reload_handle,
            session,
            nearai_login_states: Arc::new(
                ironclaw_operator::llm_admin::llm_config_service::NearAiLoginStateStore::new(),
            ),
            provider_routes: routes,
        },
    })
}

#[derive(Debug)]
struct PlaceholderLlmProvider;

#[async_trait::async_trait]
impl ironclaw_llm::LlmProvider for PlaceholderLlmProvider {
    fn model_name(&self) -> &str {
        "unconfigured"
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
    }

    async fn complete(
        &self,
        _request: ironclaw_llm::CompletionRequest,
    ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
        Err(placeholder_unconfigured_error())
    }

    async fn complete_with_tools(
        &self,
        _request: ironclaw_llm::ToolCompletionRequest,
    ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
        Err(placeholder_unconfigured_error())
    }
}

fn placeholder_unconfigured_error() -> ironclaw_llm::LlmError {
    ironclaw_llm::LlmError::RequestFailed {
        provider: ironclaw_llm::UNCONFIGURED_PROVIDER_ID.to_string(),
        reason: "no LLM provider is configured yet; choose one in Settings → Inference".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ironclaw_assistant::projection::build_reborn_projection_services;
    use ironclaw_event_log::{DurableEventLog, InMemoryDurableEventLog};
    use ironclaw_host_api::ids::{AgentId, TenantId, ThreadId};
    use ironclaw_product_contracts::outbound::{ProductOutboundPayload, ProductProjectionItem};
    use ironclaw_product_contracts::projection::ProjectionSubscriptionRequest;
    use ironclaw_turns::{ReplyTargetBindingRef, TurnActor};

    const SKILL_NAME: &str = "csv-column-sum";
    const FEEDBACK: &str = "picked this up summing a report column";

    /// The adapter is four lines of forwarding, which is exactly the shape that
    /// fails silently: a swapped `skill_name`/`feedback` pair still compiles
    /// (both are `&str`), and dropping the `Some(owner)` wrapper still compiles
    /// (the publisher takes `Option<&UserId>`) while quietly re-keying every
    /// learned-skill bubble onto the runtime operator's stream instead of the
    /// user's. Neither is reachable from `skill_learning.rs`'s `StubNotifier`
    /// tests, which stop at the port.
    ///
    /// So this drives the production trait object over the *real*
    /// `LiveProjectionPublisher` — no double anywhere — with the runtime actor
    /// deliberately different from the run owner, and reads the result back off
    /// the product event stream the WebUI actually drains.
    #[tokio::test]
    async fn live_notifier_forwards_each_argument_and_keys_the_bubble_to_the_run_owner() {
        let runtime_actor = UserId::new("skill-learned-runtime-actor").expect("valid user");
        let run_owner = UserId::new("skill-learned-run-owner").expect("valid user");
        let scope = TurnScope::new(
            TenantId::new("skill-learned-tenant").expect("valid tenant"),
            Some(AgentId::new("skill-learned-agent").expect("valid agent")),
            None,
            ThreadId::new("skill-learned-thread").expect("valid thread"),
        );

        let event_log: Arc<dyn DurableEventLog> = Arc::new(InMemoryDurableEventLog::new());
        let services = build_reborn_projection_services(
            event_log,
            ReplyTargetBindingRef::new("skill-learned-reply").expect("valid reply ref"),
        );
        let notifier: Arc<dyn SkillLearnedNotifier> = Arc::new(LiveSkillLearnedNotifier::new(
            services.live_projection_publisher(runtime_actor.clone()),
        ));

        notifier.notify(&run_owner, &scope, TurnRunId::new(), SKILL_NAME, FEEDBACK);

        let owner_items = skill_activations_for(&services, &run_owner, &scope).await;
        assert_eq!(
            owner_items,
            vec![(vec![SKILL_NAME.to_string()], vec![FEEDBACK.to_string()])],
            "the run owner must see exactly one learned-skill bubble carrying the name in \
             `skill_names` and the feedback in `feedback` — a swap or a dropped argument \
             shows up here"
        );

        let runtime_actor_items = skill_activations_for(&services, &runtime_actor, &scope).await;
        assert!(
            runtime_actor_items.is_empty(),
            "the bubble must be keyed to the run owner the notifier was handed, not to the \
             runtime actor the publisher was built with; found {runtime_actor_items:?}"
        );
    }

    async fn skill_activations_for(
        services: &ironclaw_assistant::projection::RebornProjectionServices,
        actor: &UserId,
        scope: &TurnScope,
    ) -> Vec<(Vec<String>, Vec<String>)> {
        services
            .product_event_stream()
            .drain(ProjectionSubscriptionRequest {
                actor: TurnActor::new(actor.clone()),
                scope: scope.clone(),
                after_cursor: None,
            })
            .await
            .expect("projection drain succeeds")
            .iter()
            .filter_map(|event| match event.payload() {
                ProductOutboundPayload::ProjectionUpdate { state } => Some(state.items.clone()),
                _ => None,
            })
            .flatten()
            .filter_map(|item| match item {
                ProductProjectionItem::SkillActivation {
                    skill_names,
                    feedback,
                    ..
                } => Some((skill_names, feedback)),
                _ => None,
            })
            .collect()
    }
}
