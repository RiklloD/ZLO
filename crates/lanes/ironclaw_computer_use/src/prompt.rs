//! The system-prompt section: the server's instructions with its two
//! Codex-specific sentences made true for IronClaw, the tool-name mapping,
//! and the patched fork's guidance when the fork is installed (the Hermes
//! plugin's `codex.instructions()`).

use crate::catalog::server_instructions;

const FORK_GUIDANCE: &str = include_str!("../prompts/fork_guidance.md");
const TOOL_NAMES: &str = include_str!("../prompts/tool_names.md");

const CODEX_STOPS: &str = "Codex will automatically stop the session after each assistant turn";
const IRONCLAW_STOPS: &str = "IronClaw ends the computer use session after each assistant turn";
const TOOL_SEARCH_HINT: &str =
    " If any of these are not available in your environment, use tool_search";

pub(crate) fn instructions(fork: bool) -> String {
    let mut text = server_instructions().replace(CODEX_STOPS, IRONCLAW_STOPS);
    // The tools are always offered up front; drop the sentence up to its period.
    if let Some(start) = text.find(TOOL_SEARCH_HINT) {
        let end = text[start + 1..]
            .find('.')
            .map_or(text.len(), |offset| start + 1 + offset + 1);
        text.replace_range(start..end, "");
    }
    text.push_str("\n\n");
    text.push_str(TOOL_NAMES.trim_end());
    if fork {
        text.push_str("\n\n");
        text.push_str(FORK_GUIDANCE.trim_end());
    }
    text
}
