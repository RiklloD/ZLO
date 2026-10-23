//! Per-turn telemetry: the latest context fill, generation speed and prompt
//! cache hit rate of each thread, whatever runtime ran the turn.
//!
//! Runtimes measure; the engine keeps one row per thread (`orch_telemetry`,
//! overwritten by every report) so the status bar survives a restart.

use serde::{Deserialize, Serialize};

use crate::model::Millis;

/// One measurement, taken from the last model call of a turn (context and
/// cache) and from the turn's streamed output (speed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TurnTelemetry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Prompt tokens of the last model call (fresh + cache read + cache write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_max: Option<u64>,
    /// Output tokens per second of generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f64>,
    /// `cache_read / (input + cache_read + cache_creation)` of the last call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
}

impl TurnTelemetry {
    /// Context and cache figures from one call's prompt-side token counts,
    /// with `input` excluding both cache buckets (Anthropic semantics).
    pub fn from_prompt_tokens(input: u64, cache_read: u64, cache_creation: u64) -> Self {
        let total = input + cache_read + cache_creation;
        Self {
            context_used: Some(total),
            cache_hit_rate: (total > 0).then(|| cache_read as f64 / total as f64),
            ..Self::default()
        }
    }

    /// Context fill in percent, when both ends are known.
    pub fn context_pct(&self) -> Option<f64> {
        match (self.context_used, self.context_max) {
            (Some(used), Some(max)) if max > 0 => Some(used as f64 * 100.0 / max as f64),
            _ => None,
        }
    }
}

/// Output tokens per second over `generation_ms`; `None` for spans too short
/// to mean anything.
pub fn tokens_per_second(output_tokens: u64, generation_ms: u64) -> Option<f64> {
    (output_tokens > 0 && generation_ms >= 50)
        .then(|| output_tokens as f64 * 1000.0 / generation_ms as f64)
}

/// The stored row: the latest telemetry of a thread and when it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadTelemetryRecord {
    pub telemetry: TurnTelemetry,
    pub updated_at: Millis,
}
