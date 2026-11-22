use std::collections::BTreeMap;

use crate::state::{
    CompactionEffectivenessBaseline, IndexedMessageKind, LoopExecutionState, MessageIndexEntry,
};
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_loop_contracts::{CompactionInitiator, LoopRunContext, PromptContextTokenBudget};

/// Decides whether to replace older transcript context with a host-managed summary.
///
/// The strategy is pure policy: it reads durable compaction state and returns
/// either `Skip` or the inclusive end of the prefix the executor should
/// compact. State mutation, transcript reads, inference, persistence, and
/// progress events stay in the executor and host compaction port.
///
/// Normal cuts retain from a user boundary. When one turn exceeds the retained
/// tail, the cut retains from an assistant boundary so that assistant tool
/// calls and all following results stay together. Window eviction may compact
/// through a finalized tool result whose complete call/result pair belongs to
/// the replaced prefix.
pub(crate) trait CompactionStrategy: Send + Sync {
    fn should_compact(
        &self,
        state: &LoopExecutionState,
        ctx: &LoopRunContext,
    ) -> CompactionDecision;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionDecision {
    Skip,
    Trigger {
        drop_through_seq: u64,
        preserve_tail_tokens: u64,
        deadline_ms: u64,
        /// Trigger-kind-specific yardstick for circuit-breaker accounting.
        /// The executor compares the refreshed post-compaction prompt
        /// estimate against it to decide whether the compaction was
        /// effective: threshold-triggered compactions carry the transcript
        /// threshold, forced compactions carry the pre-compaction prompt
        /// estimate.
        effectiveness_baseline: CompactionEffectivenessBaseline,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DefaultCompactionStrategy {
    pub prompt_context_budget: PromptContextTokenBudget,
    pub preserve_tail_tokens: u64,
    pub deadline_ms: u64,
}

impl DefaultCompactionStrategy {
    pub(crate) const DEFAULT_PRESERVE_TAIL_TOKENS: u64 = 20_000;
    pub(crate) const DEFAULT_DEADLINE_MS: u64 = 60_000;

    fn prompt_context_budget(&self, ctx: &LoopRunContext) -> PromptContextTokenBudget {
        ctx.model_context_window_tokens
            .map(|tokens| self.prompt_context_budget.with_context_limit_tokens(tokens))
            .unwrap_or(self.prompt_context_budget)
    }

    /// The retained tail for this run: the configured tail, unless it does
    /// not fit under the visible-transcript threshold (a 32k model under the
    /// 20k default), where half the threshold is kept instead so compaction
    /// still has a prefix to replace rather than turning off.
    pub(super) fn preserve_tail_tokens_for(&self, ctx: &LoopRunContext) -> u64 {
        let threshold = self.prompt_context_budget(ctx).visible_transcript_tokens();
        if self.preserve_tail_tokens < threshold {
            self.preserve_tail_tokens
        } else {
            threshold / 2
        }
    }

    pub(super) fn can_evaluate(&self, state: &LoopExecutionState, ctx: &LoopRunContext) -> bool {
        if state.compaction_prompt.message_index.is_empty() {
            return false;
        }
        // Forced/recovery compactions (context-overflow retry, byte-cap
        // overflow) bypass the circuit breaker and the threshold: they are the
        // loop's only mechanism for shrinking an oversized prompt before a
        // retry, so suppressing them would silently rebuild the same prompt
        // until the retry budget aborts. The breaker only gates automatic
        // threshold-triggered compaction.
        if state.compaction_state.force_compact_on_next_iteration {
            return true;
        }
        let threshold = self.prompt_context_budget(ctx).visible_transcript_tokens();
        if threshold == 0 || threshold <= self.preserve_tail_tokens_for(ctx) {
            return false;
        }
        !state.compaction_state.compaction_circuit_open
            && state.compaction_prompt.observed_prompt_tokens >= threshold
    }

    pub(super) fn trigger_at(
        &self,
        state: &LoopExecutionState,
        drop_through_seq: u64,
        ctx: &LoopRunContext,
    ) -> CompactionDecision {
        let effectiveness_baseline = if state.compaction_state.force_compact_on_next_iteration {
            CompactionEffectivenessBaseline::PreCompactionPromptTokens {
                tokens: state.compaction_prompt.observed_prompt_tokens,
            }
        } else {
            CompactionEffectivenessBaseline::TriggerThresholdTokens {
                tokens: self.prompt_context_budget(ctx).visible_transcript_tokens(),
            }
        };
        CompactionDecision::Trigger {
            drop_through_seq,
            preserve_tail_tokens: self.preserve_tail_tokens_for(ctx),
            deadline_ms: self.deadline_ms,
            effectiveness_baseline,
        }
    }
}

impl Default for DefaultCompactionStrategy {
    fn default() -> Self {
        Self {
            prompt_context_budget: PromptContextTokenBudget::default(),
            preserve_tail_tokens: Self::DEFAULT_PRESERVE_TAIL_TOKENS,
            deadline_ms: Self::DEFAULT_DEADLINE_MS,
        }
    }
}

impl CompactionStrategy for DefaultCompactionStrategy {
    fn should_compact(
        &self,
        state: &LoopExecutionState,
        ctx: &LoopRunContext,
    ) -> CompactionDecision {
        if !self.can_evaluate(state, ctx) {
            return CompactionDecision::Skip;
        }
        let prompt_fingerprint = state.compaction_prompt.fingerprint();
        if state.compaction_state.force_compact_on_next_iteration {
            if state.compaction_state.force_compact_initiator
                == Some(CompactionInitiator::WindowEviction)
            {
                return eligible_window_eviction_boundary(state, prompt_fingerprint, None)
                    .map(|sequence| self.trigger_at(state, sequence, ctx))
                    .unwrap_or(CompactionDecision::Skip);
            }
            return tail_preserving_user_boundary(state, prompt_fingerprint, 0, 0, |_| true)
                .or_else(|| {
                    state
                        .compaction_prompt
                        .message_index
                        .iter()
                        .rev()
                        .find(|entry| is_eligible_user_boundary(entry, state, prompt_fingerprint))
                        .map(|entry| entry.sequence)
                })
                .map(|sequence| self.trigger_at(state, sequence, ctx))
                .unwrap_or(CompactionDecision::Skip);
        }

        tail_preserving_user_boundary(
            state,
            prompt_fingerprint,
            self.preserve_tail_tokens_for(ctx),
            0,
            |_| true,
        )
        .map(|sequence| self.trigger_at(state, sequence, ctx))
        .unwrap_or(CompactionDecision::Skip)
    }
}

pub(super) fn eligible_window_eviction_boundary(
    state: &LoopExecutionState,
    prompt_fingerprint: u64,
    before_sequence: Option<u64>,
) -> Option<u64> {
    let watermark = state.compaction_state.window_eviction.as_ref()?;
    let eligible_kind = matches!(
        watermark.omitted_through_kind,
        ironclaw_loop_contracts::LoopContextCompactionKind::User
            | ironclaw_loop_contracts::LoopContextCompactionKind::ToolResult
    );
    let matches_deferred_boundary = state
        .compaction_state
        .last_deferred
        .is_some_and(|deferred| deferred.prompt_fingerprint == prompt_fingerprint);
    if !eligible_kind || matches_deferred_boundary {
        return None;
    }
    state
        .compaction_prompt
        .message_index
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry.kind,
                IndexedMessageKind::User | IndexedMessageKind::ToolResult
            ) && before_sequence.is_none_or(|before| entry.sequence < before)
                && Some(entry.sequence) > state.compaction_state.last_compacted_through_seq
        })
        .map(|entry| entry.sequence)
        .or_else(|| {
            (Some(watermark.omitted_through_sequence)
                > state.compaction_state.last_compacted_through_seq)
                .then_some(watermark.omitted_through_sequence)
                .filter(|sequence| before_sequence.is_none_or(|before| *sequence < before))
        })
}

pub(super) fn tail_preserving_user_boundary(
    state: &LoopExecutionState,
    prompt_fingerprint: u64,
    preserve_tail_tokens: u64,
    minimum_tail_messages: usize,
    boundary_guard: impl Fn(&MessageIndexEntry) -> bool,
) -> Option<u64> {
    let entries = &state.compaction_prompt.message_index;
    let mut tail_tokens = 0_u64;
    let mut tail_messages = 0_usize;
    let mut threshold_index = None;
    for (index, entry) in entries.iter().enumerate().rev() {
        tail_tokens = tail_tokens.saturating_add(entry.estimated_tokens);
        tail_messages = tail_messages.saturating_add(1);
        if tail_tokens >= preserve_tail_tokens && tail_messages >= minimum_tail_messages {
            threshold_index = Some(index);
            break;
        }
    }

    let threshold_index = threshold_index?;
    let first_retained_index = (threshold_index..entries.len()).find(|index| {
        let entry = &entries[*index];
        matches!(
            entry.kind,
            IndexedMessageKind::User | IndexedMessageKind::Assistant
        ) && boundary_guard(entry)
    })?;
    let dropped_boundary = first_retained_index
        .checked_sub(1)
        .and_then(|index| entries.get(index))?;
    is_eligible_drop_boundary(dropped_boundary, state, prompt_fingerprint)
        .then_some(dropped_boundary.sequence)
}

pub(super) fn is_eligible_user_boundary(
    entry: &MessageIndexEntry,
    state: &LoopExecutionState,
    prompt_fingerprint: u64,
) -> bool {
    entry.kind == IndexedMessageKind::User
        && is_eligible_drop_boundary(entry, state, prompt_fingerprint)
}

fn is_eligible_drop_boundary(
    entry: &MessageIndexEntry,
    state: &LoopExecutionState,
    prompt_fingerprint: u64,
) -> bool {
    let matches_deferred_boundary = state
        .compaction_state
        .last_deferred
        .is_some_and(|watermark| {
            watermark.through_seq == entry.sequence
                && watermark.prompt_fingerprint == prompt_fingerprint
        });
    Some(entry.sequence) > state.compaction_state.last_compacted_through_seq
        && !matches_deferred_boundary
}

/// Proactive compaction trigger evaluated by `PostCapabilityStage` after a
/// capability batch flushes. Inspects per-capability byte accounting on
/// `state.post_capability_state.pending_capability_bytes` and decides whether
/// the batch's results are large enough to force a compact-then-skip-model on
/// the next iteration, returning the `CompactionInitiator` variant that should
/// be emitted in the resulting `LoopProgressEvent::CompactionStarted` event.
///
/// The name `CompactionForceStrategy` distinguishes this from `CompactionStrategy`
/// (which decides when/how to run normal compaction) — this trait specifically
/// decides whether to FORCE a compact-then-skip-model on the next iteration
/// based on capability byte accounting.
///
/// A forced compaction is expensive: it replaces the transcript prefix (which
/// invalidates the provider's prompt-cache prefix) and spends an iteration
/// without a model call. The default policy is therefore
/// [`BudgetFractionStrategy`], which scales with the run's real context
/// window; [`ByteCapStrategy`] is the retired fixed-cap policy.
pub(crate) trait CompactionForceStrategy: Send + Sync {
    fn should_force_compact(
        &self,
        state: &LoopExecutionState,
        ctx: &LoopRunContext,
    ) -> Option<CompactionInitiator>;
}

/// Context-window-relative compaction force strategy — the default
/// post-capability policy.
///
/// Trips when the capability bytes accumulated by the current turn's batch
/// (summed across every capability, because they all land in the same
/// prompt) exceed a fixed fraction of the run's model context window. The
/// window is converted from tokens to bytes at [`Self::BYTES_PER_TOKEN`].
///
/// Scaling with the window is the point: a fixed 32 KB cap force-compacted a
/// 200k–1M-token session after a single ordinary `read_file` (which may return
/// up to 64 KB), discarding the cached prompt prefix on the second turn. Under
/// this policy a 200k-token window tolerates ~160 KB of results per turn and a
/// 32k-token window still trips at ~25 KB.
///
/// When the provider has not reported a window (older persisted run contexts)
/// the policy falls back to [`Self::UNKNOWN_WINDOW_THRESHOLD_BYTES`]. Ordinary
/// threshold compaction (`CompactionStrategy`) keeps guarding total prompt
/// growth either way; this policy only decides when ONE turn's results are
/// big enough to compact before the model even sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetFractionStrategy {
    fraction_percent: u64,
    unknown_window_threshold_bytes: u64,
}

impl BudgetFractionStrategy {
    /// Share of the context window, in percent, one turn's capability
    /// results may occupy before forcing a compaction.
    pub(crate) const DEFAULT_FRACTION_PERCENT: u64 = 20;
    /// Token-to-byte conversion for the window. Four bytes per token is the
    /// usual English/code average; denser text (≈3 bytes/token) makes the
    /// effective share slightly larger, never a hair trigger.
    pub(crate) const BYTES_PER_TOKEN: u64 = 4;
    /// Threshold used when the run context carries no provider-reported
    /// context window.
    pub(crate) const UNKNOWN_WINDOW_THRESHOLD_BYTES: u64 = 256 * 1024;

    /// The per-turn byte threshold for this run.
    pub(crate) fn threshold_bytes(&self, ctx: &LoopRunContext) -> u64 {
        match ctx.model_context_window_tokens.filter(|tokens| *tokens > 0) {
            Some(tokens) => {
                tokens
                    .saturating_mul(Self::BYTES_PER_TOKEN)
                    .saturating_mul(self.fraction_percent)
                    / 100
            }
            None => self.unknown_window_threshold_bytes,
        }
    }
}

impl Default for BudgetFractionStrategy {
    fn default() -> Self {
        Self {
            fraction_percent: Self::DEFAULT_FRACTION_PERCENT,
            unknown_window_threshold_bytes: Self::UNKNOWN_WINDOW_THRESHOLD_BYTES,
        }
    }
}

impl CompactionForceStrategy for BudgetFractionStrategy {
    fn should_force_compact(
        &self,
        state: &LoopExecutionState,
        ctx: &LoopRunContext,
    ) -> Option<CompactionInitiator> {
        let turn_bytes = state
            .post_capability_state
            .pending_capability_bytes
            .values()
            .fold(0_u64, |total, bytes| total.saturating_add(*bytes));
        (turn_bytes > self.threshold_bytes(ctx))
            .then_some(CompactionInitiator::CapabilityResultOverflow)
    }
}

/// Per-capability byte-cap compaction force strategy. Trips compaction when any
/// single capability id has accumulated more than its configured byte cap in
/// `pending_capability_bytes` during the current turn.
///
/// Retired as the default in favour of [`BudgetFractionStrategy`]: its fixed
/// 32 KB fallback is below one ordinary `read_file` result and ignores the
/// model's context window. Kept as an explicit alternative policy.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "retired fixed-cap policy kept as an explicit alternative; \
                  BudgetFractionStrategy is the production default"
    )
)]
#[derive(Debug, Clone)]
pub(crate) struct ByteCapStrategy {
    caps: BTreeMap<CapabilityId, u64>,
    default_cap: u64,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "retired fixed-cap policy kept as an explicit alternative"
    )
)]
impl ByteCapStrategy {
    /// Default cap applied to any capability not explicitly listed.
    pub(crate) const DEFAULT_FALLBACK_CAP_BYTES: u64 = 32_000;

    /// Built-in default caps. Override or extend via `with_cap`.
    pub(crate) fn with_defaults() -> Self {
        let mut caps = BTreeMap::new();
        // spawn_subagent results can carry larger structured payloads.
        caps.insert(
            CapabilityId::new("builtin.spawn_subagent").expect("builtin capability id"), // safety: compile-time constant builtin id, structurally valid by construction
            48_000,
        );
        // http + web_fetch occasionally return large pages/JSON.
        caps.insert(
            CapabilityId::new("builtin.http").expect("builtin capability id"), // safety: compile-time constant builtin id, structurally valid by construction
            32_000,
        );
        caps.insert(
            CapabilityId::new("builtin.web_fetch").expect("builtin capability id"), // safety: compile-time constant builtin id, structurally valid by construction
            32_000,
        );
        Self {
            caps,
            default_cap: Self::DEFAULT_FALLBACK_CAP_BYTES,
        }
    }
}

impl Default for ByteCapStrategy {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl CompactionForceStrategy for ByteCapStrategy {
    fn should_force_compact(
        &self,
        state: &LoopExecutionState,
        _ctx: &LoopRunContext,
    ) -> Option<CompactionInitiator> {
        for (capability_id, bytes) in &state.post_capability_state.pending_capability_bytes {
            let cap = self
                .caps
                .get(capability_id)
                .copied()
                .unwrap_or(self.default_cap);
            if *bytes > cap {
                return Some(CompactionInitiator::CapabilityResultOverflow);
            }
        }
        None
    }
}

#[cfg(test)]
mod test_seams {
    use super::*;

    impl ByteCapStrategy {
        pub(crate) fn with_cap(mut self, capability_id: CapabilityId, cap_bytes: u64) -> Self {
            self.caps.insert(capability_id, cap_bytes);
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        CompactionEffectivenessBaseline, CompactionPromptSnapshot, CompactionStrategyState,
        DeferredCompactionWatermark, LoopExecutionState, MessageIndexEntry,
    };
    use ironclaw_host_api::ids::CapabilityId;
    use ironclaw_loop_contracts::PromptContextTokenBudget;

    #[test]
    fn evaluate_skips_when_message_index_is_empty() {
        let context = crate::test_support::test_run_context("compaction-strategy-empty");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.force_compact_on_next_iteration = true;
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 1,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn evaluate_skips_when_no_eligible_user_message_boundary_exists() {
        let context = crate::test_support::test_run_context("compaction-strategy");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt =
            CompactionPromptSnapshot::from_message_index(vec![MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 100,
            }]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 1,
        };
        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn evaluate_skips_when_below_threshold_with_valid_user_boundary_and_forcing_is_off() {
        let context = crate::test_support::test_run_context("compaction-strategy-below-threshold");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 20,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 20,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 1,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn can_evaluate_skips_when_visible_threshold_equals_preserve_tail() {
        let context = crate::test_support::test_run_context("compaction-strategy-equal-tail");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt =
            CompactionPromptSnapshot::from_message_index(vec![MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 100,
            }]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 90,
            deadline_ms: 1,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn evaluate_triggers_at_latest_user_boundary_outside_tail() {
        let context = crate::test_support::test_run_context("compaction-strategy-trigger");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state = CompactionStrategyState::default();
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 30,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 30,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 30,
            },
            MessageIndexEntry {
                sequence: 4,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 30,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 2,
                preserve_tail_tokens: 60,
                deadline_ms: 7,
                effectiveness_baseline: CompactionEffectivenessBaseline::TriggerThresholdTokens {
                    tokens: 90,
                },
            }
        );
    }

    #[test]
    fn a_window_smaller_than_twice_the_tail_still_compacts_with_a_scaled_tail() {
        // A 32k-class window under the 20k default tail: the configured tail
        // exceeds the threshold, so the tail is scaled to half the threshold
        // instead of disabling compaction.
        let context = crate::test_support::test_run_context("compaction-strategy-small-window");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state = CompactionStrategyState::default();
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(
            [
                (1, IndexedMessageKind::User),
                (2, IndexedMessageKind::Assistant),
                (3, IndexedMessageKind::User),
                (4, IndexedMessageKind::Assistant),
            ]
            .into_iter()
            .map(|(sequence, kind)| MessageIndexEntry {
                sequence,
                kind,
                estimated_tokens: 30,
            })
            .collect(),
        );
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1_000,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 2,
                preserve_tail_tokens: 45,
                deadline_ms: 7,
                effectiveness_baseline: CompactionEffectivenessBaseline::TriggerThresholdTokens {
                    tokens: 90,
                },
            }
        );
    }

    #[test]
    fn evaluate_triggers_when_newest_assistant_block_exceeds_tail_budget() {
        let context = crate::test_support::test_run_context("compaction-strategy-tail-overflow");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state = CompactionStrategyState::default();
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 100,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 1,
                preserve_tail_tokens: 60,
                deadline_ms: 7,
                effectiveness_baseline: CompactionEffectivenessBaseline::TriggerThresholdTokens {
                    tokens: 90,
                },
            }
        );
    }

    #[test]
    fn evaluate_never_retains_tool_result_without_its_assistant_call() {
        let context = crate::test_support::test_run_context("compaction-strategy-tool-pair");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::ToolResult,
                estimated_tokens: 100,
            },
            MessageIndexEntry {
                sequence: 4,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 3,
                preserve_tail_tokens: 60,
                deadline_ms: 7,
                effectiveness_baseline: CompactionEffectivenessBaseline::TriggerThresholdTokens {
                    tokens: 90,
                },
            }
        );
    }

    #[test]
    fn evaluate_skips_when_latest_user_boundary_was_already_compacted() {
        let context = crate::test_support::test_run_context("compaction-strategy-compacted");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.last_compacted_through_seq = Some(3);
        state.compaction_state.force_compact_on_next_iteration = true;
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 4,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 100,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn evaluate_skips_previously_deferred_boundary_when_forced() {
        let context = crate::test_support::test_run_context("compaction-strategy-deferred");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.force_compact_on_next_iteration = true;
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
        ]);
        state.compaction_state.last_deferred = Some(DeferredCompactionWatermark {
            through_seq: 3,
            prompt_fingerprint: state.compaction_prompt.fingerprint(),
        });
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 2,
                preserve_tail_tokens: 1,
                deadline_ms: 7,
                effectiveness_baseline:
                    CompactionEffectivenessBaseline::PreCompactionPromptTokens { tokens: 30 },
            }
        );
    }

    #[test]
    fn evaluate_skips_deferred_boundary_in_threshold_overflow_path() {
        let context =
            crate::test_support::test_run_context("compaction-strategy-deferred-threshold");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 50,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 50,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 50,
            },
            MessageIndexEntry {
                sequence: 4,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 50,
            },
        ]);
        state.compaction_state.last_deferred = Some(DeferredCompactionWatermark {
            through_seq: 3,
            prompt_fingerprint: state.compaction_prompt.fingerprint(),
        });
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 2,
                preserve_tail_tokens: 60,
                deadline_ms: 7,
                effectiveness_baseline: CompactionEffectivenessBaseline::TriggerThresholdTokens {
                    tokens: 90,
                },
            }
        );
    }

    #[test]
    fn evaluate_skips_when_only_deferred_boundary_is_eligible_in_threshold_overflow_path() {
        let context = crate::test_support::test_run_context("compaction-strategy-deferred-skip");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 50,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 50,
            },
        ]);
        state.compaction_state.last_deferred = Some(DeferredCompactionWatermark {
            through_seq: 1,
            prompt_fingerprint: state.compaction_prompt.fingerprint(),
        });
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 60,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn evaluate_retries_deferred_boundary_after_prompt_snapshot_changes() {
        let context = crate::test_support::test_run_context("compaction-strategy-deferred-changed");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.last_deferred = Some(DeferredCompactionWatermark {
            through_seq: 3,
            prompt_fingerprint: 42,
        });
        state.compaction_state.force_compact_on_next_iteration = true;
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 2,
                preserve_tail_tokens: 1,
                deadline_ms: 7,
                effectiveness_baseline:
                    CompactionEffectivenessBaseline::PreCompactionPromptTokens { tokens: 30 },
            }
        );
    }

    #[test]
    fn evaluate_retries_after_transcript_advances_past_deferred_boundary() {
        let context = crate::test_support::test_run_context("compaction-strategy-deferred-newer");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.force_compact_on_next_iteration = true;
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 4,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 5,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
        ]);
        state.compaction_state.last_deferred = Some(DeferredCompactionWatermark {
            through_seq: 3,
            prompt_fingerprint: state.compaction_prompt.fingerprint(),
        });
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 4,
                preserve_tail_tokens: 1,
                deadline_ms: 7,
                effectiveness_baseline:
                    CompactionEffectivenessBaseline::PreCompactionPromptTokens { tokens: 50 },
            }
        );
    }

    #[test]
    fn evaluate_uses_output_budget_when_larger_than_reserve() {
        let context = crate::test_support::test_run_context("compaction-strategy-output-budget");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 40,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 35,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 30),
            preserve_tail_tokens: 1,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 1,
                preserve_tail_tokens: 1,
                deadline_ms: 7,
                effectiveness_baseline: CompactionEffectivenessBaseline::TriggerThresholdTokens {
                    tokens: 70,
                },
            }
        );
    }

    #[test]
    fn tail_preserving_user_boundary_respects_minimum_tail_message_count() {
        let context = crate::test_support::test_run_context("compaction-strategy-min-tail");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 3,
                kind: IndexedMessageKind::User,
                estimated_tokens: 10,
            },
            MessageIndexEntry {
                sequence: 4,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 10,
            },
        ]);

        let boundary = tail_preserving_user_boundary(
            &state,
            state.compaction_prompt.fingerprint(),
            1,
            2,
            |_| true,
        );

        assert_eq!(boundary, Some(2));
    }

    #[test]
    fn evaluate_skips_threshold_trigger_when_circuit_is_open() {
        let context = crate::test_support::test_run_context("compaction-strategy-circuit-open");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.compaction_circuit_open = true;
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 100,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 100,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Skip
        );
    }

    #[test]
    fn evaluate_triggers_forced_compaction_even_when_circuit_is_open() {
        // BUG B1 regression: force_compact_on_next_iteration is how
        // context-overflow recovery and byte-cap overflow request a shrink.
        // An open breaker must not suppress it — only automatic
        // threshold-triggered compaction is gated.
        let context =
            crate::test_support::test_run_context("compaction-strategy-circuit-open-forced");
        let mut state = LoopExecutionState::initial_for_run(&context);
        state.compaction_state.compaction_circuit_open = true;
        state.compaction_state.force_compact_on_next_iteration = true;
        state.compaction_prompt = CompactionPromptSnapshot::from_message_index(vec![
            MessageIndexEntry {
                sequence: 1,
                kind: IndexedMessageKind::User,
                estimated_tokens: 100,
            },
            MessageIndexEntry {
                sequence: 2,
                kind: IndexedMessageKind::Assistant,
                estimated_tokens: 100,
            },
        ]);
        let strategy = DefaultCompactionStrategy {
            prompt_context_budget: PromptContextTokenBudget::new(100, 10, 0),
            preserve_tail_tokens: 1,
            deadline_ms: 7,
        };

        assert_eq!(
            strategy.should_compact(&state, &context),
            CompactionDecision::Trigger {
                drop_through_seq: 1,
                preserve_tail_tokens: 1,
                deadline_ms: 7,
                effectiveness_baseline:
                    CompactionEffectivenessBaseline::PreCompactionPromptTokens { tokens: 200 },
            }
        );
    }

    // --- BudgetFractionStrategy tests ---

    fn budget_fraction_state(
        label: &str,
        window_tokens: Option<u64>,
        entries: &[(&str, u64)],
    ) -> (LoopRunContext, LoopExecutionState) {
        let context = crate::test_support::test_run_context(label)
            .with_model_context_window_tokens(window_tokens);
        let mut state = LoopExecutionState::initial_for_run(&context);
        for (id, bytes) in entries {
            state
                .post_capability_state
                .pending_capability_bytes
                .insert(CapabilityId::new(*id).expect("valid capability"), *bytes);
        }
        (context, state)
    }

    /// Regression: one ordinary 40 KB `read_file` result used to trip the
    /// fixed 32 KB cap and force a cold compaction on turn two.
    #[test]
    fn budget_fraction_strategy_ignores_a_normal_file_read_in_a_large_window() {
        let strategy = BudgetFractionStrategy::default();
        for window in [Some(200_000), Some(1_000_000), None] {
            let (context, state) = budget_fraction_state(
                "budget-fraction-normal-read",
                window,
                &[("builtin.read_file", 40_000)],
            );
            assert_eq!(
                strategy.should_force_compact(&state, &context),
                None,
                "a 40 KB read must not force compaction (window {window:?})"
            );
        }
        // The full 64 KB read_file ceiling also stays under the fraction for a
        // 128k window.
        let (context, state) = budget_fraction_state(
            "budget-fraction-max-read",
            Some(128_000),
            &[("builtin.read_file", 64 * 1024)],
        );
        assert_eq!(strategy.should_force_compact(&state, &context), None);
    }

    #[test]
    fn budget_fraction_strategy_trips_when_turn_bytes_exceed_the_window_fraction() {
        let strategy = BudgetFractionStrategy::default();
        // 200k tokens × 4 bytes × 20% = 160_000 bytes.
        let (context, _) = budget_fraction_state("budget-fraction-threshold", Some(200_000), &[]);
        assert_eq!(strategy.threshold_bytes(&context), 160_000);

        let (context, at_threshold) = budget_fraction_state(
            "budget-fraction-at",
            Some(200_000),
            &[("builtin.http", 160_000)],
        );
        assert_eq!(strategy.should_force_compact(&at_threshold, &context), None);

        let (context, over) = budget_fraction_state(
            "budget-fraction-over",
            Some(200_000),
            &[("builtin.http", 160_001)],
        );
        assert_eq!(
            strategy.should_force_compact(&over, &context),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }

    /// Every result of one batch lands in the same prompt, so the fraction is
    /// measured against the turn's total, not per capability.
    #[test]
    fn budget_fraction_strategy_sums_bytes_across_capabilities_in_the_turn() {
        let strategy = BudgetFractionStrategy::default();
        let (context, state) = budget_fraction_state(
            "budget-fraction-sum",
            Some(200_000),
            &[
                ("builtin.read_file", 60_000),
                ("builtin.grep", 60_000),
                ("builtin.glob", 40_001),
            ],
        );
        assert_eq!(
            strategy.should_force_compact(&state, &context),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }

    #[test]
    fn budget_fraction_strategy_scales_down_for_small_windows() {
        let strategy = BudgetFractionStrategy::default();
        // 32k tokens × 4 × 20% = 25_600 bytes: a 40 KB result is a real
        // fraction of a small window and still forces compaction.
        let (context, state) = budget_fraction_state(
            "budget-fraction-small",
            Some(32_000),
            &[("builtin.read_file", 40_000)],
        );
        assert_eq!(strategy.threshold_bytes(&context), 25_600);
        assert_eq!(
            strategy.should_force_compact(&state, &context),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }

    #[test]
    fn budget_fraction_strategy_falls_back_when_the_window_is_unknown() {
        let strategy = BudgetFractionStrategy::default();
        for window in [None, Some(0)] {
            let (context, under) = budget_fraction_state(
                "budget-fraction-unknown-under",
                window,
                &[(
                    "custom.tool",
                    BudgetFractionStrategy::UNKNOWN_WINDOW_THRESHOLD_BYTES,
                )],
            );
            assert_eq!(
                strategy.threshold_bytes(&context),
                BudgetFractionStrategy::UNKNOWN_WINDOW_THRESHOLD_BYTES
            );
            const { assert!(BudgetFractionStrategy::UNKNOWN_WINDOW_THRESHOLD_BYTES >= 256 * 1024) };
            assert_eq!(strategy.should_force_compact(&under, &context), None);

            let (context, over) = budget_fraction_state(
                "budget-fraction-unknown-over",
                window,
                &[(
                    "custom.tool",
                    BudgetFractionStrategy::UNKNOWN_WINDOW_THRESHOLD_BYTES + 1,
                )],
            );
            assert_eq!(
                strategy.should_force_compact(&over, &context),
                Some(CompactionInitiator::CapabilityResultOverflow)
            );
        }
    }

    #[test]
    fn budget_fraction_strategy_saturates_instead_of_overflowing() {
        let strategy = BudgetFractionStrategy::default();
        let (context, state) = budget_fraction_state(
            "budget-fraction-saturate",
            Some(u64::MAX),
            &[("custom.a", u64::MAX), ("custom.b", u64::MAX)],
        );
        // The window conversion and the byte sum both saturate instead of
        // panicking; a saturated sum still exceeds the saturated fraction.
        assert_eq!(
            strategy.should_force_compact(&state, &context),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }

    // --- ByteCapStrategy tests ---

    #[test]
    fn byte_cap_strategy_trips_when_capability_exceeds_cap() {
        let context = crate::test_support::test_run_context("byte-cap-policy-trips");
        let mut state = LoopExecutionState::initial_for_run(&context);
        let id = CapabilityId::new("builtin.http").expect("valid capability");
        // 32_000 is the cap; 32_001 exceeds it.
        state
            .post_capability_state
            .pending_capability_bytes
            .insert(id, 32_001);

        let strategy = ByteCapStrategy::with_defaults();
        assert_eq!(
            strategy.should_force_compact(&state, &context),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }

    #[test]
    fn byte_cap_strategy_skips_when_under_threshold() {
        let context = crate::test_support::test_run_context("byte-cap-policy-under");
        let mut state = LoopExecutionState::initial_for_run(&context);
        let http_id = CapabilityId::new("builtin.http").expect("valid capability");
        let subagent_id = CapabilityId::new("builtin.spawn_subagent").expect("valid capability");
        // Both under their respective caps.
        state
            .post_capability_state
            .pending_capability_bytes
            .insert(http_id, 31_999);
        state
            .post_capability_state
            .pending_capability_bytes
            .insert(subagent_id, 47_999);

        let strategy = ByteCapStrategy::with_defaults();
        assert_eq!(strategy.should_force_compact(&state, &context), None);
    }

    #[test]
    fn byte_cap_strategy_uses_default_cap_for_unknown_capability() {
        let context = crate::test_support::test_run_context("byte-cap-policy-unknown");
        let mut state = LoopExecutionState::initial_for_run(&context);
        let id = CapabilityId::new("custom.unknown_tool").expect("valid capability");
        // DEFAULT_FALLBACK_CAP_BYTES is 32_000; 32_001 exceeds it.
        state
            .post_capability_state
            .pending_capability_bytes
            .insert(id, ByteCapStrategy::DEFAULT_FALLBACK_CAP_BYTES + 1);

        let strategy = ByteCapStrategy::with_defaults();
        assert_eq!(
            strategy.should_force_compact(&state, &context),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }

    #[test]
    fn byte_cap_strategy_empty_accumulator_returns_none() {
        let context = crate::test_support::test_run_context("byte-cap-policy-empty");
        let state = LoopExecutionState::initial_for_run(&context);
        // pending_capability_bytes is empty by default.
        let strategy = ByteCapStrategy::with_defaults();
        assert_eq!(strategy.should_force_compact(&state, &context), None);
    }

    #[test]
    fn byte_cap_strategy_with_cap_overrides_default_cap() {
        let ctx = crate::test_support::test_run_context("byte-cap-with-cap");
        let mut state = LoopExecutionState::initial_for_run(&ctx);
        let id = CapabilityId::new("custom.large_tool").unwrap();
        state
            .post_capability_state
            .pending_capability_bytes
            .insert(id.clone(), 5_000);
        // Default cap (32_000) would NOT trip at 5_000; custom cap of 4_000 should trip.
        let strategy = ByteCapStrategy::with_defaults().with_cap(id, 4_000);
        assert_eq!(
            strategy.should_force_compact(&state, &ctx),
            Some(CompactionInitiator::CapabilityResultOverflow)
        );
    }
}
