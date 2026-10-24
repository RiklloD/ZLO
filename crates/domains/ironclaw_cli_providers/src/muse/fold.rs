//! Folding one Muse turn's MSP view events into its outcome, transcript entry
//! and telemetry. Pure: the process side feeds notifications in.

use std::collections::{BTreeMap, HashSet};

use ironclaw_orchestrator::runtime::{
    HistoryMessage, HistoryRole, TurnEndStatus, TurnOutcome, TurnUsage,
};
use ironclaw_orchestrator::telemetry::{TurnTelemetry, tokens_per_second};
use serde_json::Value;

use crate::journal;

/// Muse's own rows carry these tools' work; the call is housekeeping.
const HOUSEKEEPING_TOOLS: [&str; 3] = ["write_todos", "request_user_input", "workflow"];
const HIDDEN_KINDS: [&str; 5] = [
    "userMessage",
    "reminderChild",
    "agentMessage",
    "reasoning",
    "compaction",
];

#[derive(Debug, Default)]
pub(crate) struct TurnFold {
    /// The turn the host admitted, plus turns it ran on its own inside it.
    pub(crate) turn_id: Option<String>,
    joined: HashSet<String>,
    /// Agent messages by item id: (first-seen order, revision, text).
    messages: BTreeMap<String, (usize, u64, String)>,
    tools: Vec<String>,
    tool_items: HashSet<String>,
    /// Last model call: counted-once prompt tokens, cache read, cache write.
    last_prompt: Option<(u64, u64, u64)>,
    /// Sums over the turn's model calls.
    usage: TurnUsage,
    calls: u64,
    output_tokens: u64,
    generation_ms: u64,
    /// Latest `session/contextUsage`: used, window.
    context: Option<(u64, Option<u64>)>,
    pub(crate) model: Option<String>,
    /// The latest `usage/changed` subscription windows.
    pub(crate) limit_reading: Option<Value>,
    completed: Option<Value>,
}

impl TurnFold {
    fn owns(&self, turn_id: Option<&str>) -> bool {
        match (turn_id, self.turn_id.as_deref()) {
            (None, _) => true,
            (Some(id), Some(ours)) => id == ours || self.joined.contains(id),
            (Some(_), None) => false,
        }
    }

    /// One server notification. Returns `true` once the turn has completed.
    pub(crate) fn push(&mut self, method: &str, params: &Value) -> bool {
        let turn = params.get("turnId").and_then(Value::as_str);
        match method {
            "turn/started" => {
                if let Some(id) = turn
                    && self.turn_id.as_deref() != Some(id)
                {
                    // A turn Muse launched on its own (a finished workflow).
                    self.joined.insert(id.to_string());
                }
            }
            "item/started" | "item/updated" | "item/completed" => self.push_item(&params["item"]),
            "item/delta" => {
                let id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let field = params
                    .get("field")
                    .and_then(Value::as_str)
                    .unwrap_or("text");
                if field == "text"
                    && let Some(entry) = self.messages.get_mut(id)
                {
                    entry.2.push_str(
                        params
                            .get("delta")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
            }
            "session/tokenUsage" if self.owns(turn) => {
                let u = &params["usage"];
                let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
                let read = n(u, "cacheReadTokens")
                    .or_else(|| n(u, "cachedTokens"))
                    .unwrap_or(0);
                let write = n(u, "cacheWriteTokens").unwrap_or(0);
                let prompt = n(params, "promptTokens").unwrap_or(0);
                let output = n(u, "outputTokens").unwrap_or(0);
                self.last_prompt = Some((prompt, read, write));
                self.usage.add(&TurnUsage {
                    input_tokens: prompt.saturating_sub(read + write),
                    output_tokens: output,
                    cache_read_tokens: read,
                    cache_creation_tokens: write,
                    context_window: None,
                });
                self.calls += 1;
                if let Some(ms) = n(params, "durationMs") {
                    self.output_tokens += output;
                    self.generation_ms += ms;
                }
                if let Some(model) = params.get("modelId").and_then(Value::as_str) {
                    self.model = Some(model.to_string());
                }
            }
            "session/contextUsage" => {
                if let Some(used) = params.get("usedTokens").and_then(Value::as_u64) {
                    let window = params.get("windowTokens").and_then(Value::as_u64);
                    self.context = Some((used, window));
                }
            }
            "usage/changed" => self.limit_reading = Some(params.clone()),
            "turn/completed" if turn.is_some() && turn == self.turn_id.as_deref() => {
                self.completed = Some(params.clone());
                return true;
            }
            _ => {}
        }
        false
    }

    fn push_item(&mut self, item: &Value) {
        let Some(id) = item.get("itemId").and_then(Value::as_str) else {
            return;
        };
        if !self.owns(item.get("turnId").and_then(Value::as_str)) {
            return;
        }
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or_default();
        if kind == "agentMessage" {
            let revision = item.get("revision").and_then(Value::as_u64).unwrap_or(0);
            let order = self.messages.len();
            let entry = self
                .messages
                .entry(id.to_string())
                .or_insert((order, 0, String::new()));
            if revision >= entry.1 {
                entry.1 = revision;
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    entry.2 = text.to_string();
                }
            }
            return;
        }
        let tool = item.get("tool").and_then(Value::as_str);
        let housekeeping = kind == "toolCall"
            && tool.is_some_and(|t| HOUSEKEEPING_TOOLS.contains(&t))
            && item.get("status").and_then(Value::as_str) != Some("failed");
        if HIDDEN_KINDS.contains(&kind) || housekeeping || !self.tool_items.insert(id.to_string()) {
            return;
        }
        self.tools.push(format!("{}()", tool.unwrap_or(kind)));
    }

    /// The last agent message: the turn's answer.
    fn answer(&self) -> Option<String> {
        self.messages
            .values()
            .filter(|(_, _, text)| !text.trim().is_empty())
            .max_by_key(|(order, _, _)| *order)
            .map(|(_, _, text)| text.trim().to_string())
    }

    pub(crate) fn assistant_message(&self) -> Option<HistoryMessage> {
        let text = self.answer().unwrap_or_default();
        (!text.is_empty() || !self.tools.is_empty())
            .then(|| journal::message(HistoryRole::Assistant, text, self.tools.clone()))
    }

    fn usage(&self) -> Option<TurnUsage> {
        (self.calls > 0).then(|| TurnUsage {
            context_window: self.context.and_then(|(_, w)| w),
            ..self.usage
        })
    }

    /// Context from `session/contextUsage` (else the last call's prompt),
    /// max from it or the model catalog, cache rate from the last call,
    /// speed over the measured model calls.
    pub(crate) fn telemetry(
        &self,
        provider: &str,
        catalog_window: Option<u64>,
    ) -> Option<TurnTelemetry> {
        let (prompt, read, write) = self.last_prompt?;
        let mut t =
            TurnTelemetry::from_prompt_tokens(prompt.saturating_sub(read + write), read, write);
        if let Some((used, window)) = self.context {
            t.context_used = Some(used);
            t.context_max = window;
        }
        t.context_max = t.context_max.or(catalog_window);
        t.tps = tokens_per_second(self.output_tokens, self.generation_ms);
        t.provider = Some(provider.to_string());
        t.model = self.model.clone();
        Some(t)
    }

    /// The turn's outcome once `turn/completed` arrived or the host died.
    pub(crate) fn outcome(&self, cancelled: bool) -> TurnOutcome {
        let usage = self.usage();
        let Some(done) = &self.completed else {
            if cancelled {
                return TurnOutcome::ended(TurnEndStatus::Cancelled, None);
            }
            return TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some("muse host exited before the turn completed".into()),
            );
        };
        match done.get("terminal").and_then(Value::as_str) {
            Some("completed") => TurnOutcome {
                usage,
                ..TurnOutcome::completed(self.answer())
            },
            Some("cancelled") | Some("interrupted") => TurnOutcome {
                usage,
                ..TurnOutcome::ended(TurnEndStatus::Cancelled, None)
            },
            other => {
                let error = &done["error"];
                let kind = error
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let resets_at = error["quota"].get("resetsAtMs").and_then(Value::as_i64);
                // Raw failure text can carry paths or provider bodies; keep classes.
                let message = if resets_at.is_some() {
                    "usage_limit: the Muse Code subscription limit is reached".to_string()
                } else if kind == "authRequired" {
                    "muse: sign-in required (run `muse login`)".to_string()
                } else {
                    format!("muse turn {} ({kind})", other.unwrap_or("failed"))
                };
                TurnOutcome {
                    usage,
                    usage_limit_resets_at: resets_at,
                    ..TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
                }
            }
        }
    }
}

/// `model/list` rows: Meta's catalog only, first entry per model id, with
/// each model's context limit.
pub(crate) fn model_rows(catalog: &Value) -> Vec<(String, Option<u64>, bool)> {
    let mut seen = HashSet::new();
    catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m.get("providerId").and_then(Value::as_str) == Some("meta"))
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            seen.insert(id.to_string()).then(|| {
                (
                    id.to_string(),
                    m.get("contextLimit").and_then(Value::as_u64),
                    m.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quota_failure_arms_resume_at_reset_without_leaking_text() {
        let mut fold = TurnFold {
            turn_id: Some("t1".into()),
            ..TurnFold::default()
        };
        let done = fold.push(
            "turn/completed",
            &json!({"turnId": "t1", "terminal": "failed", "error": {
                "kind": "modelError", "message": "quota for /Users/x", "retryable": false,
                "quota": {"limitKind": "fiveHour", "resetsAtMs": 1_900_000_000_000_i64}}}),
        );
        assert!(done);
        let outcome = fold.outcome(false);
        assert_eq!(outcome.status, TurnEndStatus::Failed);
        assert_eq!(outcome.usage_limit_resets_at, Some(1_900_000_000_000));
        assert!(!outcome.error.unwrap().contains("/Users"));
    }

    #[test]
    fn other_turns_and_housekeeping_tools_stay_out() {
        let mut fold = TurnFold {
            turn_id: Some("t1".into()),
            ..TurnFold::default()
        };
        let item = |id: &str, turn: &str, kind: &str, tool: &str| json!({"item": {"itemId": id, "turnId": turn, "kind": kind, "tool": tool, "status": "completed"}});
        fold.push("item/completed", &item("a", "t1", "toolCall", "read_file"));
        fold.push(
            "item/completed",
            &item("b", "t1", "toolCall", "write_todos"),
        );
        fold.push("item/completed", &item("c", "t0", "toolCall", "bash"));
        fold.push("item/updated", &item("a", "t1", "toolCall", "read_file"));
        let entry = fold.assistant_message().unwrap();
        assert_eq!(entry.tool_calls, vec!["read_file()".to_string()]);
    }
}
