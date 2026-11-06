//! The read-only hook inventory for the Settings page, derived from the same
//! inputs the per-run dispatcher factory replays: the activation flags, the
//! (empty) first-party catalog, every projected extension `[[hooks]]` entry,
//! and the after-turn memory upkeep hook when it is wired.
//!
//! Extension hooks always install at the `Installed` trust tier (the
//! registrar is the only install path), so that is the class reported for
//! them whatever the package's own posture.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ironclaw_hooks::evaluator::PredicateEvaluator;
use ironclaw_hooks::manifest::HookManifestEntry;
use ironclaw_hooks::predicate_state::InMemoryPredicateStateBackend;
use ironclaw_hooks::registrar::HookRegistrar;
use ironclaw_product_contracts::hooks_catalog::{
    HookCatalog, HookCatalogEntry, HookSource, HookTrustClassView,
};

use super::HooksActivationConfig;
use super::factory::project_extension_hook_sets;
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
    let Some(registry) = registry else {
        return finish(config, hooks);
    };
    // Validate exactly as the dispatcher factory does (same caps, containment,
    // projection, and registrar install against a scratch builder), recording
    // quarantines instead of only logging them. A hook is "on" only when its
    // extension's whole set survived AND the flags admit its source.
    let evaluator = Arc::new(PredicateEvaluator::with_state_backend(Arc::new(
        InMemoryPredicateStateBackend::new(),
    )));
    let registrar = HookRegistrar::new(evaluator);
    let mut install_quarantines: BTreeMap<String, String> = BTreeMap::new();
    let survivors: BTreeSet<String> = match project_extension_hook_sets(
        registry.projections(),
        &registrar,
        None,
        &mut |extension_id, reason, _| {
            install_quarantines.insert(extension_id.to_string(), reason.to_string());
        },
    ) {
        Ok(sets) => sets
            .into_iter()
            .map(|set| set.extension_id.as_str().to_string())
            .collect(),
        // A trusted (host-bundled) package with an invalid hook fails the
        // whole runtime build; the catalog reports every hook off instead.
        Err(error) => {
            tracing::debug!(%error, "hook catalog: trusted hook set failed validation");
            BTreeSet::new()
        }
    };
    for projection in registry.projections() {
        let trusted = projection.source.allows_first_party();
        let enabled_by_flags = if trusted {
            config.is_enabled()
        } else {
            config.is_third_party_enabled()
        };
        let extension = projection.extension_id.as_str();
        let quarantine = install_quarantines.get(extension);
        let installed = survivors.contains(extension);
        for hook in &projection.hooks {
            let id = format!("{extension}/{}", hook.local_id);
            let entry = toml::from_str::<HookManifestEntry>(&hook.raw_toml).ok();
            let point = entry
                .as_ref()
                .and_then(|entry| serde_json::to_value(entry.kind).ok())
                .and_then(|value| value.as_str().map(str::to_string));
            let note = if let Some(reason) = quarantine {
                Some(format!("Quarantined with its extension: {}", short(reason)))
            } else if !installed {
                Some("Quarantined: the extension's hooks failed validation.".to_string())
            } else if !enabled_by_flags {
                Some(if trusted {
                    "Off: HOOKS_ENABLED is not set.".to_string()
                } else {
                    "Off: third-party hooks need HOOKS_ENABLED and HOOKS_THIRD_PARTY_ENABLED."
                        .to_string()
                })
            } else {
                None
            };
            hooks.push(HookCatalogEntry {
                name: entry
                    .as_ref()
                    .and_then(|entry| entry.description.clone())
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| hook.local_id.clone()),
                id,
                source: HookSource::Extension,
                trust_class: HookTrustClassView::Installed,
                owner: Some(extension.to_string()),
                points: point.into_iter().collect(),
                enabled: installed && enabled_by_flags,
                note,
            });
        }
    }
    // Packages dropped before projection (malformed, oversized, over the
    // tenant budget, outside the root, duplicate id) have no readable hooks;
    // each is listed once so the drop is visible.
    for (extension, reason) in registry.admission_quarantines() {
        hooks.push(HookCatalogEntry {
            id: extension.clone(),
            name: format!("{extension} (all hooks)"),
            source: HookSource::Extension,
            trust_class: HookTrustClassView::Installed,
            owner: Some(extension.clone()),
            points: Vec::new(),
            enabled: false,
            note: Some(format!("Quarantined: {}", short(reason))),
        });
    }
    finish(config, hooks)
}

/// Bounded, single-line reason text for the page.
fn short(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or(reason);
    let mut text: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        text.push('…');
    }
    text
}

fn finish(config: HooksActivationConfig, hooks: Vec<HookCatalogEntry>) -> HookCatalog {
    HookCatalog {
        framework_enabled: config.is_enabled(),
        third_party_enabled: config.is_third_party_enabled(),
        toggle_supported: false,
        hooks,
    }
}
