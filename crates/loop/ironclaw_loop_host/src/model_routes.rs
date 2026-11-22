use std::{collections::HashMap, error::Error, fmt};

use serde::{Deserialize, Serialize};

use ironclaw_loop_contracts::{
    LoopModelRouteSnapshot, ModelProfileId, validate_model_route_component_value,
};

const DEFAULT_CONFIG_VERSION: &str = "config:default";
const DEFAULT_AUTH_VERSION: &str = "auth:default";

/// Internal Reborn model purpose. Users choose provider/model routes; drivers
/// request purpose slots instead of raw provider identifiers.
///
/// `Cheap` is the lightweight-role slot (explorer/planner subagents,
/// compaction, other side work). It is opt-in by configuration: when no cheap
/// route is configured, [`StaticModelRouteResolver`] serves it with the
/// `Default` route (see [`ModelRouteSource::DefaultSlotFallback`]), so an
/// unset `LLM_CHEAP_MODEL` changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSlot {
    Default,
    Mission,
    Cheap,
}

impl ModelSlot {
    const ALL: [Self; 3] = [Self::Default, Self::Mission, Self::Cheap];

    pub fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Mission => "mission",
            Self::Cheap => "cheap",
        }
    }

    pub fn from_model_profile_id(model_profile_id: &ModelProfileId) -> Option<Self> {
        match model_profile_id.as_str() {
            "default" | "default_model" | "interactive_model" => Some(Self::Default),
            "mission_model" | "long_running_mission_model" => Some(Self::Mission),
            "cheap_model" | "fast_model" | "explorer_model" | "planner_model"
            | "compaction_model" => Some(Self::Cheap),
            _ => None,
        }
    }

    /// The slot whose configured route serves this slot when it has none of
    /// its own. Only `Cheap` degrades (to `Default`); `Default` and `Mission`
    /// stay strict so an unconfigured mission route still fails closed.
    fn fallback(self) -> Option<Self> {
        match self {
            Self::Cheap => Some(Self::Default),
            Self::Default | Self::Mission => None,
        }
    }
}

impl fmt::Display for ModelSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Concrete provider/model route selected by user/admin policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRoute {
    provider_id: String,
    model_id: String,
}

impl ModelRoute {
    pub fn new(
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<Self, ModelRouteError> {
        let provider_id = validate_provider_id(provider_id.into())?;
        let model_id = validate_model_id(model_id.into())?;
        Ok(Self {
            provider_id,
            model_id,
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn from_active_settings(
        settings: &ActiveModelRouteSettings,
    ) -> Result<Self, ModelRouteError> {
        Self::new(settings.provider_id(), settings.model_id())
    }
}

/// Minimal bridge from existing active IronClaw model settings into Reborn's
/// structured default model route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveModelRouteSettings {
    provider_id: String,
    model_id: String,
}

impl ActiveModelRouteSettings {
    pub fn new(
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<Self, ModelRouteError> {
        let provider_id = validate_provider_id(provider_id.into())?;
        let model_id = validate_model_id(model_id.into())?;
        Ok(Self {
            provider_id,
            model_id,
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn from_llm_config(config: &ironclaw_llm::LlmConfig) -> Result<Self, ModelRouteError> {
        Self::new(config.active_provider_id(), config.active_model_name())
    }

    /// Route settings for [`ModelSlot::Cheap`], from the configured cheap
    /// model (`LLM_CHEAP_MODEL`, or `NEARAI_CHEAP_MODEL` on NEAR AI) on the
    /// active provider. `Ok(None)` when no cheap model is configured — the
    /// caller then leaves the cheap slot unrouted and it falls back to the
    /// default route. A configured but invalid model id fails loud.
    pub fn cheap_from_llm_config(
        config: &ironclaw_llm::LlmConfig,
    ) -> Result<Option<Self>, ModelRouteError> {
        match config
            .cheap_model_name()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            Some(model) => Self::new(config.active_provider_id(), model).map(Some),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSelectionMode {
    ManagedOnly,
    UserSelectableAllowlist,
    DeveloperAnyConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoutePolicy {
    mode: ModelSelectionMode,
    approved_routes: Vec<ModelRoute>,
}

impl ModelRoutePolicy {
    pub fn new(mode: ModelSelectionMode) -> Self {
        Self {
            mode,
            approved_routes: Vec::new(),
        }
    }

    pub fn mode(&self) -> ModelSelectionMode {
        self.mode
    }

    pub fn with_approved_route(mut self, route: ModelRoute) -> Self {
        if !self.approved_routes.contains(&route) {
            self.approved_routes.push(route);
        }
        self
    }

    fn permits(&self, route: &ModelRoute) -> bool {
        match self.mode {
            ModelSelectionMode::DeveloperAnyConfigured => true,
            ModelSelectionMode::ManagedOnly | ModelSelectionMode::UserSelectableAllowlist => {
                self.approved_routes.contains(route)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRouteProviderKey {
    route: ModelRoute,
    config_version: String,
    auth_version: String,
}

impl ModelRouteProviderKey {
    pub fn new(
        route: ModelRoute,
        config_version: impl Into<String>,
        auth_version: impl Into<String>,
    ) -> Result<Self, ModelRouteError> {
        let config_version = validate_version_token(config_version.into())?;
        let auth_version = validate_version_token(auth_version.into())?;
        Ok(Self {
            route,
            config_version,
            auth_version,
        })
    }

    pub fn for_route(route: ModelRoute) -> Self {
        Self {
            route,
            config_version: DEFAULT_CONFIG_VERSION.to_string(),
            auth_version: DEFAULT_AUTH_VERSION.to_string(),
        }
    }

    pub fn route(&self) -> &ModelRoute {
        &self.route
    }

    pub fn config_version(&self) -> &str {
        &self.config_version
    }

    pub fn auth_version(&self) -> &str {
        &self.auth_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRouteSource {
    ConfiguredDefault,
    /// The requested slot had no route of its own and is served by its
    /// fallback slot's route (today: `Cheap` → `Default`).
    DefaultSlotFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModelRouteSnapshot {
    slot: ModelSlot,
    route: ModelRoute,
    provider_key: ModelRouteProviderKey,
    policy_mode: ModelSelectionMode,
    source: ModelRouteSource,
}

impl ResolvedModelRouteSnapshot {
    pub fn new(slot: ModelSlot, route: ModelRoute, policy_mode: ModelSelectionMode) -> Self {
        Self {
            slot,
            provider_key: ModelRouteProviderKey::for_route(route.clone()),
            route,
            policy_mode,
            source: ModelRouteSource::ConfiguredDefault,
        }
    }

    pub fn with_provider_key(
        slot: ModelSlot,
        provider_key: ModelRouteProviderKey,
        policy_mode: ModelSelectionMode,
    ) -> Self {
        Self {
            slot,
            route: provider_key.route().clone(),
            provider_key,
            policy_mode,
            source: ModelRouteSource::ConfiguredDefault,
        }
    }

    pub fn slot(&self) -> ModelSlot {
        self.slot
    }

    pub fn route(&self) -> &ModelRoute {
        &self.route
    }

    pub fn provider_key(&self) -> &ModelRouteProviderKey {
        &self.provider_key
    }

    pub fn policy_mode(&self) -> ModelSelectionMode {
        self.policy_mode
    }

    pub fn source(&self) -> ModelRouteSource {
        self.source
    }

    fn with_source(mut self, source: ModelRouteSource) -> Self {
        self.source = source;
        self
    }

    pub fn to_loop_model_route_snapshot(&self) -> Result<LoopModelRouteSnapshot, String> {
        LoopModelRouteSnapshot::try_new(
            self.provider_key.route().provider_id(),
            self.provider_key.route().model_id(),
            self.provider_key.config_version(),
            self.provider_key.auth_version(),
        )
    }
}

pub trait ModelRouteResolver: Send + Sync {
    fn resolve_model_route(
        &self,
        slot: ModelSlot,
    ) -> Result<ResolvedModelRouteSnapshot, ModelRouteError>;

    fn validate_model_route(
        &self,
        slot: ModelSlot,
        route: &ModelRoute,
    ) -> Result<ModelSelectionMode, ModelRouteError> {
        let snapshot = self.resolve_model_route(slot)?;
        if snapshot.route() == route {
            Ok(snapshot.policy_mode())
        } else {
            Err(ModelRouteError::new(ModelRouteErrorKind::RouteNotApproved))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticModelRouteResolver {
    policy: ModelRoutePolicy,
    routes: HashMap<ModelSlot, ModelRouteProviderKey>,
}

impl StaticModelRouteResolver {
    pub fn new(policy: ModelRoutePolicy) -> Self {
        Self {
            policy,
            routes: HashMap::new(),
        }
    }

    pub fn with_route(mut self, slot: ModelSlot, route: ModelRoute) -> Self {
        self.routes
            .insert(slot, ModelRouteProviderKey::for_route(route));
        self
    }

    pub fn with_provider_key(mut self, slot: ModelSlot, key: ModelRouteProviderKey) -> Self {
        self.routes.insert(slot, key);
        self
    }

    pub fn resolve(&self, slot: ModelSlot) -> Result<ResolvedModelRouteSnapshot, ModelRouteError> {
        self.resolve_model_route(slot)
    }
}

impl StaticModelRouteResolver {
    /// The slot whose configured route actually serves `slot`, plus whether
    /// that is a fallback. `None` when neither the slot nor its fallback has
    /// a route.
    fn serving_slot(&self, slot: ModelSlot) -> Option<(ModelSlot, ModelRouteSource)> {
        if self.routes.contains_key(&slot) {
            return Some((slot, ModelRouteSource::ConfiguredDefault));
        }
        let fallback = slot.fallback()?;
        self.routes
            .contains_key(&fallback)
            .then_some((fallback, ModelRouteSource::DefaultSlotFallback))
    }
}

impl ModelRouteResolver for StaticModelRouteResolver {
    fn resolve_model_route(
        &self,
        slot: ModelSlot,
    ) -> Result<ResolvedModelRouteSnapshot, ModelRouteError> {
        let (serving_slot, source) = self
            .serving_slot(slot)
            .ok_or_else(|| ModelRouteError::new(ModelRouteErrorKind::RouteUnavailable))?;
        let provider_key = self
            .routes
            .get(&serving_slot)
            .ok_or_else(|| ModelRouteError::new(ModelRouteErrorKind::RouteUnavailable))?;
        if !self.policy.permits(&provider_key.route) {
            return Err(ModelRouteError::new(ModelRouteErrorKind::RouteNotApproved));
        }
        // The snapshot records the *requested* slot (telemetry and replay
        // validate against it) and says, via `source`, when it was served by
        // the fallback slot's route.
        Ok(ResolvedModelRouteSnapshot::with_provider_key(
            slot,
            provider_key.clone(),
            self.policy.mode(),
        )
        .with_source(source))
    }

    fn validate_model_route(
        &self,
        slot: ModelSlot,
        route: &ModelRoute,
    ) -> Result<ModelSelectionMode, ModelRouteError> {
        let Some((serving_slot, _)) = self.serving_slot(slot) else {
            return Err(ModelRouteError::new(ModelRouteErrorKind::RouteUnavailable));
        };
        // A fallback slot replays exactly as the slot serving it: it inherits
        // that slot's policy and exclusivity and can never widen them.
        let slot = serving_slot;
        if !self.policy.permits(route) {
            return Err(ModelRouteError::new(ModelRouteErrorKind::RouteNotApproved));
        }
        // Persisted snapshots may outlive current settings, but a route that
        // is currently configured exclusively for a different slot must not be
        // replayed as this slot's route.
        let current_slot_uses_route = self
            .routes
            .get(&slot)
            .is_some_and(|configured_route| configured_route.route() == route);
        let other_slot_uses_route =
            self.routes
                .iter()
                .any(|(configured_slot, configured_route)| {
                    *configured_slot != slot && configured_route.route() == route
                });
        if other_slot_uses_route && !current_slot_uses_route {
            return Err(ModelRouteError::new(ModelRouteErrorKind::RouteNotApproved));
        }
        Ok(self.policy.mode())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRouteErrorKind {
    InvalidRoute,
    RouteUnavailable,
    RouteNotApproved,
}

impl ModelRouteErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRoute => "invalid_route",
            Self::RouteUnavailable => "route_unavailable",
            Self::RouteNotApproved => "route_not_approved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRouteError {
    kind: ModelRouteErrorKind,
}

impl ModelRouteError {
    fn new(kind: ModelRouteErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> ModelRouteErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelRouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind.as_str())
    }
}

impl Error for ModelRouteError {}

fn validate_provider_id(value: String) -> Result<String, ModelRouteError> {
    validate_route_component("provider_id", value, 128, |character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
    })
}

fn validate_model_id(value: String) -> Result<String, ModelRouteError> {
    validate_route_component("model_id", value, 256, |character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':' | '/')
    })
}

fn validate_version_token(value: String) -> Result<String, ModelRouteError> {
    validate_route_component("route version token", value, 128, |character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
    })
}

fn validate_route_component(
    label: &'static str,
    value: String,
    max_bytes: usize,
    allowed: impl Fn(char) -> bool,
) -> Result<String, ModelRouteError> {
    let trimmed = value.trim().to_string();
    validate_model_route_component_value(label, &trimmed, max_bytes, allowed)
        .map_err(|_| ModelRouteError::new(ModelRouteErrorKind::InvalidRoute))?;
    Ok(trimmed)
}
