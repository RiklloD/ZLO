//! Portable `config.toml` section extraction and merge for continuous config
//! sync.
//!
//! Machine-local sections ([`MACHINE_LOCAL_SECTIONS`]) are NEVER overwritten
//! by sync. Every other section is portable: together they replace the
//! target's portable sections, so removing a portable key on the source
//! reverts it to its compiled default on the target.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config_file::{
    RebornConfigFile, RebornConfigFileError, RebornConfigFileUpdateError, acquire_update_lock,
    write_config_text_atomic,
};
use crate::config_reload::{ConfigChangeSet, diff_config_files};
use crate::config_restore::read_config_text;

/// Top-level `config.toml` keys that belong to one machine and never travel:
/// its boot profile, identity, storage, listeners, remote access and update
/// method. Every other section is portable, so a section added to
/// [`RebornConfigFile`] later syncs unless it is listed here.
pub(crate) const MACHINE_LOCAL_SECTIONS: &[&str] = &[
    "api_version",
    "boot",
    "identity",
    "storage",
    "webui",
    "remote",
    "update",
];

/// The section whose `base_url` may name a server on the source machine.
const EMBEDDINGS_SECTION: &str = "embeddings";

fn is_machine_local(section: &str) -> bool {
    MACHINE_LOCAL_SECTIONS.contains(&section)
}

/// The portable sections of a `config.toml`, by top-level section name.
/// Built by [`extract_portable_sections`], which leaves every machine-local
/// section out; one received from elsewhere is checked by
/// [`validate_portable_sections`] before it is used.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct PortableConfigSections(pub toml::Table);

/// Outcome of merging and atomically writing portable sections into a target
/// `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableConfigApplyResult {
    /// Whether `config.toml` on disk actually changed.
    pub changed: bool,
    /// Keys that changed, split into live-reloadable vs restart-required.
    pub changes: ConfigChangeSet,
    /// Non-fatal warnings (e.g., loopback `[embeddings]` endpoint skipped).
    pub warnings: Vec<String>,
}

fn invalid(field: &str, reason: impl ToString) -> RebornConfigFileError {
    RebornConfigFileError::InvalidField {
        path: "config.toml".to_string(),
        field: field.to_string(),
        reason: reason.to_string(),
    }
}

fn to_table(file: &RebornConfigFile) -> Result<toml::Table, RebornConfigFileError> {
    toml::Table::try_from(file).map_err(|error| invalid("portable", error))
}

/// Extract the portable subset of a parsed [`RebornConfigFile`].
pub fn extract_portable_sections(
    file: &RebornConfigFile,
) -> Result<PortableConfigSections, RebornConfigFileError> {
    let mut table = to_table(file)?;
    table.retain(|section, _| !is_machine_local(section));
    // A sign-in written into an address (`https://user:pass@host`) never
    // leaves this machine; the receiver sees that one was there and keeps
    // its own section.
    for (_, value) in table.iter_mut() {
        redact_url_credentials(value);
    }
    Ok(PortableConfigSections(table))
}

/// `incoming`'s portable sections over `target`'s machine-local ones: the
/// portable sections are replaced as a set, so one removed at the source
/// goes back to its compiled default here.
///
/// A loopback `[embeddings]` `base_url` (`127.0.0.1`, `localhost`, `::1`)
/// names a server on the source machine: that section is skipped (the
/// target keeps its own) and reported in the warnings.
pub fn merge_portable_sections(
    target: Option<&RebornConfigFile>,
    incoming: &PortableConfigSections,
) -> Result<(RebornConfigFile, Vec<String>), RebornConfigFileError> {
    let mut merged = match target {
        Some(file) => to_table(file)?,
        None => toml::Table::new(),
    };
    let own = merged.clone();
    merged.retain(|section, _| is_machine_local(section));
    let mut warnings = Vec::new();
    for (section, value) in &incoming.0 {
        if is_machine_local(section) {
            return Err(invalid(
                section,
                "is a machine-local section and does not sync",
            ));
        }
        let loopback = section == EMBEDDINGS_SECTION
            && value
                .get("base_url")
                .and_then(toml::Value::as_str)
                .is_some_and(is_loopback_base_url);
        let reason = if loopback {
            Some("loopback base_url is machine-local")
        } else if carries_url_credentials(value) {
            Some("an address in it carries a sign-in; set it on this computer")
        } else {
            None
        };
        if let Some(reason) = reason {
            warnings.push(format!("skipped [{section}]: {reason}"));
            if let Some(own) = own.get(section) {
                merged.insert(section.clone(), own.clone());
            }
            continue;
        }
        merged.insert(section.clone(), value.clone());
    }
    let file: RebornConfigFile = merged
        .try_into()
        .map_err(|error| invalid("portable", error))?;
    Ok((file, warnings))
}

/// Validate portable sections by round-tripping through [`RebornConfigFile::parse_text`],
/// which enforces structural rules and [`crate::secrets_guard`] (rejecting
/// inline secrets). A machine-local section among them is refused.
pub fn validate_portable_sections(
    sections: &PortableConfigSections,
) -> Result<(), RebornConfigFileError> {
    let probe = merge_portable_sections(None, sections)?.0;
    let text = toml::to_string_pretty(&probe).map_err(|error| invalid("portable", error))?;
    RebornConfigFile::parse_text(&text, Path::new("config.toml"))?;
    Ok(())
}

/// Atomically merge `incoming` portable sections into `config_path`, preserving
/// every machine-local section ([`MACHINE_LOCAL_SECTIONS`]).
///
/// Validates the merged document before writing. If no effective key changed,
/// leaves the file untouched on disk and reports `changed = false`.
pub fn apply_portable_config_file(
    config_path: &Path,
    incoming: &PortableConfigSections,
) -> Result<PortableConfigApplyResult, RebornConfigFileUpdateError> {
    let validate = |error: RebornConfigFileError| RebornConfigFileUpdateError::Validate {
        path: config_path.to_path_buf(),
        source: Box::new(error),
    };
    let _lock = acquire_update_lock(config_path)?;
    let existing_text = read_config_text(config_path)?;
    let previous = match existing_text.as_deref() {
        Some(text) => Some(RebornConfigFile::parse_text(text, config_path).map_err(validate)?),
        None => None,
    };

    let (merged, warnings) =
        merge_portable_sections(previous.as_ref(), incoming).map_err(validate)?;
    let rendered =
        toml::to_string_pretty(&merged).map_err(|error| validate(invalid("portable", error)))?;
    // Re-parse through `RebornConfigFile::parse_text` so inline-secret guards
    // and section validators run before writing a single byte to disk.
    let validated = RebornConfigFile::parse_text(&rendered, config_path).map_err(validate)?;

    let changes = diff_config_files(previous.as_ref(), Some(&validated))
        .map_err(|error| validate(invalid("diff", error)))?;

    if changes.is_empty() {
        return Ok(PortableConfigApplyResult {
            changed: false,
            changes,
            warnings,
        });
    }

    write_config_text_atomic(config_path, &rendered)?;
    Ok(PortableConfigApplyResult {
        changed: true,
        changes,
        warnings,
    })
}

/// Whether `raw_url` targets a loopback host (`localhost`, `127.x.x.x`, `::1`).
/// What stands in for a sign-in removed from an address.
const REDACTED_USERINFO: &str = "redacted";

/// The `user[:password]` part of an address, when it has one.
fn url_userinfo(raw: &str) -> Option<std::ops::Range<usize>> {
    let start = raw.find("://")? + 3;
    let authority_len = raw[start..]
        .find(['/', '?', '#'])
        .unwrap_or(raw.len() - start);
    let at = raw[start..start + authority_len].rfind('@')?;
    Some(start..start + at)
}

fn redact_url_credentials(value: &mut toml::Value) {
    match value {
        toml::Value::String(text) => {
            if let Some(userinfo) = url_userinfo(text) {
                text.replace_range(userinfo, REDACTED_USERINFO);
            }
        }
        toml::Value::Array(items) => items.iter_mut().for_each(redact_url_credentials),
        toml::Value::Table(table) => table
            .iter_mut()
            .for_each(|(_, value)| redact_url_credentials(value)),
        _ => {}
    }
}

fn carries_url_credentials(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => url_userinfo(text).is_some(),
        toml::Value::Array(items) => items.iter().any(carries_url_credentials),
        toml::Value::Table(table) => table.values().any(carries_url_credentials),
        _ => false,
    }
}

pub fn is_loopback_base_url(raw_url: &str) -> bool {
    let trimmed = raw_url.trim();
    let after_scheme = match trimmed.find("://") {
        Some(idx) => &trimmed[idx + 3..],
        None => trimmed,
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    let lower = host.to_ascii_lowercase();
    lower == "localhost"
        || lower == "::1"
        || lower == "127.0.0.1"
        || lower.starts_with("127.")
        || lower.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_base_url_detection_covers_ipv4_ipv6_and_localhost() {
        for loopback in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:11434/v1",
            "https://LOCALHOST/v1",
            "http://[::1]:11434/v1",
            "http://127.0.1.1:8080/v1",
        ] {
            assert!(
                is_loopback_base_url(loopback),
                "expected loopback: {loopback}"
            );
        }
        for remote in [
            "https://api.openai.com/v1",
            "https://10.0.0.5:11434/v1",
            "https://my-localhost.example.com/v1",
        ] {
            assert!(!is_loopback_base_url(remote), "expected remote: {remote}");
        }
    }

    #[test]
    fn apply_portable_preserves_machine_local_sections_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        let target_toml = r#"
[boot]
profile = "standalone"

[webui]
listen_host = "127.0.0.1"
listen_port = 9090

[remote]
enabled = true
owner_logins = ["box@example.com"]
tailnet_url = "https://box.tail0000.ts.net"

[budget]
user_daily_usd = 5.0
"#;
        std::fs::write(&path, target_toml).expect("write target");

        let source_toml = r#"
[boot]
profile = "local-dev"

[webui]
listen_host = "127.0.0.1"
listen_port = 3000

[remote]
enabled = false

[budget]
user_daily_usd = 25.0

[runner]
worker_count = 4
"#;
        let source_file = RebornConfigFile::parse_text(source_toml, Path::new("source.toml"))
            .expect("parse source");
        let portable = extract_portable_sections(&source_file).expect("extract");
        for local in MACHINE_LOCAL_SECTIONS {
            assert!(!portable.0.contains_key(*local), "{local} never travels");
        }

        let first = apply_portable_config_file(&path, &portable).expect("first apply");
        assert!(first.changed);
        assert!(
            first
                .changes
                .restart_required
                .contains(&"budget".to_string())
        );
        assert!(
            first
                .changes
                .restart_required
                .contains(&"runner".to_string())
        );
        assert!(
            !first
                .changes
                .restart_required
                .contains(&"webui".to_string())
        );
        assert!(
            !first
                .changes
                .restart_required
                .contains(&"remote".to_string())
        );

        let after_text = std::fs::read_to_string(&path).expect("read after");
        let after = RebornConfigFile::parse_text(&after_text, &path).expect("parse after");
        // Machine-local sections preserved verbatim.
        assert_eq!(after.webui.as_ref().and_then(|w| w.listen_port), Some(9090));
        assert_eq!(
            after.remote.as_ref().and_then(|r| r.owner_logins.clone()),
            Some(vec!["box@example.com".to_string()])
        );
        assert_eq!(
            after.boot.as_ref().and_then(|b| b.profile.as_deref()),
            Some("standalone")
        );
        // Portable sections arrived.
        assert_eq!(
            after.budget.as_ref().and_then(|b| b.user_daily_usd),
            Some(25.0)
        );
        assert_eq!(after.runner.as_ref().and_then(|r| r.worker_count), Some(4));

        // Second apply is a no-op.
        let second = apply_portable_config_file(&path, &portable).expect("second apply");
        assert!(!second.changed);
        assert!(second.changes.is_empty());
    }

    #[test]
    fn loopback_embeddings_endpoint_is_skipped_with_warning() {
        let incoming: PortableConfigSections = toml::from_str(
            "[embeddings]\nprovider = \"ollama\"\nmodel = \"nomic-embed-text\"\ndimension = 768\nbase_url = \"http://127.0.0.1:11434/v1\"\n",
        )
        .expect("sections");
        let (merged, warnings) = merge_portable_sections(None, &incoming).expect("merge");
        assert!(merged.embeddings.is_none());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("embeddings"));
    }

    #[test]
    fn a_sign_in_inside_an_address_never_leaves_and_the_receiver_keeps_its_own() {
        let source = RebornConfigFile::parse_text(
            "[embeddings]\nprovider = \"openai\"\nmodel = \"m\"\ndimension = 8\nbase_url = \"https://alice:hunter2@embed.example.com/v1\"\n",
            Path::new("config.toml"),
        )
        .expect("source");
        let exported = extract_portable_sections(&source).expect("extract");
        let text = toml::to_string(&exported).expect("toml");
        assert!(
            !text.contains("hunter2") && !text.contains("alice"),
            "{text}"
        );
        assert!(
            text.contains("https://redacted@embed.example.com/v1"),
            "{text}"
        );

        let target = RebornConfigFile::parse_text(
            "[embeddings]\nprovider = \"openai\"\nmodel = \"own\"\ndimension = 8\nbase_url = \"https://embed.internal/v1\"\n",
            Path::new("config.toml"),
        )
        .expect("target");
        let (merged, warnings) = merge_portable_sections(Some(&target), &exported).expect("merge");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("[embeddings]"), "{warnings:?}");
        let kept = toml::to_string(&merged).expect("toml");
        assert!(kept.contains("https://embed.internal/v1"), "{kept}");
        assert!(!kept.contains("redacted"), "{kept}");

        assert_eq!(url_userinfo("https://host/path@x"), None);
        assert_eq!(url_userinfo("plain text with @ sign"), None);
    }

    #[test]
    fn a_machine_local_section_in_a_received_set_is_refused() {
        let incoming: PortableConfigSections =
            toml::from_str("[webui]\nlisten_port = 1\n").expect("sections");
        assert!(validate_portable_sections(&incoming).is_err());
    }
}
