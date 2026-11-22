#![deny(clippy::let_underscore_must_use)]
use ironclaw_loop_host::{
    ActiveModelRouteSettings, ModelRoute, ModelRouteErrorKind, ModelRoutePolicy,
    ModelRouteProviderKey, ModelRouteResolver, ModelRouteSource, ModelSelectionMode, ModelSlot,
    StaticModelRouteResolver,
};

use ironclaw_llm::{LlmConfig, NearAiConfig, SessionConfig};

#[test]
fn active_llm_settings_resolve_to_default_model_route() {
    let settings =
        ActiveModelRouteSettings::new("openrouter", "anthropic/claude-sonnet-4").unwrap();

    let route = ModelRoute::from_active_settings(&settings).unwrap();

    assert_eq!(route.provider_id(), "openrouter");
    assert_eq!(route.model_id(), "anthropic/claude-sonnet-4");
}

#[test]
fn llm_config_resolves_to_default_model_route_settings() {
    let config = nearai_config("qwen3-coder");

    let settings = ActiveModelRouteSettings::from_llm_config(&config).unwrap();

    assert_eq!(settings.provider_id(), "nearai");
    assert_eq!(settings.model_id(), "qwen3-coder");
}

#[test]
fn model_slots_are_exposed_in_cli_display_order() {
    assert_eq!(
        ModelSlot::all(),
        &[ModelSlot::Default, ModelSlot::Mission, ModelSlot::Cheap]
    );
    assert_eq!(ModelSlot::Cheap.as_str(), "cheap");
}

#[test]
fn lightweight_role_profiles_map_to_the_cheap_slot() {
    for profile in [
        "cheap_model",
        "fast_model",
        "explorer_model",
        "planner_model",
        "compaction_model",
    ] {
        let profile_id = ironclaw_loop_contracts::ModelProfileId::new(profile).unwrap();
        assert_eq!(
            ModelSlot::from_model_profile_id(&profile_id),
            Some(ModelSlot::Cheap),
            "{profile} must route through the cheap slot"
        );
    }
    let interactive_model =
        ironclaw_loop_contracts::ModelProfileId::new("interactive_model").unwrap();
    assert_eq!(
        ModelSlot::from_model_profile_id(&interactive_model),
        Some(ModelSlot::Default),
        "interactive turns never silently downgrade to the cheap slot"
    );
}

#[test]
fn cheap_slot_resolves_the_configured_cheap_route() {
    let default_route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let cheap_route = ModelRoute::new("openrouter", "anthropic/claude-haiku-4").unwrap();
    let resolver = StaticModelRouteResolver::new(
        ModelRoutePolicy::new(ModelSelectionMode::ManagedOnly)
            .with_approved_route(default_route.clone())
            .with_approved_route(cheap_route.clone()),
    )
    .with_route(ModelSlot::Default, default_route.clone())
    .with_route(ModelSlot::Cheap, cheap_route.clone());

    let snapshot = resolver.resolve(ModelSlot::Cheap).unwrap();
    assert_eq!(snapshot.slot(), ModelSlot::Cheap);
    assert_eq!(snapshot.route(), &cheap_route);
    assert_eq!(snapshot.source(), ModelRouteSource::ConfiguredDefault);
    assert_eq!(
        resolver
            .validate_model_route(ModelSlot::Cheap, &cheap_route)
            .unwrap(),
        ModelSelectionMode::ManagedOnly
    );
    // The cheap route is exclusive to its slot: an interactive snapshot can
    // never be replayed onto it, and the cheap slot never takes the default.
    assert_eq!(
        resolver
            .validate_model_route(ModelSlot::Default, &cheap_route)
            .unwrap_err()
            .kind(),
        ModelRouteErrorKind::RouteNotApproved
    );
    assert_eq!(
        resolver
            .validate_model_route(ModelSlot::Cheap, &default_route)
            .unwrap_err()
            .kind(),
        ModelRouteErrorKind::RouteNotApproved
    );
}

#[test]
fn cheap_slot_falls_back_to_the_default_route_when_no_cheap_model_is_configured() {
    let default_route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let unrelated_route = ModelRoute::new("openrouter", "anthropic/claude-haiku-4").unwrap();
    let resolver = StaticModelRouteResolver::new(
        ModelRoutePolicy::new(ModelSelectionMode::ManagedOnly)
            .with_approved_route(default_route.clone()),
    )
    .with_route(ModelSlot::Default, default_route.clone());

    let snapshot = resolver.resolve(ModelSlot::Cheap).unwrap();
    assert_eq!(snapshot.slot(), ModelSlot::Cheap);
    assert_eq!(snapshot.route(), &default_route);
    assert_eq!(snapshot.source(), ModelRouteSource::DefaultSlotFallback);
    assert_eq!(
        resolver
            .validate_model_route(ModelSlot::Cheap, &default_route)
            .unwrap(),
        ModelSelectionMode::ManagedOnly,
        "a persisted fallback snapshot must replay against the default route"
    );
    assert_eq!(
        resolver
            .validate_model_route(ModelSlot::Cheap, &unrelated_route)
            .unwrap_err()
            .kind(),
        ModelRouteErrorKind::RouteNotApproved,
        "the fallback inherits the default slot's policy, never widens it"
    );
    // Mission keeps its strict behavior: no silent fallback.
    assert_eq!(
        resolver.resolve(ModelSlot::Mission).unwrap_err().kind(),
        ModelRouteErrorKind::RouteUnavailable
    );
}

#[test]
fn cheap_slot_fallback_still_honors_the_route_policy() {
    let default_route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let resolver =
        StaticModelRouteResolver::new(ModelRoutePolicy::new(ModelSelectionMode::ManagedOnly))
            .with_route(ModelSlot::Default, default_route);

    assert_eq!(
        resolver.resolve(ModelSlot::Cheap).unwrap_err().kind(),
        ModelRouteErrorKind::RouteNotApproved
    );
}

#[test]
fn llm_config_cheap_model_resolves_to_cheap_route_settings_only_when_configured() {
    let unset = nearai_config("qwen3-coder");
    assert_eq!(
        ActiveModelRouteSettings::cheap_from_llm_config(&unset).unwrap(),
        None
    );

    let mut configured = nearai_config("qwen3-coder");
    configured.cheap_model = Some("qwen3-mini".to_string());
    let settings = ActiveModelRouteSettings::cheap_from_llm_config(&configured)
        .unwrap()
        .expect("LLM_CHEAP_MODEL yields cheap route settings");
    assert_eq!(settings.provider_id(), "nearai");
    assert_eq!(settings.model_id(), "qwen3-mini");

    let mut blank = nearai_config("qwen3-coder");
    blank.cheap_model = Some("   ".to_string());
    assert_eq!(
        ActiveModelRouteSettings::cheap_from_llm_config(&blank).unwrap(),
        None,
        "a blank cheap model is treated as unset"
    );
}

#[test]
fn model_slots_cover_builtin_interactive_and_mission_profiles() {
    let interactive_model =
        ironclaw_loop_contracts::ModelProfileId::new("interactive_model").unwrap();
    let mission_model = ironclaw_loop_contracts::ModelProfileId::new("mission_model").unwrap();

    assert_eq!(
        ModelSlot::from_model_profile_id(&interactive_model),
        Some(ModelSlot::Default),
    );
    assert_eq!(
        ModelSlot::from_model_profile_id(&mission_model),
        Some(ModelSlot::Mission),
    );
    assert_eq!(ModelSlot::Mission.as_str(), "mission");
}

#[test]
fn route_validation_allows_bearer_prefixed_provider_ids() {
    let route = ModelRoute::new("bearer-mini", "qwen3-coder").unwrap();

    assert_eq!(route.provider_id(), "bearer-mini");
}

#[test]
fn provider_key_includes_route_config_and_auth_versions() {
    let route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();

    let key = ModelRouteProviderKey::new(route.clone(), "config:v2", "auth:v7").unwrap();

    assert_eq!(key.route(), &route);
    assert_eq!(key.config_version(), "config:v2");
    assert_eq!(key.auth_version(), "auth:v7");
}

#[test]
fn static_resolver_preserves_provider_key_versions() {
    let route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let provider_key = ModelRouteProviderKey::new(route.clone(), "config:v3", "auth:v9").unwrap();
    let resolver = StaticModelRouteResolver::new(ModelRoutePolicy::new(
        ModelSelectionMode::DeveloperAnyConfigured,
    ))
    .with_provider_key(ModelSlot::Default, provider_key.clone());

    let snapshot = resolver.resolve(ModelSlot::Default).unwrap();

    assert_eq!(snapshot.provider_key(), &provider_key);
    assert_eq!(snapshot.route(), &route);
}

#[test]
fn developer_policy_allows_any_configured_default_route() {
    let route = ModelRoute::new("ollama", "qwen2.5-coder:7b").unwrap();
    let resolver = StaticModelRouteResolver::new(ModelRoutePolicy::new(
        ModelSelectionMode::DeveloperAnyConfigured,
    ))
    .with_route(ModelSlot::Default, route.clone());

    let snapshot = resolver.resolve(ModelSlot::Default).unwrap();

    assert_eq!(snapshot.slot(), ModelSlot::Default);
    assert_eq!(snapshot.route(), &route);
    assert_eq!(
        snapshot.policy_mode(),
        ModelSelectionMode::DeveloperAnyConfigured
    );
}

#[test]
fn user_selectable_policy_allows_explicitly_approved_default_route() {
    let route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let resolver = StaticModelRouteResolver::new(
        ModelRoutePolicy::new(ModelSelectionMode::UserSelectableAllowlist)
            .with_approved_route(route.clone()),
    )
    .with_route(ModelSlot::Default, route.clone());

    let snapshot = resolver.resolve(ModelSlot::Default).unwrap();

    assert_eq!(snapshot.route(), &route);
    assert_eq!(
        snapshot.policy_mode(),
        ModelSelectionMode::UserSelectableAllowlist
    );
}

#[test]
fn static_resolver_rejects_route_configured_for_different_slot_only() {
    let default_route = ModelRoute::new("nearai", "qwen3-coder").unwrap();
    let mission_route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let resolver = StaticModelRouteResolver::new(
        ModelRoutePolicy::new(ModelSelectionMode::UserSelectableAllowlist)
            .with_approved_route(default_route.clone())
            .with_approved_route(mission_route.clone()),
    )
    .with_route(ModelSlot::Default, default_route)
    .with_route(ModelSlot::Mission, mission_route.clone());

    let error = resolver
        .validate_model_route(ModelSlot::Default, &mission_route)
        .unwrap_err();

    assert_eq!(error.kind().as_str(), "route_not_approved");
}

#[test]
fn static_resolver_allows_shared_route_configured_for_requested_slot() {
    let shared_route = ModelRoute::new("nearai", "qwen3-coder").unwrap();
    let resolver = StaticModelRouteResolver::new(
        ModelRoutePolicy::new(ModelSelectionMode::UserSelectableAllowlist)
            .with_approved_route(shared_route.clone()),
    )
    .with_route(ModelSlot::Default, shared_route.clone())
    .with_route(ModelSlot::Mission, shared_route.clone());

    let mode = resolver
        .validate_model_route(ModelSlot::Default, &shared_route)
        .unwrap();

    assert_eq!(mode, ModelSelectionMode::UserSelectableAllowlist);
}

#[test]
fn managed_policy_rejects_unapproved_default_route() {
    let route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let resolver =
        StaticModelRouteResolver::new(ModelRoutePolicy::new(ModelSelectionMode::ManagedOnly))
            .with_route(ModelSlot::Default, route);

    let error = resolver.resolve(ModelSlot::Default).unwrap_err();

    assert_eq!(error.kind().as_str(), "route_not_approved");
}

#[test]
fn managed_policy_allows_explicitly_approved_default_route() {
    let route = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let resolver = StaticModelRouteResolver::new(
        ModelRoutePolicy::new(ModelSelectionMode::ManagedOnly).with_approved_route(route.clone()),
    )
    .with_route(ModelSlot::Default, route.clone());

    let snapshot = resolver.resolve(ModelSlot::Default).unwrap();

    assert_eq!(snapshot.route(), &route);
}

#[test]
fn resolved_route_snapshot_remains_stable_after_settings_change() {
    let initial = ModelRoute::new("nearai", "qwen3-coder").unwrap();
    let replacement = ModelRoute::new("openrouter", "anthropic/claude-sonnet-4").unwrap();
    let policy = ModelRoutePolicy::new(ModelSelectionMode::DeveloperAnyConfigured);
    let initial_resolver = StaticModelRouteResolver::new(policy.clone())
        .with_route(ModelSlot::Default, initial.clone());
    let replacement_resolver =
        StaticModelRouteResolver::new(policy).with_route(ModelSlot::Default, replacement);

    let snapshot = initial_resolver.resolve(ModelSlot::Default).unwrap();
    let later_snapshot = replacement_resolver.resolve(ModelSlot::Default).unwrap();

    assert_eq!(snapshot.route(), &initial);
    assert_ne!(snapshot.route(), later_snapshot.route());
}

#[test]
fn route_validation_rejects_secret_like_provider_ids() {
    let error = ModelRoute::new("sk-secret-provider", "gpt-4").unwrap_err();

    assert_eq!(error.kind().as_str(), "invalid_route");
}

#[test]
fn route_validation_rejects_secret_like_model_ids() {
    let error = ModelRoute::new("openrouter", "anthropic/secret-model").unwrap_err();

    assert_eq!(error.kind().as_str(), "invalid_route");
}

fn nearai_config(model: &str) -> LlmConfig {
    LlmConfig {
        backend: "nearai".to_string(),
        session: SessionConfig::default(),
        nearai: NearAiConfig {
            model: model.to_string(),
            cheap_model: None,
            base_url: "https://private.near.ai".to_string(),
            api_key: None,
            fallback_model: None,
            max_retries: 3,
            circuit_breaker_threshold: None,
            circuit_breaker_recovery_secs: 30,
            response_cache_enabled: false,
            response_cache_ttl_secs: 3600,
            response_cache_max_entries: 1000,
            failover_cooldown_secs: 300,
            failover_cooldown_threshold: 3,
            smart_routing_cascade: true,
            unsupported_params: Vec::new(),
        },
        provider: None,
        bedrock: None,
        gemini_oauth: None,
        openai_codex: None,
        request_timeout_secs: 120,
        cheap_model: None,
        smart_routing_cascade: true,
        max_retries: 3,
        circuit_breaker_threshold: None,
        circuit_breaker_recovery_secs: 30,
        response_cache_enabled: false,
        response_cache_ttl_secs: 3600,
        response_cache_max_entries: 1000,
    }
}
