//! Tier-1 microcompaction: observation masking on the outbound provider
//! request.
//!
//! When the assembled conversation grows past a watermark of the model's
//! context window, bulky tool outputs from *old* tool turns are replaced — in
//! the provider-bound `ChatMessage` projection only — by a compact,
//! deterministic stub that names the tool, the original size, and (when the
//! result is durable) the `result_ref` the model can pass to `result_read` to
//! read it again. Recent tool turns and small outputs stay verbatim.
//!
//! # What this never does
//!
//! * It never touches persistence. The durable transcript, tool-result
//!   artifacts, and `ToolResultReferenceEnvelope`s are untouched; masking is
//!   recomputed from the full history on every request ("LLM data is never
//!   deleted").
//! * It never breaks provider tool pairing. Only the `content` of a
//!   `Role::Tool` message is rewritten; its `tool_call_id`, `name`, and the
//!   assistant tool-call message it answers stay in place, so every call id
//!   still has exactly one matching result.
//!
//! # KV-cache stability invariant
//!
//! Providers cache the longest byte-identical prompt prefix. A masking pass
//! that slid its boundary forward by one message per turn would rewrite a
//! message just behind the tail on every request and bust the cache from
//! there on, every turn. Two rules prevent that:
//!
//! 1. **Monotonic trigger.** The watermark is compared against the estimate of
//!    the *unmasked* conversation, never the masked one. For an append-only
//!    transcript that estimate only grows, so once masking engages it stays
//!    engaged — it cannot flap on/off as masking shrinks the prompt.
//! 2. **Quantized boundary.** The number of masked tool turns is
//!    `floor((tool_turns - keep_recent) / chunk) * chunk`. It advances in
//!    steps of `chunk` tool turns, so between steps the masked projection of
//!    turn `t + 1` starts with the masked projection of turn `t`
//!    byte-for-byte (append-only growth). The cached prefix is invalidated at
//!    most once per `chunk` tool turns, and only from the old boundary on.
//!
//! The stub text is a pure function of the original content's size, the tool
//! name, and the `result_ref`, so re-masking the same message on a later
//! request reproduces the same bytes.

use std::collections::HashMap;

use ironclaw_llm::{ChatMessage, Role};
use ironclaw_loop_contracts::PromptContextTokenBudget;

use crate::{
    HostManagedModelMessage, HostManagedModelMessageRole, HostManagedToolResultContent,
    token_estimator::estimate_tokens_from_chars,
};

/// Env switch for observation masking. Enabled by default; `off`, `0`, or
/// `false` disables it (the provider-bound projection is then byte-identical to
/// the pre-feature path).
pub(crate) const REBORN_OBSERVATION_MASKING_ENV: &str = "REBORN_OBSERVATION_MASKING";

/// Tuning for the observation-masking pass. Defaults are the shipped policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObservationMaskingConfig {
    pub(crate) enabled: bool,
    /// Masking engages when the unmasked prompt estimate reaches this
    /// percentage of the model context window.
    pub(crate) trigger_percent_of_context_window: u8,
    /// The most recent tool turns (assistant tool-call message plus its
    /// results) that always stay verbatim.
    pub(crate) keep_recent_tool_turns: usize,
    /// Tool outputs at or below this many bytes are never masked.
    pub(crate) min_masked_bytes: usize,
    /// The masked-turn boundary advances in steps of this many tool turns.
    /// See the module-level cache-stability invariant.
    pub(crate) boundary_chunk_tool_turns: usize,
}

impl ObservationMaskingConfig {
    pub(crate) const DEFAULT_TRIGGER_PERCENT_OF_CONTEXT_WINDOW: u8 = 55;
    pub(crate) const DEFAULT_KEEP_RECENT_TOOL_TURNS: usize = 4;
    pub(crate) const DEFAULT_MIN_MASKED_BYTES: usize = 2048;
    pub(crate) const DEFAULT_BOUNDARY_CHUNK_TOOL_TURNS: usize = 4;

    pub(crate) const fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::enabled_defaults()
        }
    }

    const fn enabled_defaults() -> Self {
        Self {
            enabled: true,
            trigger_percent_of_context_window: Self::DEFAULT_TRIGGER_PERCENT_OF_CONTEXT_WINDOW,
            keep_recent_tool_turns: Self::DEFAULT_KEEP_RECENT_TOOL_TURNS,
            min_masked_bytes: Self::DEFAULT_MIN_MASKED_BYTES,
            boundary_chunk_tool_turns: Self::DEFAULT_BOUNDARY_CHUNK_TOOL_TURNS,
        }
    }

    /// Shipped defaults with the [`REBORN_OBSERVATION_MASKING_ENV`] kill switch
    /// applied.
    pub(crate) fn from_env() -> Self {
        Self::from_raw_env(std::env::var(REBORN_OBSERVATION_MASKING_ENV).ok())
    }

    /// Pure resolution from a raw env value, so the default-on contract is
    /// testable without mutating process env.
    fn from_raw_env(raw: Option<impl AsRef<str>>) -> Self {
        let disabled = raw.is_some_and(|value| {
            let value = value.as_ref().trim();
            value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("0")
                || value.eq_ignore_ascii_case("false")
        });
        if disabled {
            Self::disabled()
        } else {
            Self::enabled_defaults()
        }
    }

    fn watermark_tokens(&self, context_window_tokens: u64) -> u64 {
        context_window_tokens.saturating_mul(u64::from(self.trigger_percent_of_context_window))
            / 100
    }
}

impl Default for ObservationMaskingConfig {
    fn default() -> Self {
        Self::enabled_defaults()
    }
}

/// Durable result refs by provider call id, captured from the host messages
/// before they are converted (conversion drops the envelope).
pub(super) fn tool_result_refs_by_call_id(
    messages: &[HostManagedModelMessage],
) -> HashMap<String, String> {
    messages
        .iter()
        .filter(|message| message.role == HostManagedModelMessageRole::ToolResult)
        .filter_map(|message| {
            let call = message.tool_result_provider_call.as_ref()?;
            match message.tool_result_content.as_ref()? {
                HostManagedToolResultContent::Reference { envelope } => {
                    Some((call.provider_call_id.clone(), envelope.result_ref.clone()))
                }
                HostManagedToolResultContent::Resolved { .. } => None,
            }
        })
        .collect()
}

/// What a masking pass did. Carries sizes and counts only — never content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ObservationMaskingReport {
    pub(super) triggered: bool,
    pub(super) estimated_prompt_tokens: u64,
    pub(super) watermark_tokens: u64,
    pub(super) masked_tool_turns: usize,
    pub(super) masked_results: usize,
    pub(super) bytes_saved: u64,
}

/// Mask bulky old tool outputs in place. `context_window_tokens` falls back to
/// the default prompt budget limit when the provider did not report one — the
/// same fallback the prompt budget uses.
pub(super) fn mask_old_tool_observations(
    messages: &mut [ChatMessage],
    result_refs: &HashMap<String, String>,
    context_window_tokens: Option<u64>,
    config: &ObservationMaskingConfig,
) -> ObservationMaskingReport {
    if !config.enabled {
        return ObservationMaskingReport::default();
    }
    let context_window_tokens = context_window_tokens
        .filter(|tokens| *tokens > 0)
        .unwrap_or(PromptContextTokenBudget::DEFAULT_CONTEXT_LIMIT_TOKENS);
    let watermark_tokens = config.watermark_tokens(context_window_tokens);
    // Rule 1: compare the UNMASKED estimate (monotonic for append-only growth).
    let estimated_prompt_tokens = estimate_prompt_tokens(messages);
    let mut report = ObservationMaskingReport {
        triggered: estimated_prompt_tokens >= watermark_tokens,
        estimated_prompt_tokens,
        watermark_tokens,
        ..ObservationMaskingReport::default()
    };
    if !report.triggered {
        return report;
    }

    let tool_turn_starts = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_tool_turn_start(message))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    // Rule 2: quantize the boundary so it moves in `chunk`-sized steps.
    let chunk = config.boundary_chunk_tool_turns.max(1);
    let eligible = tool_turn_starts
        .len()
        .saturating_sub(config.keep_recent_tool_turns);
    let masked_tool_turns = eligible / chunk * chunk;
    if masked_tool_turns == 0 {
        return report;
    }
    let boundary = tool_turn_starts
        .get(masked_tool_turns)
        .copied()
        .unwrap_or(messages.len());
    report.masked_tool_turns = masked_tool_turns;

    for message in messages.iter_mut().take(boundary) {
        if message.role != Role::Tool || message.content.len() <= config.min_masked_bytes {
            continue;
        }
        let original_bytes = message.content.len();
        let stub = masked_tool_output_stub(
            &message.content,
            message.name.as_deref(),
            message
                .tool_call_id
                .as_ref()
                .and_then(|call_id| result_refs.get(call_id))
                .map(String::as_str),
        );
        let saved = original_bytes.saturating_sub(stub.len());
        message.content = stub;
        // The stub is plain text; structured page controls no longer apply.
        message.tool_result_structured_json_view = false;
        report.masked_results = report.masked_results.saturating_add(1);
        report.bytes_saved = report
            .bytes_saved
            .saturating_add(u64::try_from(saved).unwrap_or(u64::MAX));
    }
    report
}

fn is_tool_turn_start(message: &ChatMessage) -> bool {
    message.role == Role::Assistant
        && message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
}

fn estimate_prompt_tokens(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .map(|message| {
            let content = estimate_tokens_from_chars(&message.content).as_u64();
            let calls = message.tool_calls.as_ref().map_or(0, |calls| {
                calls
                    .iter()
                    .map(|call| {
                        let argument_bytes = call.arguments.to_string().len();
                        u64::try_from(argument_bytes.saturating_add(call.name.len()))
                            .unwrap_or(u64::MAX)
                            .div_ceil(crate::CHARS_PER_TOKEN_DEFAULT)
                    })
                    .fold(0_u64, u64::saturating_add)
            });
            content.saturating_add(calls)
        })
        .fold(0_u64, u64::saturating_add)
}

/// Deterministic stub: a pure function of the original size, tool name, and
/// durable ref, so the same message masks to the same bytes on every request.
fn masked_tool_output_stub(
    original: &str,
    tool_name: Option<&str>,
    result_ref: Option<&str>,
) -> String {
    let bytes = original.len();
    let lines = original.lines().count();
    let tool = tool_name.unwrap_or("unknown");
    match result_ref {
        Some(result_ref) => format!(
            "[Cleared old tool output ({bytes} bytes, {lines} lines, tool=\"{tool}\") — call result_read(result_ref=\"{result_ref}\") to inspect]"
        ),
        None => {
            format!("[Cleared old tool output ({bytes} bytes, {lines} lines, tool=\"{tool}\")]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_llm::ToolCall;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
            reasoning: None,
            signature: None,
            arguments_parse_error: None,
        }
    }

    /// One tool turn: the assistant call message followed by its result.
    fn tool_turn(index: usize, output: &str) -> Vec<ChatMessage> {
        let id = format!("call_{index}");
        let name = format!("tool_{index}");
        vec![
            ChatMessage::assistant_with_tool_calls(None, vec![call(&id, &name)]),
            ChatMessage::tool_result(id, name, output.to_string()),
        ]
    }

    fn conversation(turns: usize, output: &str) -> Vec<ChatMessage> {
        let mut messages = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("please investigate"),
        ];
        for index in 0..turns {
            messages.extend(tool_turn(index, output));
        }
        messages
    }

    fn refs(turns: usize) -> HashMap<String, String> {
        (0..turns)
            .map(|index| {
                (
                    format!("call_{index}"),
                    format!("result:tool-output_{index}.1"),
                )
            })
            .collect()
    }

    fn bulky() -> String {
        "line of tool output\n".repeat(200)
    }

    /// Provider-wire bytes per message: what the prompt cache actually keys on.
    fn wire(messages: &[ChatMessage]) -> Vec<String> {
        messages
            .iter()
            .map(|message| serde_json::to_string(message).expect("serialize message"))
            .collect()
    }

    fn tool_contents(messages: &[ChatMessage]) -> Vec<&str> {
        messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| message.content.as_str())
            .collect()
    }

    #[test]
    fn env_switch_defaults_on_and_disables_explicitly() {
        assert!(ObservationMaskingConfig::from_raw_env(None::<&str>).enabled);
        assert!(ObservationMaskingConfig::from_raw_env(Some("")).enabled);
        assert!(ObservationMaskingConfig::from_raw_env(Some("on")).enabled);
        for raw in ["off", "OFF", " 0 ", "false"] {
            assert!(!ObservationMaskingConfig::from_raw_env(Some(raw)).enabled);
        }
        assert_eq!(
            ObservationMaskingConfig::default(),
            ObservationMaskingConfig {
                enabled: true,
                trigger_percent_of_context_window: 55,
                keep_recent_tool_turns: 4,
                min_masked_bytes: 2048,
                boundary_chunk_tool_turns: 4,
            }
        );
    }

    #[test]
    fn below_watermark_leaves_every_tool_result_untouched() {
        let mut messages = conversation(12, &bulky());
        let before = messages.clone();
        let estimated = estimate_prompt_tokens(&messages);
        // A context window where the transcript sits just under 55%.
        let window = estimated * 100 / 55 + 100;

        let report = mask_old_tool_observations(
            &mut messages,
            &refs(12),
            Some(window),
            &ObservationMaskingConfig::default(),
        );

        assert!(!report.triggered);
        assert_eq!(report.masked_results, 0);
        assert_eq!(wire(&messages), wire(&before));
    }

    #[test]
    fn disabled_config_never_masks_even_far_over_watermark() {
        let mut messages = conversation(12, &bulky());
        let before = messages.clone();

        let report = mask_old_tool_observations(
            &mut messages,
            &refs(12),
            Some(100),
            &ObservationMaskingConfig::disabled(),
        );

        assert_eq!(report, ObservationMaskingReport::default());
        assert_eq!(wire(&messages), wire(&before));
    }

    #[test]
    fn above_watermark_masks_old_bulky_results_and_keeps_pairing() {
        let small = "small output";
        let mut messages = conversation(0, "");
        for index in 0..10 {
            // Turn 1 is small, everything else bulky.
            let output = if index == 1 {
                small.to_string()
            } else {
                bulky()
            };
            messages.extend(tool_turn(index, &output));
        }
        let mut refs = refs(10);
        refs.remove("call_2"); // a resolved result with no durable ref
        let original_bulky_bytes = bulky().len();

        let report = mask_old_tool_observations(
            &mut messages,
            &refs,
            Some(1_000),
            &ObservationMaskingConfig::default(),
        );

        // 10 tool turns, keep 4 recent → 6 eligible → quantized to 4.
        assert!(report.triggered);
        assert_eq!(report.masked_tool_turns, 4);
        assert_eq!(report.masked_results, 3, "turns 0, 2, 3 (turn 1 is small)");
        let contents = tool_contents(&messages);
        assert_eq!(
            contents[0],
            format!(
                "[Cleared old tool output ({original_bulky_bytes} bytes, 200 lines, tool=\"tool_0\") — call result_read(result_ref=\"result:tool-output_0.1\") to inspect]"
            )
        );
        assert_eq!(contents[1], small, "small results stay verbatim");
        assert_eq!(
            contents[2],
            format!(
                "[Cleared old tool output ({original_bulky_bytes} bytes, 200 lines, tool=\"tool_2\")]"
            ),
            "no durable ref → stub without a result_read pointer"
        );
        assert!(contents[3].starts_with("[Cleared old tool output"));
        // Turns 4..10 (two quantization-held turns plus the four most recent)
        // stay verbatim.
        for content in &contents[4..] {
            assert_eq!(*content, bulky());
        }
        assert!(report.bytes_saved > 0);

        // Pairing: every assistant call id still has exactly one tool result
        // immediately after it, in order.
        for pair in messages[2..].chunks(2) {
            let calls = pair[0].tool_calls.as_ref().expect("assistant tool call");
            assert_eq!(pair[1].role, Role::Tool);
            assert_eq!(pair[1].tool_call_id.as_deref(), Some(calls[0].id.as_str()));
            assert_eq!(pair[1].name.as_deref(), Some(calls[0].name.as_str()));
        }
    }

    #[test]
    fn masked_projection_is_append_only_between_boundary_steps() {
        let config = ObservationMaskingConfig::default();
        let window = Some(1_000);
        let project = |turns: usize| {
            let mut messages = conversation(turns, &bulky());
            mask_old_tool_observations(&mut messages, &refs(turns), window, &config);
            messages
        };

        // From 8 tool turns (first step: 4 masked) through 11, the boundary
        // does not move: each request's projection must extend the previous
        // one byte-for-byte, so the provider's cached prefix survives.
        let mut previous = project(8);
        for turns in 9..=11 {
            let next = project(turns);
            assert_eq!(
                wire(&next[..previous.len()]),
                wire(&previous),
                "projection at {turns} tool turns rewrote the cached prefix"
            );
            previous = next;
        }

        // At 12 turns the boundary steps once (4 → 8 masked turns): the prefix
        // up to the OLD boundary is still identical; only later results change.
        let stepped = project(12);
        let old_boundary = 2 + 4 * 2; // system + user + 4 tool turns
        assert_eq!(
            wire(&stepped[..old_boundary]),
            wire(&previous[..old_boundary])
        );
        assert_ne!(wire(&stepped[..previous.len()]), wire(&previous));

        // Re-masking the same history reproduces identical bytes.
        assert_eq!(wire(&project(12)), wire(&stepped));
    }

    #[test]
    fn trigger_reads_the_unmasked_estimate_so_masking_never_flaps_off() {
        let config = ObservationMaskingConfig::default();
        let mut messages = conversation(12, &bulky());
        let unmasked_estimate = estimate_prompt_tokens(&messages);
        let window = unmasked_estimate * 100 / 55; // just over the watermark
        mask_old_tool_observations(&mut messages, &refs(12), Some(window), &config);
        let masked_estimate = estimate_prompt_tokens(&messages);
        assert!(
            masked_estimate < config.watermark_tokens(window),
            "masking must shrink the prompt below the watermark for this test to bite"
        );

        // The next request replays the full unmasked history plus one more
        // turn; the pass must still engage and mask the same prefix.
        let mut next = conversation(13, &bulky());
        let report = mask_old_tool_observations(&mut next, &refs(13), Some(window), &config);
        assert!(report.triggered);
        assert_eq!(wire(&next[..messages.len()]), wire(&messages));
    }

    #[test]
    fn composes_with_repeated_failure_markers_and_small_results() {
        // Collapsed repeated-failure markers are tiny, so they pass through.
        let marker = r#"{"status":"error","summary":"identical error elided"}"#;
        let mut messages = conversation(10, marker);
        let before = messages.clone();
        let report = mask_old_tool_observations(
            &mut messages,
            &refs(10),
            Some(10),
            &ObservationMaskingConfig::default(),
        );
        assert!(report.triggered);
        assert_eq!(report.masked_results, 0);
        assert_eq!(wire(&messages), wire(&before));
    }

    #[test]
    fn unknown_context_window_uses_default_prompt_budget_limit() {
        let mut messages = conversation(10, &bulky());
        let report = mask_old_tool_observations(
            &mut messages,
            &refs(10),
            None,
            &ObservationMaskingConfig::default(),
        );
        assert_eq!(
            report.watermark_tokens,
            PromptContextTokenBudget::DEFAULT_CONTEXT_LIMIT_TOKENS * 55 / 100
        );
        assert!(!report.triggered);
    }
}
