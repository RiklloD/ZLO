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
    pub entries: &'static [UserRulesEntry],
}

/// One entry of a root: exactly one file, or a folder and what is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserRulesEntry {
    File(&'static str),
    Folder(&'static str),
}

impl UserRulesEntry {
    pub fn path(self) -> &'static str {
        match self {
            Self::File(path) | Self::Folder(path) => path,
        }
    }
}

use UserRulesEntry::{File, Folder};

/// The Claude CLI's default folder: global instructions, skills, custom
/// commands, subagent definitions and output styles.
const CLAUDE_ENTRIES: [UserRulesEntry; 5] = [
    File("CLAUDE.md"),
    Folder("skills"),
    Folder("commands"),
    Folder("agents"),
    Folder("output-styles"),
];
/// The Codex CLI's folder: global instructions, skills and saved prompts.
const CODEX_ENTRIES: [UserRulesEntry; 3] = [File("AGENTS.md"), Folder("skills"), Folder("prompts")];
/// What the Antigravity agent reads from the person's own Gemini folder
/// (the entries its private profile links to, `antigravity::launch`).
const GEMINI_ENTRIES: [UserRulesEntry; 5] = [
    Folder("config/skills"),
    Folder("antigravity-cli/skills"),
    File("config/skills.json"),
    File("AGENTS.md"),
    File("GEMINI.md"),
];

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
            entries: &GEMINI_ENTRIES,
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
            for entry in root.entries.iter().map(|entry| entry.path()) {
                let lower = entry.to_ascii_lowercase();
                for word in forbidden {
                    // The one listed JSON file is the skills index.
                    if entry == "config/skills.json" {
                        continue;
                    }
                    assert!(!lower.contains(word), "{}: {entry}", root.id);
                }
            }
        }
    }

    #[test]
    fn the_gemini_entries_are_the_ones_the_agents_profile_links() {
        let mut listed: Vec<&str> = GEMINI_ENTRIES.iter().map(|entry| entry.path()).collect();
        let mut linked = crate::antigravity::launch::USER_CONFIG_ENTRIES.to_vec();
        listed.sort_unstable();
        linked.sort_unstable();
        assert_eq!(listed, linked);
    }
}
