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
pub(crate) const HOUSEKEEPING_TOOLS: [&str; 3] = ["write_todos", "request_user_input", "workflow"];
pub(crate) const HIDDEN_KINDS: [&str; 5] = [
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
    /// The item or event belongs to the admitted turn or one it joined
    /// (an event naming no turn belongs to the session's current one).
    pub(crate) fn owns(&self, turn_id: Option<&str>) -> bool {
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

    /// Turns Muse ran on its own inside this one (Stop interrupts them too).
    pub(crate) fn joined(&self) -> Vec<String> {
        self.joined.iter().cloned().collect()
    }

    /// The turn's outcome once `turn/completed` arrived or the host died.
    /// `now` bounds the usage-limit reset to the future.
    pub(crate) fn outcome(&self, cancelled: bool, now: i64) -> TurnOutcome {
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
                let (message, resets_at) = self.failure(done, other.unwrap_or("failed"), now);
                TurnOutcome {
                    usage,
                    usage_limit_resets_at: resets_at,
                    ..TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
                }
            }
        }
    }

    /// A failed turn's message class and, for a usage limit, when to resume.
    /// Raw failure text can carry paths, request ids or billing URLs, so only
    /// classes (and the model id) reach the person.
    fn failure(&self, done: &Value, terminal: &str, now: i64) -> (String, Option<i64>) {
        let error = &done["error"];
        let kind = error
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| done.get("reason").and_then(Value::as_str))
            .unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        let quota = error["quota"].get("resetsAtMs").and_then(Value::as_i64);
        // Credential proxies may drop `error.quota` from a 429; read the text too.
        let limited = quota.is_some()
            || (has_status(&lower, "429")
                && (lower.contains("quota") || lower.contains("usage limit")));
        if limited {
            let resets = limit_reset(quota, &lower, self.limit_reading.as_ref(), now);
            return (
                "usage_limit: the Muse Code subscription limit is reached".to_string(),
                resets,
            );
        }
        let message = if kind == "authRequired" {
            "muse: sign-in required (run `muse login`)".to_string()
        } else if lower.contains("lack access") {
            // Muse lists models an account may not be able to use.
            let model = self.model.as_deref().unwrap_or("this model");
            format!("muse: this account cannot use {model}; pick another model")
        } else if has_status(&lower, "402") || lower.contains("billing") {
            "muse: the Meta account has a billing issue; resolve it at dev.meta.ai, then retry"
                .to_string()
        } else {
            format!("muse turn {terminal} ({kind})")
        };
        (message, None)
    }
}

/// The latest instant chrono and JavaScript both hold.
const LATEST_MS: i64 = 8_640_000_000_000_000;

/// `code` as a whole number in `text` (`429`, not `14290`).
fn has_status(text: &str, code: &str) -> bool {
    text.match_indices(code).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + code.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_digit()) && !after.is_some_and(|c| c.is_ascii_digit())
    })
}

/// Resume only once every exhausted window has reset: the latest future
/// reset among the refusal's quota, a "resets at <RFC 3339>" in its text, and
/// the subscription windows at or over 100%.
fn limit_reset(quota: Option<i64>, lower: &str, usage: Option<&Value>, now: i64) -> Option<i64> {
    let named = lower.find("resets at ").and_then(|at| {
        let rest = &lower[at + "resets at ".len()..];
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | ',' | ';'))
            .collect();
        let stamp = token.trim_end_matches('.').to_ascii_uppercase();
        chrono::DateTime::parse_from_rfc3339(&stamp)
            .ok() // silent-ok: an unreadable reset time falls back to the other sources
            .map(|t| t.timestamp_millis())
    });
    let windows = usage.into_iter().flat_map(|u| {
        ["window", "weekly"].into_iter().filter_map(move |name| {
            let w = &u[name];
            let used = w.get("usedPercent").and_then(Value::as_f64)?;
            (used >= 100.0)
                .then(|| w.get("resetsAtMs").and_then(Value::as_f64))
                .flatten()
                .map(|ms| ms as i64)
        })
    });
    quota
        .into_iter()
        .chain(named)
        .chain(windows)
        .filter(|ms| *ms > now && *ms <= LATEST_MS)
        .max()
}

/// Reasoning tiers Muse defines; anything else in a catalog is ignored.
const KNOWN_EFFORTS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// One `model/list` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRow {
    pub(crate) id: String,
    pub(crate) context_limit: Option<u64>,
    pub(crate) is_default: bool,
    /// The tiers it reports (`None` when it reports none or `"unknown"`).
    pub(crate) efforts: Option<Vec<String>>,
    pub(crate) default_effort: Option<String>,
}

impl ModelRow {
    /// The tier to send for `picked`: kept when the model offers it, else
    /// the model's default (Muse may refuse a tier the model lacks). No pick
    /// keeps Muse's default.
    pub(crate) fn effort_for(&self, picked: Option<&str>) -> Option<String> {
        let picked = picked?;
        let Some(efforts) = &self.efforts else {
            return Some(picked.to_string());
        };
        if efforts.iter().any(|e| e == picked) {
            return Some(picked.to_string());
        }
        self.default_effort
            .as_ref()
            .filter(|d| efforts.contains(d))
            .or_else(|| efforts.first())
            .cloned()
    }
}

/// `model/list` rows: Meta's catalog only, first entry per model id.
pub(crate) fn model_rows(catalog: &Value) -> Vec<ModelRow> {
    let mut seen = HashSet::new();
    catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m.get("providerId").and_then(Value::as_str) == Some("meta"))
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            seen.insert(id.to_string()).then(|| ModelRow {
                id: id.to_string(),
                context_limit: m.get("contextLimit").and_then(Value::as_u64),
                is_default: m.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
                efforts: m.get("variants").and_then(Value::as_array).map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .filter(|t| KNOWN_EFFORTS.contains(t))
                        .map(str::to_string)
                        .collect()
                }),
                default_effort: m
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_string),
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
        let outcome = fold.outcome(false, 0);
        assert_eq!(outcome.status, TurnEndStatus::Failed);
        assert_eq!(outcome.usage_limit_resets_at, Some(1_900_000_000_000));
        assert!(!outcome.error.unwrap().contains("/Users"));
    }

    fn failed_with(fold: &mut TurnFold, error: Value) -> TurnOutcome {
        fold.turn_id = Some("t1".into());
        fold.push(
            "turn/completed",
            &json!({"turnId": "t1", "terminal": "failed", "error": error}),
        );
        fold.outcome(false, 1_000)
    }

    #[test]
    fn a_429_without_quota_still_waits_for_the_latest_exhausted_window() {
        let mut fold = TurnFold {
            limit_reading: Some(json!({
                "window": {"usedPercent": 100, "resetsAtMs": 5_000},
                "weekly": {"usedPercent": 40, "resetsAtMs": 9_000},
            })),
            ..TurnFold::default()
        };
        let outcome = failed_with(
            &mut fold,
            json!({"kind": "modelError", "message": "HTTP 429: usage limit reached"}),
        );
        assert!(outcome.error.unwrap().starts_with("usage_limit:"));
        assert_eq!(outcome.usage_limit_resets_at, Some(5_000));

        // A reset named in the text, later than the window's, wins.
        let outcome = failed_with(
            &mut fold,
            json!({"kind": "modelError",
                   "message": "429 quota exceeded; resets at 2030-01-01T00:00:00Z."}),
        );
        assert_eq!(outcome.usage_limit_resets_at, Some(1_893_456_000_000));

        // A 4290 is not a 429.
        let outcome = failed_with(
            &mut fold,
            json!({"kind": "modelError", "message": "code 4290 quota"}),
        );
        assert!(!outcome.error.unwrap().starts_with("usage_limit:"));
    }

    #[test]
    fn listed_models_without_access_and_billing_refusals_get_their_own_class() {
        let mut fold = TurnFold {
            model: Some("muse-wonder-1.0".into()),
            ..TurnFold::default()
        };
        let outcome = failed_with(
            &mut fold,
            json!({"kind": "modelError",
                   "message": "Model muse-wonder-1.0 does not exist or you lack access to it"}),
        );
        assert_eq!(
            outcome.error.as_deref(),
            Some("muse: this account cannot use muse-wonder-1.0; pick another model")
        );
        let outcome = failed_with(
            &mut fold,
            json!({"kind": "modelError", "message": "API error 402 [request_id=x]: Billing verification failed. https://dev.meta.ai/billing/?team_id=1 (billing_error)"}),
        );
        let message = outcome.error.unwrap();
        assert!(message.contains("billing issue"), "{message}");
        assert!(!message.contains("team_id"), "{message}");
        assert_eq!(outcome.usage_limit_resets_at, None);
    }

    #[test]
    fn model_rows_keep_known_tiers_and_fall_back_to_the_default_tier() {
        let rows = model_rows(&json!({"models": [
            {"modelId": "a", "providerId": "meta", "isDefault": true,
             "variants": ["low", "high", "turbo"], "defaultReasoningEffort": "high"},
            {"modelId": "b", "providerId": "meta", "variants": "unknown"},
            {"modelId": "c", "providerId": "other"},
        ]}));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].efforts, Some(vec!["low".into(), "high".into()]));
        assert_eq!(rows[1].efforts, None);
        assert_eq!(rows[0].effort_for(Some("low")).as_deref(), Some("low"));
        assert_eq!(rows[0].effort_for(Some("max")).as_deref(), Some("high"));
        assert_eq!(rows[0].effort_for(None), None);
        assert_eq!(rows[1].effort_for(Some("max")).as_deref(), Some("max"));
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
