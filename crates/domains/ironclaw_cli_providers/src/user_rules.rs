//! Where the person's own rules and skills for each coding-agent CLI live
//! on this computer: the files a CLI reads by itself, outside ZLO's
//! stores. Configuration sync carries exactly these between the person's
//! computers, so an agent started on another one follows the same rules.
//!
//! Only instructions and skills are listed. A CLI's login, tokens,
//! settings, MCP configuration, history and transcripts are never named
//! here, so they can never be carried.

use std::path::{Path, PathBuf};

/// One CLI's folder and the entries in it (files or folders) that are the
/// person's rules and skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRulesRoot {
    /// Stable name of the root, the same on every computer whatever the
    /// folder is called there.
    pub id: &'static str,
    pub dir: PathBuf,
    pub entries: &'static [&'static str],
}

/// The Claude CLI's default folder: global instructions, skills, custom
/// commands, subagent definitions and output styles.
const CLAUDE_ENTRIES: [&str; 5] = ["CLAUDE.md", "skills", "commands", "agents", "output-styles"];
/// The Codex CLI's folder: global instructions, skills and saved prompts.
const CODEX_ENTRIES: [&str; 3] = ["AGENTS.md", "skills", "prompts"];

/// Every root for the person whose home is `home`.
pub fn roots(home: &Path) -> Vec<UserRulesRoot> {
    vec![
        UserRulesRoot {
            id: "claude",
            dir: home.join(".claude"),
            entries: &CLAUDE_ENTRIES,
        },
        UserRulesRoot {
            id: "codex",
            dir: crate::codex::codex_home(home),
            entries: &CODEX_ENTRIES,
        },
        UserRulesRoot {
            id: "gemini",
            dir: home.join(".gemini"),
            entries: &crate::antigravity::launch::USER_CONFIG_ENTRIES,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_root_names_a_login_settings_or_history_file() {
        let forbidden = [
            "settings",
            "auth",
            "token",
            "credential",
            "oauth",
            "config.toml",
            ".json",
            "history",
            "projects",
            "sessions",
            "mcp",
        ];
        for root in roots(Path::new("/home/a")) {
            for entry in root.entries {
                let lower = entry.to_ascii_lowercase();
                for word in forbidden {
                    // The one listed JSON file is the skills index.
                    if *entry == "config/skills.json" {
                        continue;
                    }
                    assert!(!lower.contains(word), "{}: {entry}", root.id);
                }
            }
        }
    }
}
