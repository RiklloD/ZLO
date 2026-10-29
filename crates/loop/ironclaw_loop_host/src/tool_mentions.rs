//! `@extension` mentions in a user message.
//!
//! Writing `@tracker` asks for that extension's tools on this turn. Product
//! ingress records the mentions when it accepts the message; the tool
//! disclosure port takes them once when it builds the turn's surface and
//! advertises the mentioned extension's tools with full schemas, the same
//! way a profile pin does. A mention only reorders what the turn is already
//! authorized to call: pins are applied to the authorized definitions, so an
//! unknown or denied extension pins nothing.

use std::collections::HashMap;
use std::sync::Mutex;

use ironclaw_host_api::ids::CapabilityId;
use ironclaw_loop_contracts::{LoopRunContext, ProviderToolDefinition};
use ironclaw_turns::{AcceptedMessageRef, TurnScope};

/// Upper bound on tools one turn's mentions can pin, so `@` on a very wide
/// extension cannot blow the advertised-schema budget.
const MAX_MENTION_PINS: usize = 32;

/// Mentions per accepted message, waiting for their turn to build a surface.
#[derive(Debug, Default)]
pub struct TurnToolMentions {
    by_message: Mutex<HashMap<(TurnScope, AcceptedMessageRef), Vec<String>>>,
}

impl TurnToolMentions {
    /// Record the `@name` mentions in `message`. Messages without one are
    /// not stored.
    pub fn record(
        &self,
        scope: &TurnScope,
        accepted_message_ref: &AcceptedMessageRef,
        message: &str,
    ) {
        let names = extract_tool_mentions(message);
        if names.is_empty() {
            return;
        }
        if let Ok(mut map) = self.by_message.lock() {
            map.insert((scope.clone(), accepted_message_ref.clone()), names);
        }
    }

    /// Take the run's mentions and resolve them against the authorized
    /// definitions: every tool whose capability id is `<name>.<tool>`.
    pub(crate) fn take_pins(
        &self,
        run_context: &LoopRunContext,
        authorized: &[ProviderToolDefinition],
    ) -> Vec<CapabilityId> {
        let Some(accepted_message_ref) = run_context.accepted_message_ref.as_ref() else {
            return Vec::new();
        };
        let names = match self.by_message.lock() {
            Ok(mut map) => map.remove(&(run_context.scope.clone(), accepted_message_ref.clone())),
            Err(_) => None, // silent-ok: a poisoned map only loses a visibility hint
        };
        let Some(names) = names else {
            return Vec::new();
        };
        authorized
            .iter()
            .map(|definition| &definition.capability_id)
            .filter(|id| {
                names.iter().any(|name| {
                    id.as_str()
                        .strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
                })
            })
            .take(MAX_MENTION_PINS)
            .cloned()
            .collect()
    }
}

/// Lowercased `@name` tokens that start a word, deduplicated in order.
/// Names use the extension id alphabet: `[a-z0-9._-]`.
fn extract_tool_mentions(message: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut previous = ' ';
    let mut chars = message.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let starts_word = previous.is_whitespace() || matches!(previous, '(' | '[' | '"');
        previous = c;
        if c != '@' || !starts_word {
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while let Some(&(i, next)) = chars.peek() {
            if !(next.is_ascii_alphanumeric() || matches!(next, '-' | '_' | '.')) {
                break;
            }
            end = i + next.len_utf8();
            previous = next;
            chars.next();
        }
        // A trailing dot is sentence punctuation, not part of the name.
        let name = message[start..end]
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::extract_tool_mentions;

    #[test]
    fn mentions_start_a_word_and_drop_trailing_punctuation() {
        assert_eq!(
            extract_tool_mentions("look in @Tracker and @web-search. mail me@x.com (@notes)"),
            ["tracker", "web-search", "notes"]
        );
        assert!(extract_tool_mentions("no mentions @ here").is_empty());
    }
}
