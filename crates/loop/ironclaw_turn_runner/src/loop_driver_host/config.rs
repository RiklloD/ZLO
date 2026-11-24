use std::{error::Error, fmt};

use ironclaw_loop_contracts::{LoopRunContext, PromptContextTokenBudget};
use ironclaw_turns::runner::ClaimedTurnRun;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextOnlyLoopHostConfig {
    pub max_messages: usize,
    pub prompt_context_budget: PromptContextTokenBudget,
    pub require_model_route_snapshot: bool,
}

/// Production text-only history window. Matches the agent loop's
/// `DefaultContextStrategy::DEFAULT_MAX_MESSAGES` (512) so the host never
/// truncates a request the strategy asked for in full; the turn kernel still
/// clamps every request to `MAX_TEXT_ONLY_MESSAGE_LIMIT` (1024).
const DEFAULT_TEXT_ONLY_MAX_MESSAGES: usize = 512;

impl Default for TextOnlyLoopHostConfig {
    fn default() -> Self {
        Self {
            max_messages: DEFAULT_TEXT_ONLY_MAX_MESSAGES,
            prompt_context_budget: PromptContextTokenBudget::default(),
            require_model_route_snapshot: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebornLoopDriverHostError {
    ScopeMismatch { reason: String },
    InvalidRequest { reason: String },
}

impl fmt::Display for RebornLoopDriverHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScopeMismatch { reason } => {
                write!(formatter, "loop driver host scope mismatch: {reason}")
            }
            Self::InvalidRequest { reason } => {
                write!(formatter, "invalid loop driver host request: {reason}")
            }
        }
    }
}

impl Error for RebornLoopDriverHostError {}

#[derive(Debug, Clone)]
pub struct RebornLoopDriverHostRequest {
    pub claimed_run: ClaimedTurnRun,
    pub loop_run_context: LoopRunContext,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production default must not be narrower than what the default
    /// context strategy requests (512), or long-running turns silently lose
    /// history the strategy budgeted for.
    #[test]
    fn default_text_only_window_matches_the_context_strategy_request() {
        assert_eq!(TextOnlyLoopHostConfig::default().max_messages, 512);
        assert_eq!(DEFAULT_TEXT_ONLY_MAX_MESSAGES, 512);
    }
}
