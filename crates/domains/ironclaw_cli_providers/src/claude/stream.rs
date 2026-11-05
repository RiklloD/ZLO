//! Folding the CLI's stream-json frames into one turn's outcome and
//! telemetry. Pure: the process side feeds lines and timestamps in.

use std::time::Instant;

use claude_codes::{ClaudeOutput, RateLimitStatus};
use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnOutcome, TurnUsage};
use ironclaw_orchestrator::telemetry::{TurnTelemetry, tokens_per_second};

/// What one turn's frames add up to.
#[derive(Debug, Default)]
pub(crate) struct TurnFold {
    model: Option<String>,
    /// Prompt-side tokens of the latest assistant message: (fresh, read, write).
    last_prompt: Option<(u64, u64, u64)>,
    /// A compaction after the latest assistant message, with the context it
    /// left when the CLI says (`post_tokens`). The call before it read the
    /// whole uncompacted context, so its figure no longer describes the
    /// session.
    compacted: Option<Option<u64>>,
    /// Streamed generation of the message in flight.
    first_delta: Option<Instant>,
    message_output: u64,
    /// Totals over finished messages.
    generation_ms: u64,
    generated_tokens: u64,
    rate_limit_resets_at: Option<i64>,
    rate_limited: bool,
    /// The latest `unifiedWindows` reading (5-hour, weekly), as the CLI sent it.
    limit_windows: Option<serde_json::Value>,
    result: Option<ResultFold>,
}

#[derive(Debug)]
struct ResultFold {
    is_error: bool,
    subtype: String,
    text: Option<String>,
    /// Every non-empty reply of the turn, oldest first. A steer sent while
    /// the turn runs gets its own `result`; the answer to the original
    /// prompt (a delegated task's brief) must not be lost behind it.
    replies: Vec<String>,
    usage: Option<TurnUsage>,
    context_window: Option<u64>,
}

impl TurnFold {
    pub(crate) fn push_line(&mut self, line: &str, now: Instant) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match ClaudeOutput::parse_json_tolerant(line) {
            Ok(frame) => self.push(frame, now),
            Err(error) => tracing::debug!(%error, "claude cli: unparsed stream frame"),
        }
    }

    fn push(&mut self, frame: ClaudeOutput, now: Instant) {
        match frame {
            ClaudeOutput::Assistant(msg) => {
                self.model = Some(msg.message.model.clone());
                if let Some(u) = msg.message.usage {
                    self.compacted = None;
                    self.last_prompt = Some((
                        u64::from(u.input_tokens),
                        u64::from(u.cache_read_input_tokens),
                        u64::from(u.cache_creation_input_tokens),
                    ));
                }
            }
            ClaudeOutput::System(sys) => {
                if let Some(boundary) = sys.as_compact_boundary() {
                    self.compacted = Some(boundary.compact_metadata.post_tokens);
                }
            }
            ClaudeOutput::StreamEvent(ev) => self.push_stream_event(&ev.event, now),
            ClaudeOutput::RateLimitEvent(ev) => {
                let info = ev.rate_limit_info;
                if let Some(windows) = &info.unified_windows {
                    self.limit_windows = serde_json::to_value(windows).ok();
                }
                if matches!(info.status, RateLimitStatus::Rejected) {
                    self.rate_limited = true;
                    self.rate_limit_resets_at = info.resets_at.map(|s| s as i64 * 1000);
                }
            }
            ClaudeOutput::Result(r) => {
                let usage = r.usage.as_ref().map(|u| TurnUsage {
                    input_tokens: u64::from(u.input_tokens),
                    output_tokens: u64::from(u.output_tokens),
                    cache_read_tokens: u64::from(u.cache_read_input_tokens),
                    cache_creation_tokens: u64::from(u.cache_creation_input_tokens),
                    context_window: None,
                });
                // The window of the model that answered, else the largest.
                let context_window = r.model_usage.as_ref().and_then(|m| {
                    self.model
                        .as_ref()
                        .and_then(|name| m.get(name))
                        .map(|e| e.context_window)
                        .or_else(|| m.values().map(|e| e.context_window).max())
                        .filter(|w| *w > 0)
                });
                let mut replies = self.result.take().map(|f| f.replies).unwrap_or_default();
                if !r.is_error
                    && let Some(reply) =
                        r.result.as_deref().map(str::trim).filter(|t| !t.is_empty())
                {
                    replies.push(reply.to_string());
                }
                self.result = Some(ResultFold {
                    is_error: r.is_error,
                    subtype: format!("{:?}", r.subtype).to_lowercase(),
                    text: r.result.clone(),
                    replies,
                    usage: usage.map(|u| TurnUsage {
                        context_window,
                        ..u
                    }),
                    context_window,
                });
            }
            _ => {}
        }
    }

    fn push_stream_event(&mut self, event: &serde_json::Value, now: Instant) {
        match event.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                self.first_delta = None;
                self.message_output = 0;
                let msg = &event["message"];
                if let Some(model) = msg.get("model").and_then(|m| m.as_str()) {
                    self.model = Some(model.to_string());
                }
                let u = &msg["usage"];
                if u.is_object() {
                    let n = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    self.last_prompt = Some((
                        n("input_tokens"),
                        n("cache_read_input_tokens"),
                        n("cache_creation_input_tokens"),
                    ));
                }
            }
            Some("content_block_delta") => {
                self.first_delta.get_or_insert(now);
            }
            Some("message_delta") => {
                if let Some(out) = event["usage"].get("output_tokens").and_then(|v| v.as_u64()) {
                    self.message_output = out;
                }
            }
            Some("message_stop") => {
                if let Some(start) = self.first_delta.take() {
                    self.generation_ms += now.duration_since(start).as_millis() as u64;
                    self.generated_tokens += self.message_output;
                }
                self.message_output = 0;
            }
            _ => {}
        }
    }

    /// Context from the last assistant message, max from the result's
    /// `modelUsage`, speed over the turn's streamed messages.
    pub(crate) fn limit_windows(&self) -> Option<&serde_json::Value> {
        self.limit_windows.as_ref()
    }

    /// After a compaction the context is what it left (`None` when the CLI
    /// does not say, so readers estimate instead of showing a stale count).
    pub(crate) fn telemetry(&self, provider: &str) -> Option<TurnTelemetry> {
        let measured = match (self.compacted, self.last_prompt) {
            (Some(left), _) => TurnTelemetry {
                context_used: left,
                ..TurnTelemetry::default()
            },
            (None, Some((input, read, write))) => {
                TurnTelemetry::from_prompt_tokens(input, read, write)
            }
            (None, None) => return None,
        };
        Some(TurnTelemetry {
            provider: Some(provider.to_string()),
            model: self.model.clone(),
            context_max: self.result.as_ref().and_then(|r| r.context_window),
            tps: tokens_per_second(self.generated_tokens, self.generation_ms),
            ..measured
        })
    }

    /// The turn's outcome once the process is gone.
    pub(crate) fn outcome(&self, cancelled: bool, exit: Option<i32>) -> TurnOutcome {
        if cancelled {
            return TurnOutcome::ended(TurnEndStatus::Cancelled, None);
        }
        let Some(result) = &self.result else {
            let code = exit.map_or_else(|| "signal".to_string(), |c| c.to_string());
            return TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some(format!("claude cli exited without a result (exit {code})")),
            );
        };
        if !result.is_error {
            let text = if result.replies.len() > 1 {
                Some(result.replies.join("\n\n"))
            } else {
                result.text.clone()
            };
            return TurnOutcome {
                usage: result.usage,
                ..TurnOutcome::completed(text)
            };
        }
        let text = result.text.as_deref().unwrap_or_default();
        let resets_at = self
            .rate_limit_resets_at
            .or_else(|| legacy_limit_reset(text));
        let limited = self.rate_limited || resets_at.is_some() || mentions_usage_limit(text);
        // Raw CLI error text can carry paths or provider bodies; keep codes.
        let error = if limited {
            "usage_limit: the Claude subscription limit is reached".to_string()
        } else {
            format!("claude cli error ({})", result.subtype)
        };
        TurnOutcome {
            usage: result.usage,
            usage_limit_resets_at: if limited { resets_at } else { None },
            ..TurnOutcome::ended(TurnEndStatus::Failed, Some(error))
        }
    }
}

/// Older CLIs end a limited turn with `...usage limit reached|<epoch secs>`.
fn legacy_limit_reset(text: &str) -> Option<i64> {
    let (_, tail) = text.rsplit_once('|')?;
    let secs: i64 = tail.trim().parse().ok()?;
    (secs > 1_000_000_000).then_some(secs * 1000)
}

fn mentions_usage_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["usage limit", "session limit", "weekly limit", "rate limit"]
        .iter()
        .any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const BIG_CALL: &str = r#"{"type":"assistant","message":{"id":"m","type":"message","role":"assistant","model":"claude-haiku-5-5","content":[{"type":"text","text":"summary"}],"usage":{"input_tokens":10,"cache_read_input_tokens":170000,"cache_creation_input_tokens":0,"output_tokens":900}},"session_id":"s","uuid":"a1"}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":600,"duration_api_ms":500,"num_turns":1,"result":"","session_id":"s","total_cost_usd":0.01,"usage":{"input_tokens":10,"cache_read_input_tokens":170000,"cache_creation_input_tokens":0,"output_tokens":900},"modelUsage":{"claude-haiku-5-5":{"inputTokens":10,"outputTokens":900,"cacheReadInputTokens":170000,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.01,"contextWindow":200000,"maxOutputTokens":32000}}}"#;

    fn boundary(post: Option<u64>) -> String {
        let post = post
            .map(|p| format!(r#","post_tokens":{p}"#))
            .unwrap_or_default();
        format!(
            r#"{{"type":"system","subtype":"compact_boundary","session_id":"s","uuid":"b","compact_metadata":{{"pre_tokens":170010,"trigger":"manual"{post}}}}}"#
        )
    }

    #[test]
    fn a_compaction_reports_the_context_it_left_not_the_one_it_read() {
        let t = Instant::now();
        let mut fold = TurnFold::default();
        for line in [BIG_CALL, &boundary(Some(9_000)), RESULT] {
            fold.push_line(line, t);
        }
        let tel = fold.telemetry("claude-cli").expect("telemetry");
        assert_eq!(
            (tel.context_used, tel.context_max),
            (Some(9_000), Some(200_000))
        );

        // No post-compaction count: the exact figure is gone, not stale.
        let mut fold = TurnFold::default();
        for line in [BIG_CALL, &boundary(None), RESULT] {
            fold.push_line(line, t);
        }
        let tel = fold.telemetry("claude-cli").expect("telemetry");
        assert_eq!((tel.context_used, tel.context_max), (None, Some(200_000)));

        // A call after the boundary measured the compacted context itself.
        let mut fold = TurnFold::default();
        let after = BIG_CALL.replace("170000", "7000");
        for line in [BIG_CALL, &boundary(Some(9_000)), &after, RESULT] {
            fold.push_line(line, t);
        }
        assert_eq!(
            fold.telemetry("claude-cli")
                .expect("telemetry")
                .context_used,
            Some(7_010)
        );

        // A compaction alone (the idle /compact run streams no call of its own).
        let mut fold = TurnFold::default();
        for line in [&boundary(Some(9_000)), RESULT] {
            fold.push_line(line, t);
        }
        assert_eq!(
            fold.telemetry("claude-cli")
                .expect("telemetry")
                .context_used,
            Some(9_000)
        );
    }

    #[test]
    fn usage_limit_result_arms_resume_at_reset() {
        let mut fold = TurnFold::default();
        let t = Instant::now();
        fold.push_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1792801800},"session_id":"s","uuid":"u"}"#,
            t,
        );
        fold.push_line(
            r#"{"type":"result","subtype":"success","is_error":true,"duration_ms":5,"duration_api_ms":0,"num_turns":1,"result":"You've hit your session limit","session_id":"s","total_cost_usd":0}"#,
            t + Duration::from_millis(5),
        );
        let outcome = fold.outcome(false, Some(1));
        assert_eq!(outcome.status, TurnEndStatus::Failed);
        assert_eq!(outcome.usage_limit_resets_at, Some(1_792_801_800_000));
        assert!(!outcome.error.unwrap_or_default().contains("hit your"));
    }

    /// A steer during a delegated child's turn ends in a second `result`.
    /// The task result keeps the answer to the brief ahead of it (it used
    /// to be replaced by the steer's reply).
    #[test]
    fn a_steered_turn_keeps_every_reply() {
        let mut fold = TurnFold::default();
        let t = Instant::now();
        let result = |text: &str| {
            format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"duration_api_ms":0,"num_turns":1,"result":"{text}","session_id":"s","total_cost_usd":0}}"#
            )
        };
        fold.push_line(&result("## Review: the full findings"), t);
        fold.push_line(&result("Stopped."), t + Duration::from_millis(5));
        let outcome = fold.outcome(false, Some(0));
        assert_eq!(outcome.status, TurnEndStatus::Completed);
        assert_eq!(
            outcome.final_text.as_deref(),
            Some("## Review: the full findings\n\nStopped.")
        );

        let mut single = TurnFold::default();
        single.push_line(&result("Only reply"), t);
        assert_eq!(
            single.outcome(false, Some(0)).final_text.as_deref(),
            Some("Only reply")
        );
    }
}
