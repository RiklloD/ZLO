//! Restart continuation policy (T3's `RestartContinuation`).
//!
//! A backend restart cuts running turns. Intent is a `restart` recovery row
//! recorded from two sources: a runtime reporting a turn cut by the restart
//! (`TurnOutcome::cut_by_restart`, IronClaw's `RecoveryRequired`), and the
//! engine's own live-work rows a previous engine instance left behind (CLI
//! runtimes whose child processes died with it). Once the thread is idle the
//! engine sends "Continue where you left off.", unless one of the checks
//! below says the thread belongs to the user now.

/// Prompt every automatic continuation sends.
pub const CONTINUE_PROMPT: &str = "Continue where you left off.";
/// Consecutive automatic continuations allowed before a person steps in.
pub const MAX_CONTINUATION_CHAIN: u32 = 3;
/// Intent older than this is history, not a turn to resume.
pub const MAX_INTENT_AGE_MS: i64 = 6 * 3600 * 1000;
/// Usage-limit resumes re-arm at most this many times for one window.
pub const MAX_USAGE_LIMIT_ATTEMPTS: u32 = 3;

/// A slash command is maintenance, not work to continue.
pub fn is_maintenance_command(prompt: Option<&str>) -> bool {
    prompt.is_some_and(|p| p.trim_start().starts_with('/'))
}

/// Why a cut turn is not continued. Stored as the recovery detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Disabled,
    Superseded,
    Stopped,
    Maintenance,
    Exhausted,
    Expired,
    Interrupted,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Superseded => "superseded",
            Self::Stopped => "stopped",
            Self::Maintenance => "maintenance",
            Self::Exhausted => "exhausted",
            Self::Expired => "expired",
            Self::Interrupted => "interrupted by quit",
        }
    }
}
