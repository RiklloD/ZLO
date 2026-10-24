//! Folding one prompt's ACP `session/update` stream and its `session/prompt`
//! answer into the turn's outcome, transcript entries and telemetry. Pure:
//! the process side feeds updates and timestamps in.

use std::time::Instant;

use ironclaw_orchestrator::runtime::{HistoryRole, TurnEndStatus, TurnOutcome, TurnUsage};
use ironclaw_orchestrator::telemetry::{TurnTelemetry, tokens_per_second};
use serde_json::Value;

use crate::journal;
use crate::rpc::RpcError;

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
                let title = update
                    .get("kind")
                    .or_else(|| update.get("title"))
                    .and_then(Value::as_str)
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
        let text = self.answer.trim().to_string();
        (!text.is_empty() || !self.tools.is_empty())
            .then(|| journal::message(HistoryRole::Assistant, text, self.tools.clone()))
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
                        ..TurnOutcome::completed(Some(self.answer.trim().to_string()))
                    },
                    Some("cancelled") => TurnOutcome::ended(TurnEndStatus::Cancelled, None),
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

/// A request error as a sanitized turn failure. Raw agent messages can carry
/// paths or provider bodies, so only codes and classes leave this function.
pub(crate) fn failed(error: &RpcError) -> TurnOutcome {
    let text = error.message();
    let message = if journal::mentions_usage_limit(text) {
        "usage_limit: the Antigravity quota is reached".to_string()
    } else if matches!(error, RpcError::Remote { code: -32000, .. })
        || text.to_ascii_lowercase().contains("auth")
    {
        "antigravity: sign-in required (run the Antigravity login)".to_string()
    } else {
        format!("antigravity error ({})", error.label())
    };
    TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
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
