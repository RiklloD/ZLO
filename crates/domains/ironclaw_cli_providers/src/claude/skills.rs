//! What the Claude CLI lists of its skills each turn, kept small without the
//! person managing it.
//!
//! Every skill the CLI can reach costs its description in every turn's
//! context. ZLO keeps the hot ones fully listed and the rest name-only
//! (`skillOverrides` in `--settings`): a name-only skill stays callable
//! through the `Skill` tool, it just loses its description.
//!
//! - **Catalog:** the `skills` of the turn's `system/init` frame, recorded as
//!   this runtime's catalog in the shared skill usage store
//!   (`ironclaw_skills::usage`).
//! - **Ledger:** every `Skill` call counts one use in that store, merged
//!   read-only with the account's `.claude.json` `skillUsage` (never
//!   written). The profile plugin's skills (`<plugin>:<name>`) are kept
//!   under their bare `<name>`, the same name the harness and the other
//!   CLIs use for them.
//! - **Policy:** `ironclaw_skills::usage::listing`. On top of it the
//!   profile's plugin skills stay on unless pinned otherwise, a skill the
//!   turn's text asks for (`$name`, `/name`) is on whatever its pin, and the
//!   artifact skills are off while the artifact tools are.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ironclaw_skills::usage::{
    SkillLedger, SkillListing as Listing, SkillUsageName, SkillUsageObservation, SkillUsageSource,
    SkillUse, requested_skills,
};
use serde::{Deserialize, Serialize};

use crate::skill_listing::{CliSkillUsage, decide, usage_name};

/// The CLI's artifact and design-sync tools, off unless an account asks
/// for them: about 12k tokens of tool schema every turn.
pub(crate) const ARTIFACT_TOOLS: [&str; 5] = [
    "Artifact",
    "ArtifactComments",
    "ArtifactData",
    "ArtifactCheck",
    "DesignSync",
];

/// The CLI's bundled skills that only serve [`ARTIFACT_TOOLS`].
pub(crate) const ARTIFACT_SKILLS: [&str; 3] = [
    "artifact-design",
    "artifact-diagramming",
    "artifact-capabilities",
];

/// How an account's skills are listed.
#[derive(Debug, Clone, Default)]
pub struct SkillListing {
    /// The shared skill usage store; the default manages nothing and passes
    /// only the artifact overrides.
    pub usage: CliSkillUsage,
    /// The account's state file from before the shared store
    /// (`~/.ironclaw/claude-skills/<id>.json`): its ledger is imported once,
    /// then the file is renamed `<id>.json.imported` (kept, not deleted).
    pub legacy_state_file: Option<PathBuf>,
}

/// `skillOverrides` values the CLI takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Listed {
    On,
    NameOnly,
    Off,
}

impl From<Listing> for Listed {
    fn from(listing: Listing) -> Self {
        match listing {
            Listing::Full => Self::On,
            Listing::NameOnly => Self::NameOnly,
            Listing::Off => Self::Off,
        }
    }
}

/// How the CLI's own skill names map onto the ledger's: the profile
/// plugin's `<plugin>:<name>` is the bare `<name>`; every other name is
/// itself.
#[derive(Debug, Clone, Default)]
pub(crate) struct SkillNames {
    plugin: Option<String>,
    /// The profile's skills (bare names), which the plugin serves.
    profile: BTreeSet<String>,
}

impl SkillNames {
    pub(crate) fn new(plugin: Option<String>, profile: impl IntoIterator<Item = String>) -> Self {
        Self {
            plugin,
            profile: profile.into_iter().collect(),
        }
    }

    /// The ledger's name for one of the CLI's.
    fn usage(&self, native: &str) -> Option<SkillUsageName> {
        let bare = self
            .plugin
            .as_deref()
            .and_then(|p| native.strip_prefix(p)?.strip_prefix(':'))
            .unwrap_or(native);
        usage_name(bare)
    }

    /// The CLI's name for one of the ledger's.
    fn native(&self, name: &SkillUsageName) -> String {
        match &self.plugin {
            Some(plugin) if self.profile.contains(name.as_str()) => format!("{plugin}:{name}"),
            _ => name.to_string(),
        }
    }

    fn is_plugin(&self, name: &SkillUsageName) -> bool {
        self.plugin.is_some() && self.profile.contains(name.as_str())
    }
}

/// The `--settings` JSON carrying `overrides`; `None` when there are none.
pub(crate) fn settings_json(overrides: &BTreeMap<String, Listed>) -> Option<String> {
    (!overrides.is_empty()).then(|| serde_json::json!({ "skillOverrides": overrides }).to_string())
}

/// `skillUsage` of an account's `.claude.json`, in ledger names; empty when
/// absent or unreadable (the ledger is advisory).
pub(crate) async fn account_usage(login_file: &Path, names: &SkillNames) -> SkillLedger {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        #[serde(default)]
        usage_count: u64,
        #[serde(default)]
        last_used_at: u64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Account {
        #[serde(default)]
        skill_usage: BTreeMap<String, Entry>,
    }
    let text = match tokio::fs::read_to_string(login_file).await {
        Ok(text) => text,
        Err(error) => {
            tracing::debug!(%error, "claude skills: account usage not read");
            return SkillLedger::new();
        }
    };
    match serde_json::from_str::<Account>(&text) {
        Ok(account) => account
            .skill_usage
            .into_iter()
            .filter_map(|(name, e)| {
                Some((
                    names.usage(&name)?,
                    SkillUse {
                        count: e.usage_count,
                        last_used_ms: e.last_used_at,
                    },
                ))
            })
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "claude skills: account usage unparsed");
            SkillLedger::new()
        }
    }
}

/// The profile plugin's name (`.claude-plugin/plugin.json`).
pub(crate) async fn plugin_name(plugin_dir: &Path) -> Option<String> {
    let text = tokio::fs::read_to_string(plugin_dir.join(".claude-plugin/plugin.json"))
        .await
        .ok()?; // silent-ok: without a manifest the CLI loads no plugin skills either
    let value: serde_json::Value = serde_json::from_str(&text).ok()?; // silent-ok: same
    value.get("name")?.as_str().map(str::to_string)
}

/// Import a pre-store state file's ledger once, then set the file aside
/// (an unparsable file is set aside with nothing to import).
async fn import_legacy(listing: &SkillListing, source: &SkillUsageSource, names: &SkillNames) {
    #[derive(Deserialize)]
    struct Legacy {
        #[serde(default)]
        ledger: BTreeMap<String, SkillUse>,
    }
    let Some(path) = &listing.legacy_state_file else {
        return;
    };
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::debug!(%error, "claude skills: legacy state not read");
            return;
        }
    };
    let ledger: SkillLedger = match serde_json::from_str::<Legacy>(&text) {
        Ok(legacy) => legacy
            .ledger
            .into_iter()
            .filter_map(|(name, used)| Some((names.usage(&name)?, used)))
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "claude skills: legacy state unparsed; set aside");
            SkillLedger::new()
        }
    };
    // Set aside only once the store holds the ledger: a failed import is
    // retried on the next turn.
    if let Err(error) = listing.usage.import_ledger(source, ledger).await {
        tracing::debug!(%error, "claude skills: legacy ledger not imported; kept for a retry");
        return;
    }
    let aside = path.with_extension("json.imported");
    if let Err(error) = tokio::fs::rename(path, &aside).await {
        tracing::debug!(%error, "claude skills: legacy state not set aside");
    }
}

/// What one turn's frames say about skills: the catalog the CLI offered and
/// the skills it called.
#[derive(Debug, Default)]
pub(crate) struct SkillWatch {
    catalog: Option<Vec<String>>,
    used: Vec<String>,
}

impl SkillWatch {
    pub(crate) fn push_line(&mut self, line: &str) {
        // Cheap filter: most frames are stream deltas.
        if !(line.contains("\"init\"") || line.contains("\"Skill\"")) {
            return;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return; // silent-ok: the turn fold reports unparsed frames
        };
        match frame.get("type").and_then(|t| t.as_str()) {
            Some("system") if frame.get("subtype").and_then(|s| s.as_str()) == Some("init") => {
                if let Some(skills) = frame.get("skills").and_then(|s| s.as_array()) {
                    self.catalog = Some(
                        skills
                            .iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect(),
                    );
                }
            }
            Some("assistant") => {
                let blocks = frame["message"]["content"].as_array();
                for block in blocks.into_iter().flatten() {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        && block.get("name").and_then(|n| n.as_str()) == Some("Skill")
                        && let Some(skill) = block["input"].get("skill").and_then(|s| s.as_str())
                    {
                        self.used.push(skill.trim_start_matches('/').to_string());
                    }
                }
            }
            _ => {}
        }
    }

    /// The turn as the ledger takes it.
    pub(crate) fn observation(&self, names: &SkillNames) -> SkillUsageObservation {
        SkillUsageObservation {
            catalog: self
                .catalog
                .as_ref()
                .map(|c| c.iter().filter_map(|n| names.usage(n)).collect()),
            used: self.used.iter().filter_map(|n| names.usage(n)).collect(),
        }
    }
}

/// What one turn launches with: the `--settings` JSON, and the names it
/// records the turn under.
pub(crate) struct LaunchSkills {
    pub settings: Option<String>,
    /// The profile's skills as the CLI lists them under `settings`.
    pub listing: Option<String>,
    pub names: SkillNames,
    pub source: Option<SkillUsageSource>,
}

/// The `--settings` JSON for an account's next turn. A skill `text` asks
/// for (`$name`, `/name`) is on whatever its listing or pin.
pub(crate) async fn launch_settings(
    listing: &SkillListing,
    runtime_id: &str,
    login_file: &Path,
    profile: Option<&ironclaw_orchestrator::runtime::ProfileSkills>,
    artifacts: bool,
    text: &str,
) -> LaunchSkills {
    let plugin = match profile {
        Some(skills) => plugin_name(&skills.plugin_dir).await,
        None => None,
    };
    let names = SkillNames::new(
        plugin,
        profile
            .into_iter()
            .flat_map(|s| s.entries.iter().map(|e| e.name.clone())),
    );
    let source = SkillUsageSource::cli_runtime(runtime_id)
        .inspect_err(|error| tracing::debug!(%error, "claude skills: runtime id not a source"))
        .ok(); // silent-ok: logged; the turn then passes only the forced overrides
    let mut overrides = BTreeMap::new();
    if let Some(source) = &source {
        import_legacy(listing, source, &names).await;
        if let Some(document) = listing.usage.document().await {
            let catalog = document.catalog(source).to_vec();
            let account = account_usage(login_file, &names).await;
            let own: BTreeSet<SkillUsageName> = catalog
                .iter()
                .filter(|n| names.is_plugin(n))
                .cloned()
                .collect();
            // Only typed words ask for skills, never expanded mention context.
            let typed = ironclaw_product_contracts::mention::selection_text(text);
            let requested = requested_skills(&typed, &catalog);
            for (name, state) in decide(&document, &catalog, &own, Some(&account), &requested) {
                overrides.insert(names.native(&name), Listed::from(state.listing));
            }
        }
    }
    if !artifacts {
        for skill in ARTIFACT_SKILLS {
            overrides.insert(skill.to_string(), Listed::Off);
        }
    }
    LaunchSkills {
        settings: settings_json(&overrides),
        listing: plugin_listing(profile, &names, &overrides),
        names,
        source,
    }
}

/// The profile's (plugin) skills as the CLI lists them under `overrides`:
/// `- name: description` on, `- name` name-only, nothing off; a skill
/// without an override is on. `None` without profile skills.
fn plugin_listing(
    profile: Option<&ironclaw_orchestrator::runtime::ProfileSkills>,
    names: &SkillNames,
    overrides: &BTreeMap<String, Listed>,
) -> Option<String> {
    let lines: Vec<String> = profile?
        .entries
        .iter()
        .filter_map(|entry| {
            let native =
                usage_name(&entry.name).map_or_else(|| entry.name.clone(), |n| names.native(&n));
            match overrides.get(&native).copied().unwrap_or(Listed::On) {
                Listed::On => Some(format!("- {native}: {}", entry.description)),
                Listed::NameOnly => Some(format!("- {native}")),
                Listed::Off => None,
            }
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// [`plugin_listing`] with no overrides: what an unmanaged turn lists.
pub(crate) fn unmanaged_listing(
    profile: Option<&ironclaw_orchestrator::runtime::ProfileSkills>,
) -> Option<String> {
    plugin_listing(profile, &SkillNames::default(), &BTreeMap::new())
}

/// Record one turn in the shared store.
pub(crate) async fn record_turn(
    usage: &CliSkillUsage,
    launch_source: Option<&SkillUsageSource>,
    names: &SkillNames,
    watch: &SkillWatch,
) {
    let Some(source) = launch_source else {
        return; // silent-ok: logged at launch
    };
    usage.record(source, watch.observation(names)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_skills_are_kept_under_their_bare_name() {
        let names = SkillNames::new(Some("ironclaw".into()), ["deploy".to_string()]);
        let mut watch = SkillWatch::default();
        watch.push_line(
            r#"{"type":"system","subtype":"init","skills":["a","ironclaw:deploy"],"tools":[]}"#,
        );
        watch.push_line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Skill","input":{"skill":"/ironclaw:deploy"}}]}}"#);
        let observation = watch.observation(&names);
        let bare = |s: &str| usage_name(s).unwrap();
        assert_eq!(observation.catalog, Some(vec![bare("a"), bare("deploy")]));
        assert_eq!(observation.used, vec![bare("deploy")]);
        assert_eq!(names.native(&bare("deploy")), "ironclaw:deploy");
        assert_eq!(names.native(&bare("a")), "a");
    }

    #[test]
    fn overrides_serialize_as_the_cli_takes_them() {
        let overrides = BTreeMap::from([
            ("a".to_string(), Listed::from(Listing::Full)),
            ("b".to_string(), Listed::from(Listing::NameOnly)),
            ("c".to_string(), Listed::from(Listing::Off)),
        ]);
        assert_eq!(
            settings_json(&overrides).unwrap(),
            r#"{"skillOverrides":{"a":"on","b":"name-only","c":"off"}}"#
        );
        assert!(settings_json(&BTreeMap::new()).is_none());
    }
}
