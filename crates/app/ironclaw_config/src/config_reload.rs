//! Reload classification for `config.toml`: which changed keys a running
//! server may apply live, and which need a restart.
//!
//! This crate owns the schema, so it owns the table that says which parts of
//! it are safe to swap while the server runs. It does not apply anything —
//! the reloader that does lives with the owner of each live setting
//! (`ironclaw_operator`'s config reloader for the LLM slot). Keeping the
//! classification here means a section added to [`RebornConfigFile`] later is
//! restart-required until someone deliberately lists it as live: the safe
//! direction for a server whose trust graph is built once at boot.
//!
//! Keys are dotted paths: a top-level section name (`webui`, `storage`, …),
//! except `[llm]`, which is compared per slot (`llm.default`, `llm.mission`)
//! because only the `default` slot has a live seam.

use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

use crate::config_file::{RebornConfigFile, RebornConfigFileError};

/// Config keys the running server can apply without a restart, when the
/// matching live seam is wired. Everything else is restart-required.
///
/// - `llm.default` — provider/model/base-URL/key-env selection for the default
///   LLM slot. Applied by rebuilding the provider chain and swapping it into
///   the live provider; the key itself is still resolved through the env var
///   or the encrypted secret store, never read from this file.
pub const LIVE_RELOADABLE_CONFIG_KEYS: &[&str] = &["llm.default"];

/// Whether a changed key can be applied to a running server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKeyReloadability {
    /// Applied live through the key's owner seam.
    Live,
    /// Read once at boot, or part of the trust/authorization graph: the new
    /// value takes effect only after a restart.
    RestartRequired,
}

/// Classify one dotted config key.
pub fn config_key_reloadability(key: &str) -> ConfigKeyReloadability {
    if LIVE_RELOADABLE_CONFIG_KEYS.contains(&key) {
        ConfigKeyReloadability::Live
    } else {
        ConfigKeyReloadability::RestartRequired
    }
}

/// Keys that differ between two parsed config files, split by reloadability.
/// Both lists are sorted and free of duplicates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigChangeSet {
    pub live: Vec<String>,
    pub restart_required: Vec<String>,
}

impl ConfigChangeSet {
    pub fn is_empty(&self) -> bool {
        self.live.is_empty() && self.restart_required.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum ConfigReloadDiffError {
    /// A parsed file could not be re-serialized for comparison. Not reachable
    /// for a file that came out of [`RebornConfigFile::parse_text`]; kept as a
    /// typed error rather than a panic.
    #[error("could not compare config files: {reason}")]
    Serialize { reason: String },
}

/// Compare two parsed config files (`None` = file absent) and report which
/// keys changed. An empty section and an absent section compare equal, so
/// adding `[webui]` with nothing in it is not a change.
pub fn diff_config_files(
    previous: Option<&RebornConfigFile>,
    next: Option<&RebornConfigFile>,
) -> Result<ConfigChangeSet, ConfigReloadDiffError> {
    let previous = flatten(previous)?;
    let next = flatten(next)?;
    let mut changes = ConfigChangeSet::default();
    let keys: std::collections::BTreeSet<&String> = previous.keys().chain(next.keys()).collect();
    for key in keys {
        if previous.get(key) == next.get(key) {
            continue;
        }
        match config_key_reloadability(key) {
            ConfigKeyReloadability::Live => changes.live.push(key.clone()),
            ConfigKeyReloadability::RestartRequired => changes.restart_required.push(key.clone()),
        }
    }
    Ok(changes)
}

/// Split a file into `dotted key -> value`, dropping empty tables.
fn flatten(
    file: Option<&RebornConfigFile>,
) -> Result<BTreeMap<String, toml::Value>, ConfigReloadDiffError> {
    let mut out = BTreeMap::new();
    let Some(file) = file else {
        return Ok(out);
    };
    let value = toml::Value::try_from(file).map_err(|error| ConfigReloadDiffError::Serialize {
        reason: error.to_string(),
    })?;
    let toml::Value::Table(root) = value else {
        return Err(ConfigReloadDiffError::Serialize {
            reason: "config file did not serialize to a table".to_string(),
        });
    };
    for (section, value) in root {
        if section == "llm"
            && let toml::Value::Table(slots) = value
        {
            for (slot, value) in slots {
                insert_non_empty(&mut out, format!("llm.{slot}"), value);
            }
            continue;
        }
        insert_non_empty(&mut out, section, value);
    }
    Ok(out)
}

fn insert_non_empty(out: &mut BTreeMap<String, toml::Value>, key: String, value: toml::Value) {
    if matches!(&value, toml::Value::Table(table) if table.is_empty()) {
        return;
    }
    out.insert(key, value);
}

/// What one reload attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigReloadOutcome {
    /// The file matches what is already in effect.
    Unchanged,
    /// Every live-reloadable change was applied. `restart_required` on the
    /// report may still be non-empty when the same edit also touched a
    /// boot-time key.
    Applied,
    /// Only restart-required keys changed; nothing was applied.
    RestartRequired,
    /// The new file failed validation, or a live seam refused it. Nothing was
    /// applied and the previous configuration stays fully in effect.
    Rejected { reason: String },
}

/// Result of a reload attempt, for logs and operator surfaces. Carries key
/// names and redacted reasons only — never config values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigReloadReport {
    pub outcome: ConfigReloadOutcome,
    /// Keys whose new value is now in effect.
    pub applied: Vec<String>,
    /// Keys that changed on disk but keep their boot value until restart.
    pub restart_required: Vec<String>,
}

impl ConfigReloadReport {
    pub fn unchanged() -> Self {
        Self {
            outcome: ConfigReloadOutcome::Unchanged,
            applied: Vec::new(),
            restart_required: Vec::new(),
        }
    }

    pub fn rejected(reason: impl Into<String>, restart_required: Vec<String>) -> Self {
        Self {
            outcome: ConfigReloadOutcome::Rejected {
                reason: reason.into(),
            },
            applied: Vec::new(),
            restart_required,
        }
    }
}

impl fmt::Display for ConfigReloadReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.outcome {
            ConfigReloadOutcome::Unchanged => f.write_str("config unchanged")?,
            ConfigReloadOutcome::Applied => write!(f, "applied {}", self.applied.join(", "))?,
            ConfigReloadOutcome::RestartRequired => f.write_str("nothing applied")?,
            ConfigReloadOutcome::Rejected { reason } => {
                write!(f, "rejected, previous config stays in effect: {reason}")?
            }
        }
        if !self.restart_required.is_empty() {
            write!(
                f,
                "; restart required for {}",
                self.restart_required.join(", ")
            )?;
        }
        Ok(())
    }
}

impl RebornConfigFileError {
    /// A reason safe to show on a reload report or in a log line.
    ///
    /// The plain `Display` of a TOML parse error quotes the offending source
    /// line, and a field-validation reason can quote the field's value; either
    /// can be a secret an operator pasted by mistake. This keeps the location
    /// and the kind of failure and drops the quoted text.
    pub fn redacted_reason(&self) -> String {
        match self {
            Self::Toml { path, source } => {
                let location = source
                    .span()
                    .map(|span| format!(" (byte offset {})", span.start))
                    .unwrap_or_default();
                format!(
                    "config file `{path}` is not valid TOML{location}: {}",
                    source.message()
                )
            }
            Self::InvalidField { path, field, .. } => {
                format!("config file `{path}` field `{field}` is invalid")
            }
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(text: &str) -> RebornConfigFile {
        RebornConfigFile::parse_text(text, Path::new("/cfg/config.toml")).expect("config parses")
    }

    #[test]
    fn identical_files_have_no_changes() {
        let a = parse("[llm.default]\nprovider_id = \"nearai\"\nmodel = \"m\"\n");
        let changes = diff_config_files(Some(&a), Some(&a.clone())).expect("diff");
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn empty_section_equals_absent_section_and_missing_file() {
        let empty = parse("[webui]\n");
        assert!(
            diff_config_files(None, Some(&empty))
                .expect("diff")
                .is_empty()
        );
        assert!(
            diff_config_files(Some(&RebornConfigFile::default()), Some(&empty))
                .expect("diff")
                .is_empty()
        );
    }

    #[test]
    fn default_llm_slot_change_is_live() {
        let a = parse("[llm.default]\nprovider_id = \"nearai\"\nmodel = \"a\"\n");
        let b = parse("[llm.default]\nprovider_id = \"nearai\"\nmodel = \"b\"\n");
        let changes = diff_config_files(Some(&a), Some(&b)).expect("diff");
        assert_eq!(changes.live, vec!["llm.default".to_string()]);
        assert!(changes.restart_required.is_empty());
    }

    #[test]
    fn other_llm_slots_and_boot_sections_are_restart_required() {
        let a = parse("[llm.default]\nmodel = \"a\"\n");
        let b = parse(
            "[llm.default]\nmodel = \"a\"\n\
             [llm.mission]\nmodel = \"m\"\n\
             [webui]\nlisten_port = 4000\nallowed_origins = [\"https://x.example\"]\n\
             [storage]\nbackend = \"postgres\"\n\
             [identity]\ntenant = \"other\"\n\
             [policy]\ndefault_approval_policy = \"ask_always\"\n",
        );
        let changes = diff_config_files(Some(&a), Some(&b)).expect("diff");
        assert!(changes.live.is_empty(), "{changes:?}");
        assert_eq!(
            changes.restart_required,
            vec!["identity", "llm.mission", "policy", "storage", "webui"]
        );
    }

    #[test]
    fn mixed_edit_splits_live_from_restart_required() {
        let a = parse("[llm.default]\nmodel = \"a\"\n[runner]\nworker_count = 1\n");
        let b = parse("[llm.default]\nmodel = \"b\"\n[runner]\nworker_count = 2\n");
        let changes = diff_config_files(Some(&a), Some(&b)).expect("diff");
        assert_eq!(changes.live, vec!["llm.default"]);
        assert_eq!(changes.restart_required, vec!["runner"]);
    }

    #[test]
    fn removing_the_default_slot_is_a_live_change() {
        let a = parse("[llm.default]\nmodel = \"a\"\n");
        let changes = diff_config_files(Some(&a), None).expect("diff");
        assert_eq!(changes.live, vec!["llm.default"]);
    }

    #[test]
    fn unknown_future_keys_default_to_restart_required() {
        assert_eq!(
            config_key_reloadability("some_new_section"),
            ConfigKeyReloadability::RestartRequired
        );
        assert_eq!(
            config_key_reloadability("llm.default"),
            ConfigKeyReloadability::Live
        );
    }

    #[test]
    fn redacted_reason_does_not_quote_the_offending_line() {
        let secret = "sk-live-0123456789abcdef0123456789abcdef";
        let error = RebornConfigFile::parse_text(
            &format!("[llm.default]\nmodel = {secret}\n"),
            Path::new("/cfg/config.toml"),
        )
        .expect_err("bare word is not valid TOML");
        let reason = error.redacted_reason();
        assert!(!reason.contains(secret), "reason leaked the line: {reason}");
        assert!(reason.contains("not valid TOML"), "{reason}");
    }

    #[test]
    fn report_display_names_keys_only() {
        let report = ConfigReloadReport {
            outcome: ConfigReloadOutcome::Applied,
            applied: vec!["llm.default".to_string()],
            restart_required: vec!["webui".to_string()],
        };
        assert_eq!(
            report.to_string(),
            "applied llm.default; restart required for webui"
        );
    }
}
