//! The read-only hook inventory for the Settings page, derived from the same
//! inputs the per-run dispatcher factory replays: the activation flags, the
//! (empty) first-party catalog, every projected extension `[[hooks]]` entry,
//! and the after-turn memory upkeep hook when it is wired.
//!
//! Extension hooks always install at the `Installed` trust tier (the
//! registrar is the only install path), so that is the class reported for
//! them whatever the package's own posture.

use ironclaw_hooks::manifest::HookManifestEntry;
use ironclaw_product_contracts::hooks_catalog::{
    HookCatalog, HookCatalogEntry, HookSource, HookTrustClassView,
};

use super::HooksActivationConfig;
use super::projection::HookProjectionRegistry;

/// Id of the memory upkeep hook in the catalog.
pub(crate) const MEMORY_UPKEEP_HOOK_ID: &str = "memory.scheduled_ops.after_turn";

pub(crate) fn hook_catalog(
    config: HooksActivationConfig,
    registry: Option<&HookProjectionRegistry>,
    memory_upkeep_wired: bool,
) -> HookCatalog {
    let mut hooks = vec![HookCatalogEntry {
        id: MEMORY_UPKEEP_HOOK_ID.to_string(),
        name: "Memory upkeep".to_string(),
        source: HookSource::Builtin,
        trust_class: HookTrustClassView::Builtin,
        owner: Some("ironclaw_assistant".to_string()),
        points: vec!["after_turn".to_string()],
        enabled: memory_upkeep_wired,
        note: Some(if memory_upkeep_wired {
            "Runs the memory provider's curation pass every few completed turns.".to_string()
        } else {
            "Off: the bound memory provider declares no upkeep pass, or \
             [memory].curation_interval_turns is not set."
                .to_string()
        }),
    }];
    for projection in registry.into_iter().flat_map(HookProjectionRegistry::projections) {
        let trusted = projection.source.allows_first_party();
        let enabled_by_flags = if trusted {
            config.is_enabled()
        } else {
            config.is_third_party_enabled()
        };
        let extension = projection.extension_id.as_str();
        for hook in &projection.hooks {
            let id = format!("{extension}/{}", hook.local_id);
            let entry = match toml::from_str::<HookManifestEntry>(&hook.raw_toml) {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(%error, hook = %id, "hook catalog: invalid manifest entry");
                    hooks.push(HookCatalogEntry {
                        name: hook.local_id.clone(),
                        id,
                        source: HookSource::Extension,
                        trust_class: HookTrustClassView::Installed,
                        owner: Some(extension.to_string()),
                        points: Vec::new(),
                        enabled: false,
                        note: Some("Quarantined: the manifest entry is invalid.".to_string()),
                    });
                    continue;
                }
            };
            let point = serde_json::to_value(entry.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            hooks.push(HookCatalogEntry {
                name: entry
                    .description
                    .clone()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| hook.local_id.clone()),
                id,
                source: HookSource::Extension,
                trust_class: HookTrustClassView::Installed,
                owner: Some(extension.to_string()),
                points: vec![point],
                enabled: enabled_by_flags,
                note: (!enabled_by_flags).then(|| {
                    if trusted {
                        "Off: HOOKS_ENABLED is not set.".to_string()
                    } else {
                        "Off: third-party hooks need HOOKS_ENABLED and \
                         HOOKS_THIRD_PARTY_ENABLED."
                            .to_string()
                    }
                }),
            });
        }
    }
    HookCatalog {
        framework_enabled: config.is_enabled(),
        third_party_enabled: config.is_third_party_enabled(),
        toggle_supported: false,
        hooks,
    }
}
