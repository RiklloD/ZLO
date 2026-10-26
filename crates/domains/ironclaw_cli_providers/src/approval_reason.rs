//! "Why the agent asked": a short human reason shown with a CLI tool call.
//!
//! An engine's explicit justification wins (Claude's `description` on a Bash
//! call, Codex's `reason` on an approval request). Otherwise the bridge uses
//! what the model said since the previous call: its newest text paragraph,
//! else its newest thought. Either way the reason is one bounded line, with
//! credentials redacted before it is cut or shown.

use serde_json::Value;

/// Upper bound on a reason, in characters.
pub const REASON_MAX_CHARS: usize = 200;

/// The first non-empty string among `keys` of `value`, as a reason.
pub fn explicit(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).and_then(bounded))
}

/// The reason in what the model said: its last paragraph.
pub fn from_said(said: &str) -> Option<String> {
    said.trim()
        .rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .and_then(bounded)
}

/// One redacted line of at most [`REASON_MAX_CHARS`]; `None` when blank.
pub fn bounded(text: &str) -> Option<String> {
    let clean = ironclaw_safety::sanitize_display_text(text);
    let line = clean.split_whitespace().collect::<Vec<_>>().join(" ");
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_reason_wins_and_said_text_falls_back_to_its_last_paragraph() {
        let input = json!({"command": "ls", "description": "  List the repo  "});
        assert_eq!(
            explicit(&input, &["description"]).as_deref(),
            Some("List the repo")
        );
        assert_eq!(explicit(&json!({"command": "ls"}), &["description"]), None);
        // An empty first key does not hide a real one behind it.
        assert_eq!(
            explicit(
                &json!({"reason": " ", "justification": "Check the build"}),
                &["reason", "justification"]
            )
            .as_deref(),
            Some("Check the build")
        );
        assert_eq!(
            from_said("Looked around.\n\nNow I'll run\nthe tests.\n\n").as_deref(),
            Some("Now I'll run the tests.")
        );
        let long = "é".repeat(REASON_MAX_CHARS + 5);
        assert_eq!(
            bounded(&long).map(|r| r.chars().count()),
            Some(REASON_MAX_CHARS)
        );
        let exact = "a".repeat(REASON_MAX_CHARS);
        assert_eq!(bounded(&exact).as_deref(), Some(exact.as_str()));
    }
}
