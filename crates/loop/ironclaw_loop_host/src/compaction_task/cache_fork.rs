//! Cache-forked full compaction.
//!
//! The standalone path serializes the compacted range into a fresh XML blob
//! under the summarizer system prompt, so every compaction pays a fully cold
//! prefill. When the host can supply the live conversation's warm prefix —
//! the exact system prompt and role-preserving context messages the main loop
//! last sent for this thread, cut at the compaction boundary — this path forks
//! that prefix instead and appends the summarization instruction at the tail.
//! The provider can then serve everything up to the tail from its prompt
//! cache. `ModelGatewayBackedSystemInferencePort` already carries the thread
//! id, so the gateway derives the same `prompt_cache_key` as the live turns.
//!
//! The fork is an optimization, never a new trust path:
//!
//! * The durable range is still validated, scanned, and budgeted by
//!   `build_input` before this runs; a range the standalone path would reject
//!   is rejected exactly as before.
//! * Warm context messages must pass the same injection and leak scanners. A
//!   prefix that would need redaction or trips the injection scanner is not
//!   forked (redacting it would change its bytes and defeat the cache anyway);
//!   the standalone path then runs with its own fail-closed handling.
//! * The summary goes through the same output sanitizer, size cap, and
//!   artifact persistence as the standalone path.
//!
//! Fallback to the standalone XML path happens when the warm prefix is
//! unavailable or incomplete, fails scanning, exceeds the fork token budget,
//! or the inference rejects it (`InputTooLarge` / `Failed`). Cancellation and
//! timeouts propagate: the caller's deadline is already spent.

use ironclaw_loop_contracts::{
    SystemInferenceContextRole, SystemInferenceError, SystemInferenceIdentity,
    SystemInferenceRequest, SystemInferenceResponse, SystemPromptSource, SystemTaskKind,
};
use ironclaw_threads::{MessageKind, SessionThreadService};

use super::{
    CACHE_FORK_COMPACTION_PROMPT_ID, CompactionError, CompactionTask, CompactionTaskRequest,
    CompactionWarmPrefix, CompactionWarmPrefixRequest, CompactionWarmPrefixSource,
    InferenceDeadline, ValidatedCompactionMessage, ValidatedCompactionRange, map_inference_error,
    static_system_prompt_id,
};

const CACHE_FORK_INSTRUCTION: &str = include_str!("../../prompts/compaction_cache_fork.md");

pub(super) struct CacheForkedInference {
    pub(super) response: SystemInferenceResponse,
    pub(super) input_bytes: usize,
}

/// Why a fork was not attempted or not used. Logged, never surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheForkSkipReason {
    Unavailable,
    Incomplete,
    ScannerRejected,
    NeedsRedaction,
    OverBudget,
    InferenceRejected,
}

impl CacheForkSkipReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Incomplete => "incomplete",
            Self::ScannerRejected => "scanner_rejected",
            Self::NeedsRedaction => "needs_redaction",
            Self::OverBudget => "over_budget",
            Self::InferenceRejected => "inference_rejected",
        }
    }
}

impl<S> CompactionTask<S>
where
    S: SessionThreadService + ?Sized,
{
    /// Run compaction on the forked warm prefix. `Ok(None)` means "use the
    /// standalone path"; errors are terminal for this compaction.
    pub(super) async fn try_cache_forked_inference(
        &self,
        request: &CompactionTaskRequest,
        range: &ValidatedCompactionRange,
        warm_prefix_source: &dyn CompactionWarmPrefixSource,
        deadline: &InferenceDeadline,
    ) -> Result<Option<CacheForkedInference>, CompactionError> {
        let prefix = warm_prefix_source
            .warm_prefix(&CompactionWarmPrefixRequest {
                thread_id: range.thread_id.clone(),
                scope: range.thread_scope.clone(),
                through_sequence: range.end_sequence,
                covered_message_refs: range
                    .messages
                    .iter()
                    .filter_map(|message| message.source_ref.clone())
                    .collect(),
            })
            .await;
        let prefix = match prefix {
            Some(prefix) => prefix,
            None => return Ok(skip(CacheForkSkipReason::Unavailable)),
        };
        if let Err(reason) = self.admit_warm_prefix(&prefix) {
            return Ok(skip(reason));
        }

        let input_text = format!("{CACHE_FORK_INSTRUCTION}\n{}", self.system_prompt);
        let estimated_tokens = estimated_fork_tokens(&prefix, &input_text);
        if estimated_tokens > self.max_cache_fork_input_tokens {
            tracing::debug!(
                estimated_tokens,
                max_cache_fork_input_tokens = self.max_cache_fork_input_tokens,
                "compaction cache fork exceeds its input budget"
            );
            return Ok(skip(CacheForkSkipReason::OverBudget));
        }
        // The durable range must fit the fork budget too, and its complete
        // bodies are scanned before anything is forked: a range the
        // standalone path would reject as unsafe is rejected here the same
        // way (`?`), whatever the warm prefix holds.
        if !self.durable_range_fits_cache_fork(range) {
            return Ok(skip(CacheForkSkipReason::OverBudget));
        }
        self.sanitizer()
            .sanitize_messages_before_truncation(&range.messages)?;
        let input_bytes = prefix
            .context_messages
            .iter()
            .map(|message| message.content.len())
            .fold(0_usize, usize::saturating_add);
        let context_message_count = prefix.context_messages.len();
        let replays_live_request = prefix.cache_fork.is_some();

        let result = self
            .inference
            .call_system_inference(SystemInferenceRequest {
                task_id: request.task_id,
                identity: SystemInferenceIdentity {
                    task_kind: SystemTaskKind::Compaction,
                    prompt_source: SystemPromptSource::Static {
                        prompt_id: static_system_prompt_id(CACHE_FORK_COMPACTION_PROMPT_ID),
                    },
                    system_prompt: prefix.system_prompt,
                },
                input_text,
                context_messages: prefix.context_messages,
                max_input_tokens: self.max_cache_fork_input_tokens,
                deadline_ms: deadline.remaining_ms()?,
                output_contract: None,
                cache_fork: prefix.cache_fork,
            })
            .await;
        match result {
            Ok(response) => {
                tracing::debug!(
                    context_message_count,
                    estimated_tokens,
                    replays_live_request,
                    cache_read_input_tokens = response
                        .usage
                        .as_ref()
                        .map(|usage| usage.cache_read_input_tokens),
                    "compaction ran on the cache-forked warm prefix"
                );
                Ok(Some(CacheForkedInference {
                    response,
                    input_bytes,
                }))
            }
            Err(error @ (SystemInferenceError::Cancelled | SystemInferenceError::Timeout)) => {
                Err(map_inference_error(error))
            }
            Err(
                error @ (SystemInferenceError::InputTooLarge | SystemInferenceError::Failed { .. }),
            ) => {
                tracing::debug!(%error, "compaction cache fork was rejected by inference");
                Ok(skip(CacheForkSkipReason::InferenceRejected))
            }
        }
    }

    fn admit_warm_prefix(&self, prefix: &CompactionWarmPrefix) -> Result<(), CacheForkSkipReason> {
        if prefix.system_prompt.trim().is_empty() || prefix.context_messages.is_empty() {
            return Err(CacheForkSkipReason::Incomplete);
        }
        // The system prompt is host-assembled instruction text, so only the
        // conversation messages go through the content scanners.
        let messages = prefix
            .context_messages
            .iter()
            .enumerate()
            .map(|(index, message)| ValidatedCompactionMessage {
                sequence: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                kind: warm_message_kind(message.role),
                body: message.content.clone(),
                source_ref: None,
            })
            .collect::<Vec<_>>();
        match self
            .sanitizer()
            .sanitize_messages_before_truncation(&messages)
        {
            Ok(sanitized) if sanitized.redacted_leak_count == 0 => Ok(()),
            Ok(_) => Err(CacheForkSkipReason::NeedsRedaction),
            Err(error) => {
                tracing::debug!(%error, "compaction cache fork warm prefix failed scanning");
                Err(CacheForkSkipReason::ScannerRejected)
            }
        }
    }
}

impl<S> CompactionTask<S>
where
    S: SessionThreadService + ?Sized,
{
    /// Metadata-only bound on the durable range before its bodies are
    /// scanned: the same token budget the fork itself is held to.
    fn durable_range_fits_cache_fork(&self, range: &ValidatedCompactionRange) -> bool {
        let body_bytes = range
            .messages
            .iter()
            .map(|message| message.body.len())
            .fold(0_usize, usize::saturating_add);
        let cap = self
            .max_cache_fork_input_tokens
            .saturating_mul(crate::CHARS_PER_TOKEN_DEFAULT);
        u64::try_from(body_bytes).is_ok_and(|bytes| bytes <= cap)
    }
}

fn skip(reason: CacheForkSkipReason) -> Option<CacheForkedInference> {
    tracing::debug!(
        reason = reason.as_str(),
        "compaction falling back to the standalone summarizer input"
    );
    None
}

fn warm_message_kind(role: SystemInferenceContextRole) -> MessageKind {
    match role {
        SystemInferenceContextRole::User => MessageKind::User,
        SystemInferenceContextRole::Assistant => MessageKind::Assistant,
        SystemInferenceContextRole::Tool => MessageKind::ToolResultReference,
        SystemInferenceContextRole::System => MessageKind::Summary,
    }
}

fn estimated_fork_tokens(prefix: &CompactionWarmPrefix, input_text: &str) -> u64 {
    prefix
        .context_messages
        .iter()
        .map(|message| crate::estimate_tokens_from_chars(&message.content).as_u64())
        .fold(
            crate::estimate_tokens_from_chars(&prefix.system_prompt)
                .as_u64()
                .saturating_add(crate::estimate_tokens_from_chars(input_text).as_u64()),
            u64::saturating_add,
        )
}

#[cfg(test)]
#[path = "cache_fork/tests.rs"]
mod tests;
