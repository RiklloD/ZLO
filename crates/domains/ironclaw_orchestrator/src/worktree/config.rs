//! The project's worktree setup: `.zlo/worktree.toml` at the repository
//! top, e.g.
//!
//! ```toml
//! on_create = ["npm install"]   # after the worktree is made, before the child starts
//! on_settle = "npm test"        # when the child's task ends, before the clean check
//! timeout_s = 600               # per command
//! ```
//!
//! Each command is one `sh -c` line run in the worktree through the host
//! process runtime. A missing file means no setup.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::OrchestratorError;

/// Repository-relative location of the setup file.
pub(crate) const CONFIG_FILE: &str = ".zlo/worktree.toml";
const DEFAULT_TIMEOUT_S: u64 = 600;
const MAX_COMMANDS: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorktreeConfig {
    pub on_create: Vec<String>,
    pub on_settle: Vec<String>,
    pub timeout: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Commands {
    One(String),
    Many(Vec<String>),
}

impl Commands {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(line) => vec![line],
            Self::Many(lines) => lines,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    on_create: Option<Commands>,
    on_settle: Option<Commands>,
    timeout_s: Option<u64>,
}

fn clean(commands: Option<Commands>) -> Result<Vec<String>, OrchestratorError> {
    let lines: Vec<String> = commands
        .map(Commands::into_vec)
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > MAX_COMMANDS {
        return Err(OrchestratorError::invalid(format!(
            "{CONFIG_FILE}: at most {MAX_COMMANDS} commands per hook"
        )));
    }
    Ok(lines)
}

impl WorktreeConfig {
    pub(crate) fn parse(text: &str) -> Result<Self, OrchestratorError> {
        let raw: RawConfig = toml::from_str(text)
            .map_err(|e| OrchestratorError::invalid(format!("{CONFIG_FILE}: {e}")))?;
        Ok(Self {
            on_create: clean(raw.on_create)?,
            on_settle: clean(raw.on_settle)?,
            timeout: Duration::from_secs(
                raw.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).clamp(10, 3600),
            ),
        })
    }

    /// The setup of the first folder in `dirs` that has the file.
    pub(crate) async fn load(dirs: &[&Path]) -> Result<Self, OrchestratorError> {
        for dir in dirs {
            let path = dir.join(CONFIG_FILE);
            match tokio::fs::read_to_string(&path).await {
                Ok(text) => return Self::parse(&text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(OrchestratorError::request(
                        "worktree_setup_failed",
                        format!("could not read {CONFIG_FILE}: {e}"),
                    ));
                }
            }
        }
        Ok(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_line_or_a_list_and_bounds_the_timeout() {
        let c = WorktreeConfig::parse(
            "on_create = \"npm i\"\non_settle = [\"a\", \" \", \"b\"]\ntimeout_s = 1",
        )
        .unwrap();
        assert_eq!(c.on_create, vec!["npm i"]);
        assert_eq!(c.on_settle, vec!["a", "b"]);
        assert_eq!(c.timeout, Duration::from_secs(10));
        assert!(WorktreeConfig::parse("on_start = 1").is_err());
    }
}
