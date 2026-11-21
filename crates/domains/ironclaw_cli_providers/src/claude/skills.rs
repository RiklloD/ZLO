//! What the Claude CLI lists of its skills each turn, kept small without the
//! person managing it.
//!
//! Every skill the CLI can reach costs its description in every turn's
//! context. ZLO keeps the hot ones fully listed and the rest name-only
//! (`skillOverrides` in `--settings`): a name-only skill stays callable
//! through the `Skill` tool, it just loses its description.
//!
//! - **Catalog:** the `skills` of the turn's `system/init` frame, kept per
//!   account in a ZLO state file (0600, written only when it changed).
//! - **Ledger:** ZLO's own count of `Skill` calls per skill, merged read-only
//!   with the account's `.claude.json` `skillUsage` (never written).
//! - **Policy** ([`overrides`]): the profile's plugin skills, pinned skills,
//!   and up to [`HOT_CAP`] skills scoring at least [`HOT_SCORE`] stay on;
//!   everything else in the catalog goes name-only. Nothing goes off
//!   unless configured (or an artifact skill while artifacts are off).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// A skill scoring at least this is hot: one use within the past week, or
/// more uses longer ago.
pub(crate) const HOT_SCORE: f64 = 0.5;
/// At most this many skills stay listed for their score alone.
pub(crate) const HOT_CAP: usize = 12;

const DAY_MS: f64 = 86_400_000.0;

/// How an account's skills are listed.
#[derive(Debug, Clone, Default)]
pub struct SkillListing {
    /// The account's catalog and ledger (`~/.ironclaw/claude-skills/<id>.json`);
    /// `None` passes no skill overrides at all.
    pub state_file: Option<PathBuf>,
    /// Always fully listed.
    pub pinned_on: Vec<String>,
    /// Never listed.
    pub off: Vec<String>,
}

/// One skill's uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SkillUse {
    pub count: u64,
    pub last_used_ms: u64,
}

/// `skillOverrides` values the CLI takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Listed {
    On,
    NameOnly,
    Off,
}

/// The state file: what the account offers and what ZLO saw used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SkillState {
    #[serde(default)]
    pub catalog: Vec<String>,
    #[serde(default)]
    pub ledger: BTreeMap<String, SkillUse>,
}

/// Claude Code's own ranking: uses, halved every week, never below a tenth.
pub(crate) fn score(usage: SkillUse, now_ms: u64) -> f64 {
    let days = now_ms.saturating_sub(usage.last_used_ms) as f64 / DAY_MS;
    usage.count as f64 * 0.5_f64.powf(days / 7.0).max(0.1)
}

/// What each skill of the catalog is listed as. `plugin` is the profile's
/// plugin name: its skills (`<plugin>:<skill>`) are always on. `forced_off`
/// is listed off whether or not the catalog has it.
pub(crate) fn overrides(
    state: &SkillState,
    plugin: Option<&str>,
    listing: &SkillListing,
    forced_off: &[&str],
    now_ms: u64,
) -> BTreeMap<String, Listed> {
    let off: BTreeSet<&str> = listing
        .off
        .iter()
        .map(String::as_str)
        .chain(forced_off.iter().copied())
        .collect();
    let pinned: BTreeSet<&str> = listing.pinned_on.iter().map(String::as_str).collect();
    let own =
        |name: &str| plugin.is_some_and(|p| name.split_once(':').is_some_and(|(q, _)| q == p));
    let mut scored: Vec<(&str, f64)> = state
        .catalog
        .iter()
        .map(String::as_str)
        .filter(|n| !off.contains(n) && !pinned.contains(n) && !own(n))
        .filter_map(|n| {
            let s = score(state.ledger.get(n).copied()?, now_ms);
            (s >= HOT_SCORE).then_some((n, s))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    let hot: BTreeSet<&str> = scored.into_iter().take(HOT_CAP).map(|(n, _)| n).collect();
    let mut out: BTreeMap<String, Listed> = state
        .catalog
        .iter()
        .map(|name| {
            let n = name.as_str();
            let listed = if off.contains(n) {
                Listed::Off
            } else if pinned.contains(n) || own(n) || hot.contains(n) {
                Listed::On
            } else {
                Listed::NameOnly
            };
            (name.clone(), listed)
        })
        .collect();
    for name in off {
        out.insert(name.to_string(), Listed::Off);
    }
    out
}

/// The `--settings` JSON carrying `overrides`; `None` when there are none.
pub(crate) fn settings_json(overrides: &BTreeMap<String, Listed>) -> Option<String> {
    (!overrides.is_empty()).then(|| serde_json::json!({ "skillOverrides": overrides }).to_string())
}

/// ZLO's ledger with the account's own `skillUsage` merged in (the larger
/// count and the later use of each), as the policy reads it.
pub(crate) fn merged_ledger(
    own: &BTreeMap<String, SkillUse>,
    account: &BTreeMap<String, SkillUse>,
) -> BTreeMap<String, SkillUse> {
    let mut out = own.clone();
    for (name, theirs) in account {
        let entry = out.entry(name.clone()).or_default();
        entry.count = entry.count.max(theirs.count);
        entry.last_used_ms = entry.last_used_ms.max(theirs.last_used_ms);
    }
    out
}

/// `skillUsage` of an account's `.claude.json`; empty when absent or
/// unreadable (the ledger is advisory).
pub(crate) async fn account_usage(login_file: &Path) -> BTreeMap<String, SkillUse> {
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
            return BTreeMap::new();
        }
    };
    match serde_json::from_str::<Account>(&text) {
        Ok(account) => account
            .skill_usage
            .into_iter()
            .map(|(name, e)| {
                (
                    name,
                    SkillUse {
                        count: e.usage_count,
                        last_used_ms: e.last_used_at,
                    },
                )
            })
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "claude skills: account usage unparsed");
            BTreeMap::new()
        }
    }
}

/// The state file; empty when it does not exist yet or is unreadable.
pub(crate) async fn load(path: &Path) -> SkillState {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::debug!(%error, "claude skills: state unparsed; starting over");
            SkillState::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SkillState::default(),
        Err(error) => {
            tracing::debug!(%error, "claude skills: state not read");
            SkillState::default()
        }
    }
}

/// Replace the state file atomically (temp file, then rename), 0600.
pub(crate) async fn save(path: &Path, state: &SkillState) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .await?;
    file.write_all(&body).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&tmp, path).await
}

/// The profile plugin's name (`.claude-plugin/plugin.json`).
pub(crate) async fn plugin_name(plugin_dir: &Path) -> Option<String> {
    let text = tokio::fs::read_to_string(plugin_dir.join(".claude-plugin/plugin.json"))
        .await
        .ok()?; // silent-ok: without a manifest the CLI loads no plugin skills either
    let value: serde_json::Value = serde_json::from_str(&text).ok()?; // silent-ok: same
    value.get("name")?.as_str().map(str::to_string)
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

    /// Fold this turn into `state`; whether anything changed.
    pub(crate) fn apply(&self, state: &mut SkillState, now_ms: u64) -> bool {
        let mut changed = false;
        if let Some(catalog) = &self.catalog
            && *catalog != state.catalog
        {
            state.catalog = catalog.clone();
            changed = true;
        }
        for name in &self.used {
            let entry = state.ledger.entry(name.clone()).or_default();
            entry.count += 1;
            entry.last_used_ms = entry.last_used_ms.max(now_ms);
            changed = true;
        }
        changed
    }

    /// Record the turn in the account's state file, when it changed it.
    pub(crate) async fn record(&self, path: &Path, now_ms: u64) {
        if self.catalog.is_none() && self.used.is_empty() {
            return;
        }
        let mut state = load(path).await;
        if !self.apply(&mut state, now_ms) {
            return;
        }
        if let Err(error) = save(path, &state).await {
            tracing::debug!(%error, "claude skills: state not saved");
        }
    }
}

/// The `--settings` JSON for an account's next turn.
pub(crate) async fn launch_settings(
    listing: &SkillListing,
    login_file: &Path,
    plugin_dir: Option<&Path>,
    artifacts: bool,
    now_ms: u64,
) -> Option<String> {
    let mut state = match &listing.state_file {
        Some(path) => load(path).await,
        None => SkillState::default(),
    };
    if listing.state_file.is_some() {
        state.ledger = merged_ledger(&state.ledger, &account_usage(login_file).await);
    }
    let plugin = match plugin_dir {
        Some(dir) => plugin_name(dir).await,
        None => None,
    };
    let forced: &[&str] = if artifacts { &[] } else { &ARTIFACT_SKILLS };
    settings_json(&overrides(
        &state,
        plugin.as_deref(),
        listing,
        forced,
        now_ms,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const WEEK: u64 = 7 * 86_400_000;

    fn used(count: u64, ago: u64) -> SkillUse {
        SkillUse {
            count,
            last_used_ms: NOW - ago,
        }
    }

    fn state(catalog: &[&str], ledger: &[(&str, SkillUse)]) -> SkillState {
        SkillState {
            catalog: catalog.iter().map(|s| s.to_string()).collect(),
            ledger: ledger.iter().map(|(n, u)| (n.to_string(), *u)).collect(),
        }
    }

    #[test]
    fn score_halves_weekly_and_floors_at_a_tenth() {
        assert_eq!(score(used(4, 0), NOW), 4.0);
        assert!((score(used(4, WEEK), NOW) - 2.0).abs() < 1e-9);
        assert!((score(used(4, 52 * WEEK), NOW) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn hot_skills_stay_on_and_cold_ones_go_name_only() {
        let s = state(
            &["grilling", "pdf", "ironclaw:deploy", "old", "pinned"],
            &[("grilling", used(3, 0)), ("old", used(1, 8 * WEEK))],
        );
        let listing = SkillListing {
            pinned_on: vec!["pinned".into()],
            off: vec!["pdf".into()],
            ..SkillListing::default()
        };
        let got = overrides(&s, Some("ironclaw"), &listing, &[], NOW);
        assert_eq!(got["grilling"], Listed::On);
        assert_eq!(got["ironclaw:deploy"], Listed::On);
        assert_eq!(got["pinned"], Listed::On);
        assert_eq!(got["old"], Listed::NameOnly);
        assert_eq!(got["pdf"], Listed::Off);
        assert_eq!(got.len(), 5, "skills not in the catalog get no entry");
    }

    #[test]
    fn the_hot_set_is_capped_by_score() {
        let names: Vec<String> = (0..20).map(|i| format!("s{i:02}")).collect();
        let catalog: Vec<&str> = names.iter().map(String::as_str).collect();
        let ledger: Vec<(&str, SkillUse)> = catalog
            .iter()
            .enumerate()
            .map(|(i, n)| (*n, used(i as u64 + 1, 0)))
            .collect();
        let got = overrides(
            &state(&catalog, &ledger),
            None,
            &SkillListing::default(),
            &[],
            NOW,
        );
        let on: Vec<&str> = got
            .iter()
            .filter(|(_, l)| **l == Listed::On)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(on.len(), HOT_CAP);
        assert!(on.contains(&"s19") && !on.contains(&"s07"));
    }

    #[test]
    fn forced_off_applies_without_a_catalog_and_nothing_else_does() {
        let got = overrides(
            &SkillState::default(),
            None,
            &SkillListing::default(),
            &ARTIFACT_SKILLS,
            NOW,
        );
        assert_eq!(got.len(), 3);
        assert!(got.values().all(|l| *l == Listed::Off));
        assert_eq!(
            settings_json(&got).unwrap(),
            r#"{"skillOverrides":{"artifact-capabilities":"off","artifact-design":"off","artifact-diagramming":"off"}}"#
        );
        assert!(
            settings_json(&overrides(
                &SkillState::default(),
                None,
                &SkillListing::default(),
                &[],
                NOW
            ))
            .is_none()
        );
    }

    #[test]
    fn merging_keeps_the_larger_count_and_later_use() {
        let own = BTreeMap::from([("a".to_string(), used(5, WEEK))]);
        let theirs = BTreeMap::from([("a".to_string(), used(2, 0)), ("b".to_string(), used(1, 0))]);
        let got = merged_ledger(&own, &theirs);
        assert_eq!(got["a"], used(5, 0));
        assert_eq!(got["b"], used(1, 0));
    }

    #[test]
    fn the_watch_records_the_catalog_and_skill_calls() {
        let mut watch = SkillWatch::default();
        watch.push_line(r#"{"type":"system","subtype":"init","skills":["a","b:c"],"tools":[]}"#);
        watch.push_line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Skill","input":{"skill":"b:c"}}]}}"#);
        let mut s = SkillState::default();
        assert!(watch.apply(&mut s, NOW));
        assert_eq!(s.catalog, vec!["a", "b:c"]);
        assert_eq!(s.ledger["b:c"], used(1, 0));
        // The same catalog, no calls: nothing to write.
        let mut again = SkillWatch::default();
        again.push_line(r#"{"type":"system","subtype":"init","skills":["a","b:c"]}"#);
        assert!(!again.apply(&mut s, NOW));
    }

    #[tokio::test]
    async fn state_round_trips_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills").join("claude-cli.json");
        let s = state(&["a"], &[("a", used(1, 0))]);
        save(&path, &s).await.unwrap();
        assert_eq!(load(&path).await, s);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
