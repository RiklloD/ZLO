//! "Why the agent asked": the reason a tool call or approval row shows.
//!
//! A gate's own `reason` wins (the gateway sets it when the engine gave a
//! justification). Otherwise the row borrows what the model said last in the
//! same run before the call: its newest text or thought paragraph, one line,
//! at most [`REASON_MAX_CHARS`]. That is also how a call that ran without
//! asking (auto-approved) shows why it ran.

/// Upper bound on a shown reason, in characters (matches the gateway's).
pub const REASON_MAX_CHARS: usize = 200;

/// The last paragraph of `said` as one bounded line; `None` when blank.
pub(super) fn from_said(said: &str) -> Option<String> {
    said.trim()
        .rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .and_then(bounded)
}

/// `text` as one line of at most [`REASON_MAX_CHARS`]; `None` when blank.
pub(super) fn bounded(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    Some(match line.char_indices().nth(REASON_MAX_CHARS - 1) {
        Some((at, _)) if line.chars().count() > REASON_MAX_CHARS => {
            format!("{}…", line[..at].trim_end())
        }
        _ => line,
    })
}
