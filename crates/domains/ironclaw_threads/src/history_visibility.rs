//! The visible part of a user-kind transcript message for history search.
//!
//! What the engine sends on its own (delegation results, context handoffs,
//! the restart continue prompt) is identified by *provenance*, not by text:
//! the orchestrator records the runs it starts with its own text, and the
//! search request carries them as excluded runs (see
//! `SearchThreadMessagesRequest::excluded_runs`). Text a person types is
//! never hidden here because of how it begins.
//!
//! The one textual rule left mirrors the orchestrator's own format for
//! context it *prepends* to a person's message
//! (`ironclaw_orchestrator::handoff::strip_context`): a message that starts
//! with a carried-context header *and* contains that context's closing
//! marker shows only the text after the marker. Without the closing marker
//! the message is the person's, whole.

/// Headers of context the engine prepends to a person's message.
pub const CARRIED_CONTEXT_PREFIXES: [&str; 2] = ["Context handoff (", "Merged context from fork"];

/// Closing markers of carried context.
pub const CARRIED_CONTEXT_ENDS: [&str; 2] =
    ["[End of context handoff]", "[End of merged fork context]"];

/// The text the person typed in a user-kind message: the message itself, or
/// what follows prepended context. `None` when the message was only
/// prepended context.
pub fn visible_user_text(text: &str) -> Option<&str> {
    if !CARRIED_CONTEXT_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return Some(text);
    }
    let Some(at) = CARRIED_CONTEXT_ENDS
        .iter()
        .filter_map(|end| text.rfind(end).map(|at| at + end.len()))
        .max()
    else {
        // No closing marker: not the engine's format; the person's text.
        return Some(text);
    };
    let typed = text[at..]
        .trim_start_matches('\n')
        .trim_start_matches("---")
        .trim();
    (!typed.is_empty()).then_some(typed)
}

#[cfg(test)]
mod tests {
    use super::visible_user_text;

    #[test]
    fn only_closed_carried_context_is_stripped() {
        assert_eq!(
            visible_user_text("how do I deploy?"),
            Some("how do I deploy?")
        );
        // What the person types is never hidden by how it begins.
        for typed in [
            "Continue where you left off. Then explain the failure.",
            "Continue where you left off.",
            "Delegated work finished. Use these results to continue.",
            "Context handoff (my notes) for the team",
        ] {
            assert_eq!(visible_user_text(typed), Some(typed));
        }
        let carried = "Context handoff (fork). Source thread: \"t\" (id).\n\nold stuff\n\n\
                       [End of context handoff]\n\n---\n\nwhat about zebras?";
        assert_eq!(visible_user_text(carried), Some("what about zebras?"));
        let only_context =
            "Merged context from fork \"f\" (id).\n\nbody\n\n[End of merged fork context]";
        assert_eq!(visible_user_text(only_context), None);
    }
}
