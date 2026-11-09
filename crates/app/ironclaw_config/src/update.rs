//! `[update]` — how this installation updates itself from a source checkout
//! (`zlo update`, `POST /api/operator/update`).
//!
//! ```toml
//! [update]
//! method = "source"                  # "source" | "none" (default "none")
//! repo = "/Users/me/ironclaw"        # the checkout this binary is built from
//! branch = "main"                    # optional; default: the checkout's branch
//! ```
//!
//! Everything an update runs comes from here: the HTTP routes carry no
//! command, path or branch. This module only parses and validates; git, the
//! build and the install run in the binary.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config_file::{
    RebornConfigFileError, RebornConfigFileUpdateError, acquire_update_lock, load_edit_document,
    write_edit_document,
};

/// `[update] method`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMethodSetting {
    /// Fast-forward `repo`, rebuild, reinstall.
    Source,
    /// Updates are refused. The default.
    #[default]
    None,
}

/// `[update]` as written in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSection {
    pub method: Option<UpdateMethodSetting>,
    /// Absolute path of the source checkout. Required for `source`.
    pub repo: Option<PathBuf>,
    /// Branch to follow. Default: the branch the checkout has checked out.
    pub branch: Option<String>,
}

/// `[update]` for `method = "source"`, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUpdateSettings {
    pub repo: PathBuf,
    pub branch: Option<String>,
}

impl UpdateSection {
    /// `Some` only for `method = "source"` (validation already required
    /// `repo` then).
    pub fn source(&self) -> Option<SourceUpdateSettings> {
        if self.method != Some(UpdateMethodSetting::Source) {
            return None;
        }
        Some(SourceUpdateSettings {
            repo: self.repo.clone()?,
            branch: self.branch.clone(),
        })
    }

    pub(crate) fn validate(&self, path: &Path) -> Result<(), RebornConfigFileError> {
        let invalid = |field: &str, reason: &str| RebornConfigFileError::InvalidField {
            path: path.display().to_string(),
            field: format!("update.{field}"),
            reason: reason.to_string(),
        };
        if let Some(repo) = &self.repo
            && (!repo.is_absolute()
                || repo
                    .to_str()
                    .is_none_or(|s| s.chars().any(char::is_control)))
        {
            return Err(invalid("repo", "must be an absolute path"));
        }
        if self.method == Some(UpdateMethodSetting::Source) && self.repo.is_none() {
            return Err(invalid(
                "repo",
                "is required when method = \"source\" (the checkout this binary is built from)",
            ));
        }
        if let Some(branch) = &self.branch
            && !is_safe_branch_name(branch)
        {
            return Err(invalid(
                "branch",
                "must be a plain branch name (letters, digits, `.`, `_`, `-`, `/`)",
            ));
        }
        Ok(())
    }
}

/// Replace the `[update]` table in `path` with `section`, keeping every
/// other section and its comments. The result is re-parsed and validated
/// before the atomic write, so a bad section never lands on disk.
pub fn write_update_section(
    path: &Path,
    section: &UpdateSection,
) -> Result<(), RebornConfigFileUpdateError> {
    let _lock = acquire_update_lock(path)?;
    let mut doc = load_edit_document(path)?;
    let mut table = toml_edit::Table::new();
    if let Some(method) = section.method {
        let method = match method {
            UpdateMethodSetting::Source => "source",
            UpdateMethodSetting::None => "none",
        };
        table.insert("method", toml_edit::value(method));
    }
    if let Some(repo) = section.repo.as_deref().and_then(Path::to_str) {
        table.insert("repo", toml_edit::value(repo));
    }
    if let Some(branch) = &section.branch {
        table.insert("branch", toml_edit::value(branch.as_str()));
    }
    doc.as_table_mut()
        .insert("update", toml_edit::Item::Table(table));
    write_edit_document(path, &doc)
}

/// A branch or remote name safe to hand to git as one argument: ASCII
/// letters, digits, `.`, `_`, `-` and `/`, not starting with `-` or `/`,
/// with no `..`, `//`, trailing `/` or `.lock` suffix, at most 200 bytes.
/// Stricter than `git check-ref-format`; never an option or a revision
/// expression.
pub fn is_safe_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("/.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RebornConfigFile;

    fn parse(text: &str) -> Result<RebornConfigFile, RebornConfigFileError> {
        RebornConfigFile::parse_text(text, Path::new("/cfg/config.toml"))
    }

    #[test]
    fn missing_section_and_method_none_mean_no_updates() {
        assert!(parse("").expect("parses").update.is_none());
        let file =
            parse("[update]\nmethod = \"none\"\nrepo = \"/src/ironclaw\"\n").expect("parses");
        assert_eq!(file.update.as_ref().and_then(UpdateSection::source), None);
        let file = parse("[update]\nrepo = \"/src/ironclaw\"\n").expect("parses");
        assert_eq!(
            file.update.as_ref().and_then(UpdateSection::source),
            None,
            "method defaults to none"
        );
    }

    #[test]
    fn source_resolves_repo_and_optional_branch() {
        let file =
            parse("[update]\nmethod = \"source\"\nrepo = \"/src/ironclaw\"\n").expect("parses");
        assert_eq!(
            file.update.as_ref().and_then(UpdateSection::source),
            Some(SourceUpdateSettings {
                repo: PathBuf::from("/src/ironclaw"),
                branch: None
            })
        );
        let file = parse(
            "[update]\nmethod = \"source\"\nrepo = \"/src/ironclaw\"\nbranch = \"rune-port\"\n",
        )
        .expect("parses");
        assert_eq!(
            file.update
                .as_ref()
                .and_then(UpdateSection::source)
                .and_then(|s| s.branch),
            Some("rune-port".into())
        );
    }

    #[test]
    fn write_keeps_other_sections_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# operator note\n[webui]\nlisten_port = 3000\n").expect("seed");
        let section = UpdateSection {
            method: Some(UpdateMethodSetting::Source),
            repo: Some(PathBuf::from("/src/ironclaw")),
            branch: None,
        };
        write_update_section(&path, &section).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# operator note"), "{text}");
        let file = RebornConfigFile::load(&path)
            .expect("load")
            .expect("present");
        assert_eq!(file.update.as_ref(), Some(&section));
        assert_eq!(file.webui.as_ref().and_then(|w| w.listen_port), Some(3000));

        let off = UpdateSection {
            method: Some(UpdateMethodSetting::None),
            ..section
        };
        write_update_section(&path, &off).expect("turn off");
        let file = RebornConfigFile::load(&path)
            .expect("load")
            .expect("present");
        assert_eq!(file.update.as_ref().and_then(UpdateSection::source), None);
        assert_eq!(
            file.update.and_then(|u| u.repo),
            Some(PathBuf::from("/src/ironclaw")),
            "the checkout is remembered for the next enable"
        );

        // A relative checkout never lands on disk.
        let bad = UpdateSection {
            method: Some(UpdateMethodSetting::Source),
            repo: Some(PathBuf::from("relative")),
            branch: None,
        };
        assert!(write_update_section(&path, &bad).is_err());
    }

    #[test]
    fn invalid_sections_are_rejected() {
        for (text, field) in [
            ("[update]\nmethod = \"source\"\n", "update.repo"),
            (
                "[update]\nmethod = \"source\"\nrepo = \"relative/ironclaw\"\n",
                "update.repo",
            ),
            (
                "[update]\nmethod = \"source\"\nrepo = \"/r\"\nbranch = \"--upload-pack=x\"\n",
                "update.branch",
            ),
            (
                "[update]\nmethod = \"source\"\nrepo = \"/r\"\nbranch = \"main; rm -rf /\"\n",
                "update.branch",
            ),
            (
                "[update]\nmethod = \"source\"\nrepo = \"/r\"\nbranch = \"a..b\"\n",
                "update.branch",
            ),
        ] {
            match parse(text) {
                Err(RebornConfigFileError::InvalidField { field: got, .. }) => {
                    assert_eq!(got, field, "{text}")
                }
                other => panic!("{text}: expected InvalidField, got {other:?}"),
            }
        }
        assert!(parse("[update]\nmethod = \"binary\"\n").is_err());
        assert!(
            parse("[update]\nmethod = \"source\"\nrepo = \"/r\"\ncommand = \"make\"\n").is_err(),
            "unknown keys (a command) are rejected"
        );
    }

    #[test]
    fn branch_names_are_plain() {
        for ok in ["main", "rune-port", "feature/x_1.2", "v1"] {
            assert!(is_safe_branch_name(ok), "{ok}");
        }
        for bad in [
            "", "-x", "/x", "x/", ".x", "x.", "x.lock", "a..b", "a//b", "a/.b", "a b", "a~1", "a^",
            "a:b", "HEAD@{1}", "a\nb",
        ] {
            assert!(!is_safe_branch_name(bad), "{bad:?}");
        }
    }
}
