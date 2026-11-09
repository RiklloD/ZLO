//! `[remote]` — remote access to this installation over the owner's
//! tailnet (`docs/internal/plans/2026-11-08-zlo-remote-environments.md`).
//!
//! ```toml
//! [remote]
//! enabled = true
//! identity_port = 3001          # loopback port that `tailscale serve` targets
//! owner_logins = ["owner@example.com"]
//! trust_local_users = false
//! tailnet_url = "https://studio.tail9915dd.ts.net"
//! https_port = 443
//! ```
//!
//! `ironclaw remote enable` writes this section and `remote disable` sets
//! `enabled = false`; `ironclaw serve` reads it. Nothing here touches the
//! network or Tailscale: this module only parses, validates and rewrites the
//! section.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config_file::{
    RebornConfigFileError, RebornConfigFileUpdateError, acquire_update_lock, load_edit_document,
    write_edit_document,
};

/// Loopback port the identity listener binds when `[remote]` names none.
pub const DEFAULT_REMOTE_IDENTITY_PORT: u16 = 3001;
/// HTTPS port `tailscale serve` publishes when `[remote]` names none.
pub const DEFAULT_REMOTE_HTTPS_PORT: u16 = 443;

/// `[remote]` as written in `config.toml`. Every field is optional so a
/// sparse section parses; [`RemoteSection::resolve`] applies the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSection {
    /// Whether `ironclaw serve` starts the identity listener. Default `false`.
    pub enabled: Option<bool>,
    /// Loopback TCP port of the identity listener (the `tailscale serve`
    /// target). Default [`DEFAULT_REMOTE_IDENTITY_PORT`].
    pub identity_port: Option<u16>,
    /// Tailnet logins (`User.LoginName`) allowed to pair in one click.
    pub owner_logins: Option<Vec<String>>,
    /// Allow one-click pairing even though other OS accounts exist on this
    /// machine (they could reach the loopback identity port and forge the
    /// identity header). Default `false`.
    pub trust_local_users: Option<bool>,
    /// The `tailscale serve` HTTPS URL, advertised in the descriptor.
    pub tailnet_url: Option<String>,
    /// HTTPS port `tailscale serve` publishes. Default
    /// [`DEFAULT_REMOTE_HTTPS_PORT`].
    pub https_port: Option<u16>,
}

/// `[remote]` with defaults applied, for an enabled section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSettings {
    pub identity_port: u16,
    pub owner_logins: Vec<String>,
    pub tailnet_url: String,
    pub https_port: u16,
    pub trust_local_users: bool,
}

impl RemoteSettings {
    /// Whether one-click tailnet pairing is allowed, given whether other OS
    /// login accounts exist on this machine right now (they could reach the
    /// loopback identity port and forge the identity header). Needs at
    /// least one owner login.
    pub fn one_click(&self, other_local_users: bool) -> bool {
        !self.owner_logins.is_empty() && (self.trust_local_users || !other_local_users)
    }
}

impl RemoteSection {
    /// `Some` only when `enabled = true`.
    pub fn resolve(&self) -> Option<RemoteSettings> {
        if self.enabled != Some(true) {
            return None;
        }
        Some(RemoteSettings {
            identity_port: self.identity_port.unwrap_or(DEFAULT_REMOTE_IDENTITY_PORT),
            owner_logins: self.owner_logins.clone().unwrap_or_default(),
            tailnet_url: self.tailnet_url.clone().unwrap_or_default(),
            https_port: self.https_port.unwrap_or(DEFAULT_REMOTE_HTTPS_PORT),
            trust_local_users: self.trust_local_users == Some(true),
        })
    }

    pub(crate) fn validate(&self, path: &Path) -> Result<(), RebornConfigFileError> {
        let invalid = |field: &str, reason: &str| RebornConfigFileError::InvalidField {
            path: path.display().to_string(),
            field: format!("remote.{field}"),
            reason: reason.to_string(),
        };
        if self.identity_port == Some(0) {
            return Err(invalid("identity_port", "must be a fixed port, not 0"));
        }
        if self.https_port == Some(0) {
            return Err(invalid("https_port", "must be greater than 0"));
        }
        for login in self.owner_logins.iter().flatten() {
            if login.trim().is_empty()
                || login.trim() != login
                || login.chars().any(char::is_control)
            {
                return Err(invalid(
                    "owner_logins",
                    "entries must be non-empty tailnet logins without surrounding whitespace",
                ));
            }
        }
        if let Some(url) = &self.tailnet_url
            && (!url.starts_with("https://") || url.contains(char::is_whitespace))
        {
            return Err(invalid("tailnet_url", "must be an https:// URL"));
        }
        if self.enabled == Some(true) && self.tailnet_url.is_none() {
            return Err(invalid(
                "tailnet_url",
                "is required when remote access is enabled (run `ironclaw remote enable`)",
            ));
        }
        Ok(())
    }
}

/// Replace the `[remote]` table in `path` with `section`, keeping every
/// other section and its comments. The result is re-parsed and validated
/// before the atomic write, so a bad section never lands on disk.
pub fn write_remote_section(
    path: &Path,
    section: &RemoteSection,
) -> Result<(), RebornConfigFileUpdateError> {
    let _lock = acquire_update_lock(path)?;
    let mut doc = load_edit_document(path)?;
    let mut table = toml_edit::Table::new();
    if let Some(enabled) = section.enabled {
        table.insert("enabled", toml_edit::value(enabled));
    }
    if let Some(port) = section.identity_port {
        table.insert("identity_port", toml_edit::value(i64::from(port)));
    }
    if let Some(logins) = &section.owner_logins {
        let mut array = toml_edit::Array::new();
        for login in logins {
            array.push(login.as_str());
        }
        table.insert("owner_logins", toml_edit::value(array));
    }
    if let Some(trust) = section.trust_local_users {
        table.insert("trust_local_users", toml_edit::value(trust));
    }
    if let Some(url) = &section.tailnet_url {
        table.insert("tailnet_url", toml_edit::value(url.as_str()));
    }
    if let Some(port) = section.https_port {
        table.insert("https_port", toml_edit::value(i64::from(port)));
    }
    doc.as_table_mut()
        .insert("remote", toml_edit::Item::Table(table));
    write_edit_document(path, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RebornConfigFile;

    fn parse(text: &str) -> Result<RebornConfigFile, RebornConfigFileError> {
        RebornConfigFile::parse_text(text, Path::new("/cfg/config.toml"))
    }

    #[test]
    fn enabled_section_resolves_with_defaults() {
        let file = parse(
            r#"
[remote]
enabled = true
owner_logins = ["owner@example.com"]
tailnet_url = "https://studio.tail9915dd.ts.net"
"#,
        )
        .expect("parses");
        let settings = file
            .remote
            .as_ref()
            .and_then(RemoteSection::resolve)
            .expect("enabled");
        assert_eq!(settings.identity_port, DEFAULT_REMOTE_IDENTITY_PORT);
        assert_eq!(settings.https_port, DEFAULT_REMOTE_HTTPS_PORT);
        assert!(settings.one_click(false));
    }

    #[test]
    fn other_local_users_turn_one_click_off_unless_trusted() {
        let mut section = RemoteSection {
            enabled: Some(true),
            owner_logins: Some(vec!["owner@example.com".into()]),
            tailnet_url: Some("https://a.ts.net".into()),
            ..Default::default()
        };
        let settings = section.resolve().expect("enabled");
        assert!(settings.one_click(false));
        assert!(!settings.one_click(true));
        section.trust_local_users = Some(true);
        assert!(section.resolve().expect("enabled").one_click(true));
        section.owner_logins = Some(Vec::new());
        assert!(
            !section.resolve().expect("enabled").one_click(false),
            "no owners, nobody to click"
        );
        section.enabled = Some(false);
        assert!(section.resolve().is_none());
    }

    #[test]
    fn invalid_sections_are_rejected() {
        for (text, field) in [
            ("[remote]\nidentity_port = 0\n", "remote.identity_port"),
            ("[remote]\nhttps_port = 0\n", "remote.https_port"),
            (
                "[remote]\ntailnet_url = \"http://a.ts.net\"\n",
                "remote.tailnet_url",
            ),
            ("[remote]\nenabled = true\n", "remote.tailnet_url"),
            (
                "[remote]\nowner_logins = [\" a@b.c\"]\n",
                "remote.owner_logins",
            ),
        ] {
            match parse(text) {
                Err(RebornConfigFileError::InvalidField { field: got, .. }) => {
                    assert_eq!(got, field, "{text}")
                }
                other => panic!("{text}: expected InvalidField, got {other:?}"),
            }
        }
        assert!(
            parse("[remote]\nfunnel = true\n").is_err(),
            "unknown keys rejected"
        );
    }

    #[test]
    fn write_keeps_other_sections_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# operator note\n[webui]\nlisten_port = 3000\n").expect("seed");
        let section = RemoteSection {
            enabled: Some(true),
            identity_port: Some(3001),
            owner_logins: Some(vec!["owner@example.com".into()]),
            trust_local_users: Some(false),
            tailnet_url: Some("https://studio.tail9915dd.ts.net".into()),
            https_port: Some(443),
        };
        write_remote_section(&path, &section).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# operator note"), "{text}");
        let file = RebornConfigFile::load(&path)
            .expect("load")
            .expect("present");
        assert_eq!(file.remote.as_ref(), Some(&section));
        assert_eq!(file.webui.as_ref().and_then(|w| w.listen_port), Some(3000));

        let disabled = RemoteSection {
            enabled: Some(false),
            ..section
        };
        write_remote_section(&path, &disabled).expect("disable");
        let file = RebornConfigFile::load(&path)
            .expect("load")
            .expect("present");
        assert_eq!(file.remote.and_then(|r| r.enabled), Some(false));
    }
}
