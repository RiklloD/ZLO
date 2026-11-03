//! Read-only view of the installed loop hooks for the Settings page.
//!
//! Lists every hook the deployment would install into a run: its id, where
//! it comes from (which fixes its trust class), the hook points it attaches
//! to, and whether it is currently live. There is no toggle — the framework
//! switch is boot configuration (`HOOKS_ENABLED` / `HOOKS_THIRD_PARTY_ENABLED`)
//! and per-hook enablement has no durable backend yet.
//!
//! Composition implements [`HookCatalogSource`] from the same inputs the
//! per-run dispatcher factory replays; `ironclaw_assistant::memory_page`
//! answers the view.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::descriptors::ProductView;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookCatalogRequest {}

/// `GET /api/webchat/v2/hooks`.
pub const HOOK_CATALOG_VIEW: ProductView<HookCatalogRequest, HookCatalog> =
    ProductView::unpaginated("hooks.catalog");

/// Where a hook comes from. The source fixes the trust class; a hook cannot
/// declare its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSource {
    /// Shipped and installed by the host itself.
    Builtin,
    /// Declared in an installed extension's manifest (`[[hooks]]`).
    Extension,
    /// Authored by a skill.
    Skill,
    /// Authored by the user (or by the agent on the user's behalf).
    User,
}

/// The framework's four trust classes (`ironclaw_hooks::HookTrustClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrustClassView {
    Builtin,
    Trusted,
    Installed,
    SelfAuthored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookCatalogEntry {
    /// Stable hook id, e.g. `memory.curation.after_turn`.
    pub id: String,
    /// Short human label.
    pub name: String,
    pub source: HookSource,
    pub trust_class: HookTrustClassView,
    /// The owning extension (extension hooks) or package, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Hook points, e.g. `before_capability`, `after_turn`.
    pub points: Vec<String>,
    /// Installed into new runs right now.
    pub enabled: bool,
    /// Why it is off, or what it does, when there is something to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookCatalog {
    /// `HOOKS_ENABLED`: the capability/loop hook framework is composed.
    pub framework_enabled: bool,
    /// `HOOKS_THIRD_PARTY_ENABLED` (only meaningful with the framework on).
    pub third_party_enabled: bool,
    /// Per-hook toggling is not supported; the page renders read-only.
    pub toggle_supported: bool,
    pub hooks: Vec<HookCatalogEntry>,
}

/// The deployment's hook inventory. Implemented by composition.
#[async_trait]
pub trait HookCatalogSource: Send + Sync {
    async fn catalog(&self) -> HookCatalog;
}
