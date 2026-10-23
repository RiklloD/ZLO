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
    /// Streamed generation of the message in flight.
    first_delta: Option<Instant>,
    message_output: u64,
    /// Totals over finished messages.
    generation_ms: u64,
    generated_tokens: u64,
    rate_limit_resets_at: Option<i64>,
    rate_limited: bool,
    result: Option<ResultFold>,
}

#[derive(Debug)]
struct ResultFold {
    is_error: bool,
    subtype: String,
    text: Option<String>,
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
                    self.last_prompt = Some((
                        u64::from(u.input_tokens),
                        u64::from(u.cache_read_input_tokens),
                        u64::from(u.cache_creation_input_tokens),
                    ));
                }
            }
            ClaudeOutput::StreamEvent(ev) => self.push_stream_event(&ev.event, now),
            ClaudeOutput::RateLimitEvent(ev) => {
                let info = ev.rate_limit_info;
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
                self.result = Some(ResultFold {
                    is_error: r.is_error,
                    subtype: format!("{:?}", r.subtype).to_lowercase(),
                    text: r.result.clone(),
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
    pub(crate) fn telemetry(&self, provider: &str) -> Option<TurnTelemetry> {
        let (input, read, write) = self.last_prompt?;
        Some(TurnTelemetry {
            provider: Some(provider.to_string()),
            model: self.model.clone(),
            context_max: self.result.as_ref().and_then(|r| r.context_window),
            tps: tokens_per_second(self.generated_tokens, self.generation_ms),
            ..TurnTelemetry::from_prompt_tokens(input, read, write)
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
            return TurnOutcome {
                usage: result.usage,
                ..TurnOutcome::completed(result.text.clone())
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
}
