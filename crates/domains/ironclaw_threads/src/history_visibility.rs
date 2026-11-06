//! Which user-kind transcript text the person actually typed.
//!
//! The orchestrator writes its own scaffolding into threads as user-kind
//! messages: context handoffs and merged fork context (optionally followed by
//! the person's typed text after a `---` rule), delegation completions, and the
//! restart continue prompt. Transcript readers hide it from the person and
//! history search must never index it. This module is the one place that
//! recognizes it; the orchestrator's agent-loop transcript reader
//! (`ironclaw_assistant::orchestration::runtime`) and the search projection
//! both call [`visible_user_text`]. `ironclaw_assistant` pins the markers
//! against the orchestrator's own generators in a test, because the
//! orchestrator cannot name this crate.

/// Prefixes of whole messages the engine sends on its own.
pub const ENGINE_SCAFFOLDING_PREFIXES: [&str; 2] = [
    "Delegated work finished.",
    // `ironclaw_orchestrator::restart::CONTINUE_PROMPT`
    "Continue where you left off.",
];

/// Prefixes of context the engine prepends to a message; the person's own
/// text, if any, follows the closing marker.
pub const CARRIED_CONTEXT_PREFIXES: [&str; 2] = ["Context handoff (", "Merged context from fork"];

/// Closing markers of carried context.
pub const CARRIED_CONTEXT_ENDS: [&str; 2] =
    ["[End of context handoff]", "[End of merged fork context]"];

/// The text the person typed in a user-kind message, or `None` when the
/// message is engine scaffolding with nothing typed.
pub fn visible_user_text(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    if ENGINE_SCAFFOLDING_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return None;
    }
    if !CARRIED_CONTEXT_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return Some(text);
    }
    let typed = CARRIED_CONTEXT_ENDS
        .iter()
        .filter_map(|end| trimmed.rfind(end).map(|at| at + end.len()))
        .max()
        .map(|at| {
            trimmed[at..]
                .trim_start_matches('\n')
                .trim_start_matches("---")
                .trim()
        })?;
    (!typed.is_empty()).then_some(typed)
}

#[cfg(test)]
mod tests {
    use super::visible_user_text;

    #[test]
    fn scaffolding_is_hidden_and_typed_text_after_context_survives() {
        assert_eq!(
            visible_user_text("how do I deploy?"),
            Some("how do I deploy?")
        );
        assert_eq!(
            visible_user_text("Delegated work finished. Use these results to continue.\n\nx"),
            None
        );
        assert_eq!(visible_user_text("Continue where you left off."), None);
        let carried = "Context handoff (fork). Source thread: \"t\" (id).\n\nold stuff\n\n\
                       [End of context handoff]\n\n---\n\nwhat about zebras?";
        assert_eq!(visible_user_text(carried), Some("what about zebras?"));
        let only_context =
            "Merged context from fork \"f\" (id).\n\nbody\n\n[End of merged fork context]";
        assert_eq!(visible_user_text(only_context), None);
        // An unterminated handoff shows nothing rather than the context.
        assert_eq!(visible_user_text("Context handoff (x) no end marker"), None);
    }
}
