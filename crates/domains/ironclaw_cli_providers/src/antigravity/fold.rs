//! Folding one prompt's ACP `session/update` stream and its `session/prompt`
//! answer into the turn's outcome, transcript entries and telemetry. Pure:
//! the process side feeds updates and timestamps in.

use std::time::Instant;

use ironclaw_orchestrator::runtime::{HistoryRole, TurnEndStatus, TurnOutcome, TurnUsage};
use ironclaw_orchestrator::telemetry::{TurnTelemetry, tokens_per_second};
use serde_json::Value;

use ironclaw_orchestrator::model::now_ms;

use crate::journal;
use crate::rpc::RpcError;

/// `entries` as this turn's run journals them (as Codex and Muse do): the
/// parent transcript anchors turns and their subagents by run.
pub(crate) fn stamped(
    run_id: &str,
    entries: impl IntoIterator<Item = ironclaw_orchestrator::runtime::HistoryMessage>,
) -> Vec<ironclaw_orchestrator::runtime::HistoryMessage> {
    entries
        .into_iter()
        .map(|mut entry| {
            entry.run_id = Some(run_id.to_string());
            entry
        })
        .collect()
}

/// The marker a native `/goal` run leaves at the end of its reply.
const GOAL_COMPLETE_MARKER: &str = "<!-- GOAL_COMPLETE -->";

/// What the model says about a thinking block whose signature was lost.
pub(crate) const INVALID_THINKING_SIGNATURE: [&str; 2] = [
    ".thinking.signature: Field required",
    "Invalid `signature` in `thinking` block",
];

#[derive(Debug, Default)]
pub(crate) struct PromptFold {
    /// Assistant text after the last tool call: the turn's answer.
    answer: String,
    tools: Vec<String>,
    first_chunk: Option<Instant>,
    last_chunk: Option<Instant>,
    /// Latest ACP `usage_update`: tokens in context, window size.
    context: Option<(u64, u64)>,
    pub(crate) model: Option<String>,
}

impl PromptFold {
    /// One `session/update` notification's `update` object.
    pub(crate) fn push_update(&mut self, update: &Value, now: Instant) {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => {
                if let Some(text) = update["content"].get("text").and_then(Value::as_str) {
                    self.answer.push_str(text);
                    self.mark_chunk(now);
                }
            }
            Some("agent_thought_chunk") => self.mark_chunk(now),
            Some("tool_call") => {
                let title = ["name", "kind", "title"]
                    .iter()
                    .find_map(|k| update.get(*k).and_then(Value::as_str))
                    .unwrap_or("tool");
                self.tools.push(format!("{title}()"));
                // The answer is the text after the last tool call.
                self.answer.clear();
            }
            Some("usage_update") => {
                let n = |k: &str| update.get(k).and_then(Value::as_u64);
                if let (Some(used), Some(size)) = (n("used"), n("size")) {
                    self.context = Some((used, size));
                }
            }
            _ => {}
        }
    }

    fn mark_chunk(&mut self, now: Instant) {
        self.first_chunk.get_or_insert(now);
        self.last_chunk = Some(now);
    }

    /// The turn's assistant transcript entry.
    pub(crate) fn assistant_message(
        &self,
    ) -> Option<ironclaw_orchestrator::runtime::HistoryMessage> {
        let text = self.answer_text();
        (!text.is_empty() || !self.tools.is_empty())
            .then(|| journal::message(HistoryRole::Assistant, text, self.tools.clone()))
    }

    /// What the agent said before a steer, as its own transcript entry; the
    /// answer starts over under the steer.
    pub(crate) fn take_partial(
        &mut self,
    ) -> Option<ironclaw_orchestrator::runtime::HistoryMessage> {
        let entry = self.assistant_message();
        self.answer.clear();
        self.tools.clear();
        entry
    }

    /// The answer without the marker a native `/goal` run ends with.
    pub(crate) fn answer_text(&self) -> String {
        self.answer
            .replace(GOAL_COMPLETE_MARKER, "")
            .trim()
            .to_string()
    }

    /// The model rejected a thinking block saved in the agent's session, so
    /// every later prompt to that session fails the same way.
    pub(crate) fn lost_thinking_signature(&self, answer: &Result<Value, RpcError>) -> bool {
        let error = answer.as_ref().err().map(RpcError::message).unwrap_or("");
        [error, self.answer.as_str()].iter().any(|text| {
            INVALID_THINKING_SIGNATURE
                .iter()
                .any(|needle| text.contains(needle))
        })
    }

    /// `session/prompt`'s optional `usage` (ACP prompt usage), Claude-style:
    /// `input_tokens` excludes the cache read bucket.
    fn usage(&self, result: &Value) -> Option<TurnUsage> {
        let u = result.get("usage")?;
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let read = n("cachedReadTokens");
        Some(TurnUsage {
            input_tokens: n("inputTokens").saturating_sub(read),
            output_tokens: n("outputTokens"),
            cache_read_tokens: read,
            cache_creation_tokens: n("cachedWriteTokens"),
            context_window: self.context.map(|(_, size)| size),
        })
    }

    /// Context from the latest `usage_update`; cache rate from the prompt
    /// usage; speed only for tool-free turns (tool gaps would read as slow
    /// generation), and only when the agent reported output tokens.
    pub(crate) fn telemetry(
        &self,
        provider: &str,
        result: Option<&Value>,
    ) -> Option<TurnTelemetry> {
        let usage = result.and_then(|r| self.usage(r));
        if self.context.is_none() && usage.is_none() {
            return None;
        }
        let mut t = match usage {
            Some(u) => TurnTelemetry::from_prompt_tokens(
                u.input_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
            ),
            None => TurnTelemetry::default(),
        };
        if let Some((used, size)) = self.context {
            t.context_used = Some(used);
            t.context_max = (size > 0).then_some(size);
        }
        if let (Some(u), Some(first), Some(last), true) = (
            usage,
            self.first_chunk,
            self.last_chunk,
            self.tools.is_empty(),
        ) {
            t.tps = tokens_per_second(
                u.output_tokens,
                last.duration_since(first).as_millis() as u64,
            );
        }
        t.provider = Some(provider.to_string());
        t.model = self.model.clone();
        Some(t)
    }

    /// How the turn ended, from the `session/prompt` answer.
    pub(crate) fn outcome(&self, cancelled: bool, answer: &Result<Value, RpcError>) -> TurnOutcome {
        if cancelled {
            return TurnOutcome::ended(TurnEndStatus::Cancelled, None);
        }
        match answer {
            Ok(result) => {
                let usage = self.usage(result);
                match result.get("stopReason").and_then(Value::as_str) {
                    Some("end_turn") => TurnOutcome {
                        usage,
                        ..TurnOutcome::completed(Some(self.answer_text()))
                    },
                    Some("cancelled") => TurnOutcome::ended(TurnEndStatus::Cancelled, None),
                    // The agent maps only a quota-exhausted model stop here.
                    Some("refusal") => TurnOutcome {
                        usage,
                        ..usage_limited(None)
                    },
                    other => TurnOutcome {
                        usage,
                        ..TurnOutcome::ended(
                            TurnEndStatus::Failed,
                            Some(format!(
                                "antigravity stopped: {}",
                                other.unwrap_or("unknown")
                            )),
                        )
                    },
                }
            }
            Err(error) => failed(error),
        }
    }
}

/// What a turn that needs an account sign-in says. agy 1.5 keeps the sign-in
/// in the OS keyring, so signing in once with the CLI serves ZLO too.
pub(crate) const SIGN_IN_REQUIRED: &str =
    "antigravity: sign-in required (run `agy` in a terminal and sign in)";

/// A failed `authenticate` as the turn's failure, and whether it shows the
/// account signed out. A crash is named as one, not blamed on the sign-in
/// (t3code #28355); a quota stays a quota.
pub(crate) fn sign_in_failed(error: &RpcError, uses_browser: bool) -> (TurnOutcome, bool) {
    let text = error.message();
    match error {
        RpcError::Closed => (
            TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some("antigravity: the agent exited during sign-in; send the message again".into()),
            ),
            false,
        ),
        RpcError::Timeout if !uses_browser => (failed(error), false),
        _ if text.contains("SUBSCRIPTION_REQUIRED") => (
            TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some("antigravity: this account needs an eligible Antigravity subscription".into()),
            ),
            false,
        ),
        _ if journal::mentions_usage_limit(text) => (failed(error), false),
        _ => (
            TurnOutcome::ended(TurnEndStatus::Failed, Some(SIGN_IN_REQUIRED.into())),
            true,
        ),
    }
}

/// A request error as a sanitized turn failure. Raw agent messages can carry
/// paths or provider bodies, so only codes and classes leave this function.
pub(crate) fn failed(error: &RpcError) -> TurnOutcome {
    let text = error.message();
    if journal::mentions_usage_limit(text) {
        return usage_limited(reset_at(text, now_ms()));
    }
    let message = if matches!(error, RpcError::Remote { code: -32000, .. })
        || text.to_ascii_lowercase().contains("auth")
    {
        SIGN_IN_REQUIRED.to_string()
    } else {
        format!("antigravity error ({})", error.label())
    };
    TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
}

/// A quota failure; `resets_at` arms the engine's automatic resume.
pub(crate) fn usage_limited(resets_at: Option<i64>) -> TurnOutcome {
    TurnOutcome {
        usage_limit_resets_at: resets_at,
        ..TurnOutcome::ended(
            TurnEndStatus::Failed,
            Some("usage_limit: the Antigravity quota is reached".to_string()),
        )
    }
}

/// When a quota error says it resets: the provider's `quotaResetTimeStamp`
/// (RFC 3339), else a `quotaResetDelay` or `retryDelay` Go duration from
/// `now` (ms).
pub(crate) fn reset_at(text: &str, now: i64) -> Option<i64> {
    if let Some(at) = reset_stamp(text) {
        return Some(at);
    }
    let delay = reset_field(text, "quotaResetDelay:")
        .or_else(|| reset_field(text, "retryDelay:"))
        .or_else(|| resets_in(text))?;
    go_duration_ms(delay)
        .filter(|ms| *ms > 0.0)
        .map(|ms| now.saturating_add(ms.round() as i64))
}

/// The value after `key`, up to the next delimiter.
fn reset_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let start = text.find(key)? + key.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, ']' | ',' | '"' | '}'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The provider's own `quotaResetTimeStamp` (RFC 3339), which needs no
/// anchor: a failure read long after it happened keeps only this.
pub(crate) fn reset_stamp(text: &str) -> Option<i64> {
    let stamp = reset_field(text, "quotaResetTimeStamp:")?;
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok() // silent-ok: an unparsable stamp falls back to the delay
        .map(|at| at.timestamp_millis())
}

/// The Go duration of agy 1.5's "... Resets in 4m42s." message: digits, dots
/// and unit letters, a sentence's closing dot excluded; a duration glued to
/// another word is none.
fn resets_in(text: &str) -> Option<&str> {
    const KEY: &str = "Resets in ";
    let start = text.find(KEY)? + KEY.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || "nsuµμmh".contains(c)))
        .unwrap_or(rest.len());
    if rest[end..].starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    // A dot not followed by a digit closes the sentence.
    let duration = rest.get(..end)?.trim_end_matches('.');
    Some(duration).filter(|d| !d.is_empty())
}

/// A Go duration (`1h2m3.5s`, `250ms`) in milliseconds.
fn go_duration_ms(raw: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = raw;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let amount: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, factor) = [
            ("ns", 1e-6),
            ("us", 1e-3),
            ("µs", 1e-3),
            ("μs", 1e-3),
            ("ms", 1.0),
            ("h", 3_600_000.0),
            ("m", 60_000.0),
            ("s", 1_000.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        rest = &rest[unit.len()..];
        total += amount * factor;
    }
    Some(total)
}

/// Model ids from ACP `configOptions` (the `model` select, flat or grouped).
pub(crate) fn model_options(config_options: &Value) -> Vec<String> {
    let Some(model) = model_option(config_options) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in model["options"].as_array().into_iter().flatten() {
        if let Some(value) = entry.get("value").and_then(Value::as_str) {
            out.push(value.to_string());
        } else {
            for inner in entry["options"].as_array().into_iter().flatten() {
                if let Some(value) = inner.get("value").and_then(Value::as_str) {
                    out.push(value.to_string());
                }
            }
        }
    }
    out
}

pub(crate) fn current_model(config_options: &Value) -> Option<String> {
    model_option(config_options)?
        .get("currentValue")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn model_option(config_options: &Value) -> Option<&Value> {
    config_options
        .as_array()?
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
}

/// The thinking-level select (`agy` 1.5 reports it beside the model; the
/// 1.3 runtime folded it into model ids such as `gemini-3.8-flash-high`).
pub(crate) fn effort_option(config_options: &Value) -> Option<&Value> {
    config_options.as_array()?.iter().find(|o| {
        o.get("category").and_then(Value::as_str) == Some("thought_level")
            && o.get("type").and_then(Value::as_str) == Some("select")
    })
}

/// The levels the thinking-level select offers.
pub(crate) fn effort_options(config_options: &Value) -> Vec<String> {
    effort_option(config_options)
        .and_then(|o| o["options"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|o| o.get("value").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// How the thread's model is applied to a session: a listed model as is; a
/// pre-1.5 id that extends a listed family (`gemini-3.8-flash-high`) is
/// still accepted by the agent; anything else is not this account's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ModelPick {
    Listed,
    Legacy,
    Unavailable,
}

pub(crate) fn pick_model(config_options: &Value, model: &str) -> ModelPick {
    let listed = model_options(config_options);
    if listed.iter().any(|m| m == model) {
        ModelPick::Listed
    } else if listed.iter().any(|family| {
        model
            .strip_prefix(family.as_str())
            .is_some_and(|r| r.starts_with('-'))
    }) {
        ModelPick::Legacy
    } else {
        ModelPick::Unavailable
    }
}

/// A model id that appends a thinking level the session offers to a listed
/// model (`gemini-4-neon-high`, as a delegating agent may write it): that
/// model and that level. A listed id is never split.
pub(crate) fn split_effort(config_options: &Value, model: &str) -> Option<(String, String)> {
    let listed = model_options(config_options);
    if listed.iter().any(|m| m == model) {
        return None;
    }
    let (family, level) = model.rsplit_once('-')?;
    (listed.iter().any(|m| m == family)
        && effort_options(config_options).iter().any(|e| e == level))
    .then(|| (family.to_string(), level.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn quota_errors_become_usage_limits_without_leaking_the_message() {
        let error = RpcError::Remote {
            code: -32603,
            message: "RESOURCE_EXHAUSTED: quota for /Users/x exceeded".into(),
            data: None,
        };
        let outcome = PromptFold::default().outcome(false, &Err(error));
        assert_eq!(outcome.status, TurnEndStatus::Failed);
        let text = outcome.error.unwrap();
        assert!(text.starts_with("usage_limit"), "{text}");
        assert!(!text.contains("/Users"));
    }

    #[test]
    fn quota_errors_carry_their_reset_time() {
        let now = 1_000_000;
        assert_eq!(
            reset_at("Status: RESOURCE_EXHAUSTED [quotaResetDelay:1h2m3.5s]", now),
            Some(now + 3_723_500)
        );
        assert_eq!(reset_at("retryDelay:250ms, more", now), Some(now + 250));
        assert_eq!(
            reset_at("quotaResetTimeStamp:2026-11-20T00:00:00Z]", now),
            Some(1_795_132_800_000)
        );
        assert_eq!(reset_at("no reset here", now), None);
        // agy 1.5 may state the reset only in its message (t3code #42339).
        assert_eq!(
            reset_at("You have exhausted your capacity. Resets in 4m42s.", now),
            Some(now + 282_000)
        );
        assert_eq!(reset_at("Resets in 1.5h", now), Some(now + 5_400_000));
        assert_eq!(reset_at("Resets in 4m42sx", now), None);
        let error = RpcError::Remote {
            code: -32603,
            message: "RESOURCE_EXHAUSTED quota [quotaResetDelay:30s]".into(),
            data: None,
        };
        let outcome = PromptFold::default().outcome(false, &Err(error));
        assert!(
            outcome
                .usage_limit_resets_at
                .is_some_and(|at| at > now_ms())
        );
        // A `refusal` stop is the agent's quota stop.
        let refused = PromptFold::default().outcome(false, &Ok(json!({"stopReason": "refusal"})));
        assert!(refused.error.unwrap().starts_with("usage_limit"));
    }

    #[test]
    fn sign_in_failures_say_what_happened() {
        let (crashed, signed_out) = sign_in_failed(&RpcError::Closed, true);
        assert!(!signed_out);
        assert!(crashed.error.unwrap().contains("exited during sign-in"));
        let (timeout, signed_out) = sign_in_failed(&RpcError::Timeout, true);
        assert!(signed_out);
        assert_eq!(timeout.error.as_deref(), Some(SIGN_IN_REQUIRED));
        let remote = |message: &str| RpcError::Remote {
            code: -32603,
            message: message.into(),
            data: None,
        };
        // agy 1.5 with no sign-in and no browser it can use.
        let (no_browser, signed_out) = sign_in_failed(
            &remote("No browser is available here. Run `agy acp --login`"),
            true,
        );
        assert!(signed_out);
        assert_eq!(no_browser.error.as_deref(), Some(SIGN_IN_REQUIRED));
        let (plan, signed_out) =
            sign_in_failed(&remote("SUBSCRIPTION_REQUIRED for /Users/x"), true);
        assert!(!signed_out);
        let plan = plan.error.unwrap();
        assert!(
            plan.contains("subscription") && !plan.contains("/Users"),
            "{plan}"
        );
    }

    #[test]
    fn goal_marker_is_stripped_and_a_lost_signature_is_recognized() {
        let mut fold = PromptFold::default();
        let chunk = |s: &str| json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": s}});
        fold.push_update(&chunk("DONE <!-- GOAL_COMPLETE -->"), Instant::now());
        let outcome = fold.outcome(false, &Ok(json!({"stopReason": "end_turn"})));
        assert_eq!(outcome.final_text.as_deref(), Some("DONE"));
        assert!(!fold.lost_thinking_signature(&Ok(json!({}))));
        let lost = RpcError::Remote {
            code: -32603,
            message: "messages.3.content.0: Invalid `signature` in `thinking` block".into(),
            data: None,
        };
        assert!(fold.lost_thinking_signature(&Err(lost)));
    }

    #[test]
    fn models_and_efforts_from_agy_1_5_options() {
        let options = json!([
            {"id": "model", "type": "select", "currentValue": "gemini-4-neon",
             "options": [{"value": "gemini-4-neon"}, {"value": "gemini-3.8-flash"}]},
            {"id": "thought_level", "category": "thought_level", "type": "select",
             "currentValue": "high", "options": [{"value": "low"}, {"value": "high"}]},
        ]);
        assert_eq!(effort_options(&options), ["low", "high"]);
        assert_eq!(pick_model(&options, "gemini-4-neon"), ModelPick::Listed);
        assert_eq!(
            pick_model(&options, "gemini-3.8-flash-high"),
            ModelPick::Legacy
        );
        assert_eq!(pick_model(&options, "gemini-3.8"), ModelPick::Unavailable);
        // t3code #39413: a delegated id that appends a level the session
        // offers is that model at that level.
        assert_eq!(
            split_effort(&options, "gemini-4-neon-high"),
            Some(("gemini-4-neon".to_string(), "high".to_string()))
        );
        assert_eq!(split_effort(&options, "gemini-4-neon-ultra"), None);
        assert_eq!(split_effort(&options, "gemini-4-neon"), None);
    }

    #[test]
    fn answer_is_the_text_after_the_last_tool_call() {
        let mut fold = PromptFold::default();
        let t = Instant::now();
        let chunk = |s: &str| json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": s}});
        fold.push_update(&chunk("Let me look."), t);
        fold.push_update(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "kind": "read"}),
            t,
        );
        fold.push_update(&chunk("Done."), t + Duration::from_millis(10));
        let outcome = fold.outcome(false, &Ok(json!({"stopReason": "end_turn"})));
        assert_eq!(outcome.final_text.as_deref(), Some("Done."));
        let entry = fold.assistant_message().unwrap();
        assert_eq!(entry.tool_calls, vec!["read()".to_string()]);
    }
}
