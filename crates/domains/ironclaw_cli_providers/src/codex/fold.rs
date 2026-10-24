//! Folding one Codex turn's app-server notifications into its outcome,
//! transcript entry and telemetry. Pure: the process side feeds them in.

use std::collections::{BTreeMap, HashSet};

use ironclaw_orchestrator::runtime::{
    HistoryMessage, HistoryRole, TurnEndStatus, TurnOutcome, TurnUsage,
};
use ironclaw_orchestrator::telemetry::TurnTelemetry;
use serde_json::Value;

use crate::journal;

/// Items that carry conversation, not tool work.
const HIDDEN_ITEMS: [&str; 6] = [
    "userMessage",
    "agentMessage",
    "reasoning",
    "plan",
    "hookPrompt",
    "contextCompaction",
];

#[derive(Debug, Default)]
pub(crate) struct TurnFold {
    pub(crate) turn_id: Option<String>,
    /// Agent messages by item id: (first-seen order, text).
    messages: BTreeMap<String, (usize, String)>,
    tools: Vec<String>,
    tool_items: HashSet<String>,
    /// Sum of the turn's model calls, from the running thread total.
    usage: Option<TurnUsage>,
    /// Thread total when the turn began, to take the turn's share.
    base_total: Option<Value>,
    /// Last call: prompt tokens, cache read.
    last_prompt: Option<(u64, u64)>,
    context_window: Option<u64>,
    pub(crate) model: Option<String>,
    /// Latest `account/rateLimits/updated` snapshot.
    pub(crate) limit_reading: Option<Value>,
    completed: Option<Value>,
    /// Last `error` notification that will not be retried.
    error: Option<Value>,
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

impl TurnFold {
    fn owns(&self, params: &Value) -> bool {
        match (
            params.get("turnId").and_then(Value::as_str),
            self.turn_id.as_deref(),
        ) {
            (Some(id), Some(ours)) => id == ours,
            _ => true,
        }
    }

    /// One server notification. Returns `true` once the turn has completed.
    pub(crate) fn push(&mut self, method: &str, params: &Value) -> bool {
        match method {
            "item/agentMessage/delta" if self.owns(params) => {
                let id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let order = self.messages.len();
                let entry = self
                    .messages
                    .entry(id.to_string())
                    .or_insert((order, String::new()));
                entry.1.push_str(
                    params
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            "item/completed" if self.owns(params) => self.push_item(&params["item"]),
            "thread/tokenUsage/updated" if self.owns(params) => {
                let usage = &params["tokenUsage"];
                let total = &usage["total"];
                let last = &usage["last"];
                let base = self.base_total.get_or_insert_with(|| {
                    // First reading of the turn: its `last` is this turn's
                    // first call, so the base is total minus last.
                    serde_json::json!({
                        "inputTokens": n(total, "inputTokens").saturating_sub(n(last, "inputTokens")),
                        "cachedInputTokens": n(total, "cachedInputTokens").saturating_sub(n(last, "cachedInputTokens")),
                        "outputTokens": n(total, "outputTokens").saturating_sub(n(last, "outputTokens")),
                    })
                });
                let input = n(total, "inputTokens").saturating_sub(n(base, "inputTokens"));
                let cached =
                    n(total, "cachedInputTokens").saturating_sub(n(base, "cachedInputTokens"));
                let output = n(total, "outputTokens").saturating_sub(n(base, "outputTokens"));
                self.context_window = usage.get("modelContextWindow").and_then(Value::as_u64);
                self.usage = Some(TurnUsage {
                    input_tokens: input.saturating_sub(cached),
                    output_tokens: output,
                    cache_read_tokens: cached,
                    cache_creation_tokens: 0,
                    context_window: self.context_window,
                });
                self.last_prompt = Some((n(last, "inputTokens"), n(last, "cachedInputTokens")));
            }
            "account/rateLimits/updated" => {
                self.limit_reading = params.get("rateLimits").cloned();
            }
            "model/rerouted" => {
                if let Some(model) = params.get("toModel").and_then(Value::as_str) {
                    self.model = Some(model.to_string());
                }
            }
            "error" if self.owns(params) => {
                if params.get("willRetry").and_then(Value::as_bool) != Some(true) {
                    self.error = params.get("error").cloned();
                }
            }
            "turn/completed" => {
                let turn = &params["turn"];
                if self.turn_id.is_none()
                    || turn.get("id").and_then(Value::as_str) == self.turn_id.as_deref()
                {
                    self.completed = Some(turn.clone());
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    fn push_item(&mut self, item: &Value) {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "agentMessage" {
            let order = self.messages.len();
            let entry = self
                .messages
                .entry(id.to_string())
                .or_insert((order, String::new()));
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                entry.1 = text.to_string();
            }
            return;
        }
        if HIDDEN_ITEMS.contains(&kind) || !self.tool_items.insert(id.to_string()) {
            return;
        }
        let label = match kind {
            "commandExecution" => "shell".to_string(),
            "fileChange" => "apply_patch".to_string(),
            "mcpToolCall" => item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp")
                .to_string(),
            "webSearch" => "web_search".to_string(),
            other => other.to_string(),
        };
        self.tools.push(format!("{label}()"));
    }

    fn answer(&self) -> Option<String> {
        self.messages
            .values()
            .filter(|(_, text)| !text.trim().is_empty())
            .max_by_key(|(order, _)| *order)
            .map(|(_, text)| text.trim().to_string())
    }

    pub(crate) fn assistant_message(&self) -> Option<HistoryMessage> {
        let text = self.answer().unwrap_or_default();
        (!text.is_empty() || !self.tools.is_empty())
            .then(|| journal::message(HistoryRole::Assistant, text, self.tools.clone()))
    }

    pub(crate) fn telemetry(&self, provider: &str) -> Option<TurnTelemetry> {
        let (prompt, cached) = self.last_prompt?;
        let mut t = TurnTelemetry::from_prompt_tokens(prompt.saturating_sub(cached), cached, 0);
        t.context_max = self.context_window;
        t.provider = Some(provider.to_string());
        t.model = self.model.clone();
        Some(t)
    }

    /// When the plan limit resets, from the latest rate-limit snapshot
    /// (unix ms): the later of the windows that are full.
    fn resets_at(&self) -> Option<i64> {
        let reading = self.limit_reading.as_ref()?;
        ["primary", "secondary"]
            .iter()
            .filter_map(|w| reading.get(*w))
            .filter(|w| w.get("usedPercent").and_then(Value::as_f64).unwrap_or(0.) >= 100.)
            .filter_map(|w| w.get("resetsAt").and_then(Value::as_i64))
            .max()
            .map(|s| s * 1000)
    }

    pub(crate) fn outcome(&self, cancelled: bool) -> TurnOutcome {
        let usage = self.usage;
        let Some(turn) = &self.completed else {
            if cancelled {
                return TurnOutcome::ended(TurnEndStatus::Cancelled, None);
            }
            return TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some("codex app-server exited before the turn completed".into()),
            );
        };
        match turn.get("status").and_then(Value::as_str) {
            Some("completed") => TurnOutcome {
                usage,
                ..TurnOutcome::completed(self.answer())
            },
            Some("interrupted") => TurnOutcome {
                usage,
                ..TurnOutcome::ended(TurnEndStatus::Cancelled, None)
            },
            _ => {
                let error = turn
                    .get("error")
                    .filter(|e| !e.is_null())
                    .or(self.error.as_ref());
                let info = error.map(|e| &e["codexErrorInfo"]);
                let code = info
                    .and_then(|i| {
                        i.as_str()
                            .map(str::to_string)
                            .or_else(|| i.as_object()?.keys().next().cloned())
                    })
                    .unwrap_or_else(|| "unknown".into());
                let limited = code == "usageLimitExceeded";
                // Raw failure text can carry paths or provider bodies; keep classes.
                let message = match code.as_str() {
                    "usageLimitExceeded" => {
                        "usage_limit: the ChatGPT plan limit for Codex is reached".to_string()
                    }
                    "unauthorized" => "codex: sign-in required (run `codex login`)".to_string(),
                    "contextWindowExceeded" => "codex: the context window is full".to_string(),
                    other => format!("codex turn failed ({other})"),
                };
                TurnOutcome {
                    usage,
                    usage_limit_resets_at: if limited { self.resets_at() } else { None },
                    ..TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
                }
            }
        }
    }
}

/// `model/list` rows: visible models, the default first.
pub(crate) fn model_rows(list: &Value) -> (Vec<String>, Option<String>) {
    let mut models = Vec::new();
    let mut default = None;
    for m in list["data"].as_array().into_iter().flatten() {
        if m.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(id) = m
            .get("model")
            .or_else(|| m.get("id"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if m.get("isDefault").and_then(Value::as_bool) == Some(true) {
            default = Some(id.to_string());
        }
        if !models.iter().any(|x| x == id) {
            models.push(id.to_string());
        }
    }
    if let Some(d) = &default {
        models.retain(|m| m != d);
        models.insert(0, d.clone());
    }
    (models, default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold() -> TurnFold {
        TurnFold {
            turn_id: Some("t1".into()),
            ..TurnFold::default()
        }
    }

    #[test]
    fn answer_tools_and_usage_fold_from_notifications() {
        let mut f = fold();
        f.push(
            "item/agentMessage/delta",
            &json!({"turnId": "t1", "itemId": "m1", "delta": "Hel"}),
        );
        f.push(
            "item/agentMessage/delta",
            &json!({"turnId": "t1", "itemId": "m1", "delta": "lo"}),
        );
        f.push(
            "item/completed",
            &json!({"turnId": "t1", "item": {"type": "commandExecution", "id": "c1", "command": "ls"}}),
        );
        f.push(
            "item/completed",
            &json!({"turnId": "t0", "item": {"type": "fileChange", "id": "x"}}),
        );
        f.push(
            "thread/tokenUsage/updated",
            &json!({"turnId": "t1", "tokenUsage": {
                "total": {"inputTokens": 1500, "cachedInputTokens": 500, "outputTokens": 40},
                "last": {"inputTokens": 1000, "cachedInputTokens": 500, "outputTokens": 40},
                "modelContextWindow": 272000}}),
        );
        let done = f.push(
            "turn/completed",
            &json!({"threadId": "th", "turn": {"id": "t1", "status": "completed", "error": null}}),
        );
        assert!(done);
        let out = f.outcome(false);
        assert_eq!(out.status, TurnEndStatus::Completed);
        let usage = out.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.output_tokens
            ),
            (500, 500, 40)
        );
        let entry = f.assistant_message().unwrap();
        assert_eq!(entry.text, "Hello");
        assert_eq!(entry.tool_calls, vec!["shell()".to_string()]);
        assert_eq!(f.telemetry("codex").unwrap().context_max, Some(272000));
    }

    #[test]
    fn usage_limit_arms_resume_at_the_full_window_reset() {
        let mut f = fold();
        f.push(
            "account/rateLimits/updated",
            &json!({"rateLimits": {
                "primary": {"usedPercent": 100.0, "resetsAt": 1_900_000_000_i64},
                "secondary": {"usedPercent": 40.0, "resetsAt": 1_900_500_000_i64}}}),
        );
        f.push(
            "turn/completed",
            &json!({"turn": {"id": "t1", "status": "failed", "error": {
                "message": "limit for /Users/x", "codexErrorInfo": "usageLimitExceeded"}}}),
        );
        let out = f.outcome(false);
        assert_eq!(out.status, TurnEndStatus::Failed);
        assert_eq!(out.usage_limit_resets_at, Some(1_900_000_000_000));
        assert!(!out.error.unwrap().contains("/Users"));
    }

    #[test]
    fn model_list_puts_the_default_first_and_skips_hidden() {
        let (models, default) = model_rows(&json!({"data": [
            {"id": "a", "model": "gpt-a", "isDefault": false, "hidden": false},
            {"id": "b", "model": "gpt-b", "isDefault": true, "hidden": false},
            {"id": "c", "model": "gpt-c", "isDefault": false, "hidden": true},
        ]}));
        assert_eq!(models, vec!["gpt-b", "gpt-a"]);
        assert_eq!(default.as_deref(), Some("gpt-b"));
    }
}
