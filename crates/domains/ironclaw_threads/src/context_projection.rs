//! Transcript visibility and projection rules shared by the in-memory and
//! filesystem stores: which messages the model may see, how summaries replace
//! ranges, and how history hides redacted content. Pure functions over message
//! and summary lists.

use std::collections::{HashMap, HashSet};

use ironclaw_host_api::ids::ThreadId;
use uuid::Uuid;

use crate::summary_artifacts::{newest_context_barrier, sorted_context_summaries};
use crate::{
    ContextMessage, MessageKind, MessageStatus, SessionThreadError, SummaryArtifact,
    SummaryModelContextPolicy, ThreadMessageId, ThreadMessageRecord,
};

pub(crate) fn generated_thread_id() -> Result<ThreadId, SessionThreadError> {
    ThreadId::new(Uuid::new_v4().to_string())
        .map_err(|error| SessionThreadError::GeneratedThreadId(error.to_string()))
}

pub(crate) fn ensure_draft(message: &ThreadMessageRecord) -> Result<(), SessionThreadError> {
    if message.kind != MessageKind::Assistant || message.status != MessageStatus::Draft {
        return Err(SessionThreadError::MessageNotDraft {
            message_id: message.message_id,
        });
    }
    Ok(())
}

pub(crate) fn ensure_user_accepted(
    message: &ThreadMessageRecord,
    attempted: &'static str,
) -> Result<(), SessionThreadError> {
    if message.kind == MessageKind::User
        && matches!(
            message.status,
            MessageStatus::Accepted | MessageStatus::DeferredBusy | MessageStatus::Queued
        )
    {
        return Ok(());
    }
    Err(SessionThreadError::InvalidMessageTransition {
        message_id: message.message_id,
        from: message.status,
        attempted,
    })
}

fn is_model_visible(status: MessageStatus) -> bool {
    matches!(
        status,
        MessageStatus::Accepted | MessageStatus::Submitted | MessageStatus::Finalized
    )
}

fn is_model_context_visible(message: &ThreadMessageRecord) -> bool {
    is_model_visible(message.status) && message.kind != MessageKind::CapabilityDisplayPreview
}

const REDACTED_SUMMARY_CONTENT: &str = "[redacted]";

pub(crate) fn context_messages_with_summary_replacements(
    messages: &[ThreadMessageRecord],
    summaries: &[SummaryArtifact],
) -> Vec<ContextMessage> {
    if let Some(barrier) = newest_context_barrier(summaries, |summary| {
        !summary_covers_hidden_content(messages, summary)
    }) {
        let mut context = vec![ContextMessage {
            message_id: None,
            summary_id: Some(barrier.summary_id),
            sequence: barrier.end_sequence,
            kind: MessageKind::Summary,
            tool_result_provider_call: None,
            content: barrier.content.clone(),
            image_attachments: Vec::new(),
        }];
        context.extend(
            messages
                .iter()
                .filter(|message| {
                    message.sequence > barrier.end_sequence && is_model_context_visible(message)
                })
                .filter_map(|message| {
                    let content = message.content.clone()?;
                    Some(ContextMessage::from_transcript_message(message, content))
                }),
        );
        return context;
    }

    let replacement_summaries = sorted_context_summaries(summaries, |summary| {
        summary.model_context_policy == Some(SummaryModelContextPolicy::ReplaceRangeWhenSelected)
            && !summary_covers_hidden_content(messages, summary)
    });
    let mut skip_through = 0u64;
    let mut emitted_summaries = HashSet::new();
    let mut context = Vec::new();
    for message in messages
        .iter()
        .filter(|message| is_model_context_visible(message))
    {
        if message.sequence <= skip_through {
            continue;
        }
        if let Some(summary) = replacement_summaries.iter().find(|summary| {
            summary.start_sequence <= message.sequence
                && message.sequence <= summary.end_sequence
                && !emitted_summaries.contains(&summary.summary_id)
        }) {
            context.push(ContextMessage {
                message_id: None,
                summary_id: Some(summary.summary_id),
                sequence: summary.start_sequence,
                kind: MessageKind::Summary,
                tool_result_provider_call: None,
                content: summary.content.clone(),
                image_attachments: Vec::new(),
            });
            emitted_summaries.insert(summary.summary_id);
            skip_through = summary.end_sequence;
            continue;
        }
        if let Some(content) = message.content.clone() {
            context.push(ContextMessage::from_transcript_message(message, content));
        }
    }
    context
}

pub(crate) fn context_messages_by_id(
    messages: &[ThreadMessageRecord],
    message_ids: &[ThreadMessageId],
) -> Vec<ContextMessage> {
    let visible_messages: HashMap<_, _> = messages
        .iter()
        .filter(|message| is_model_context_visible(message))
        .map(|message| (message.message_id, message))
        .collect();
    message_ids
        .iter()
        .filter_map(|message_id| {
            let message = visible_messages.get(message_id)?;
            let content = message.content.clone()?;
            Some(ContextMessage::from_transcript_message(message, content))
        })
        .collect()
}

pub(crate) fn history_messages(messages: &[ThreadMessageRecord]) -> Vec<ThreadMessageRecord> {
    messages.iter().map(history_message).collect()
}

// Deny-by-default projection: every field is listed deliberately so a newly
// added sensitive field does NOT auto-flow into persisted history. Do not
// collapse to `..message.clone()` — `tool_result_provider_call` is dropped
// here precisely because raw runtime/tool payloads must never surface as
// ordinary transcript content (see crate guardrails).
pub(crate) fn history_message(message: &ThreadMessageRecord) -> ThreadMessageRecord {
    ThreadMessageRecord {
        message_id: message.message_id,
        thread_id: message.thread_id.clone(),
        sequence: message.sequence,
        kind: message.kind,
        status: message.status,
        actor_id: message.actor_id.clone(),
        source_binding_id: message.source_binding_id.clone(),
        reply_target_binding_id: message.reply_target_binding_id.clone(),
        turn_id: message.turn_id.clone(),
        turn_run_id: message.turn_run_id.clone(),
        created_at: message.created_at,
        updated_at: message.updated_at,
        tool_result_ref: message.tool_result_ref.clone(),
        tool_result_provider_call: None,
        content: message.content.clone(),
        attachments: message.attachments.clone(),
        redaction_ref: message.redaction_ref.clone(),
    }
}

pub(crate) fn history_summary_artifacts(
    messages: &[ThreadMessageRecord],
    summaries: Vec<SummaryArtifact>,
) -> Vec<SummaryArtifact> {
    summaries
        .into_iter()
        .map(|summary| {
            if summary_covers_redacted_or_deleted_content(messages, &summary) {
                let mut redacted = summary;
                redacted.content = REDACTED_SUMMARY_CONTENT.to_string();
                redacted.model_context_policy = None;
                redacted
            } else {
                summary
            }
        })
        .collect()
}

/// Returns true when a non-model-context-visible message within the summary
/// span could later become model-visible (i.e. it is in a resurfaceable pending
/// state).  Permanently-terminal non-visible messages (RejectedBusy, capability
/// previews) never resurface, so a compaction summary spanning them is safe to
/// apply — blocking it would silently drop a legitimate compacted range.
///
/// Resurfaceable statuses (must still block the summary):
///   Draft | Interrupted | Superseded | Queued | DeferredBusy
/// Permanent non-visible (must NOT block):
///   RejectedBusy (terminal, user must explicitly resend)
///   CapabilityDisplayPreview kind (never model-visible regardless of status)
///
/// Note: Redacted/Deleted keep their blocking role here — they were never
/// model-visible and the separate `summary_covers_redacted_or_deleted_content`
/// guard (used for history display) doesn't cover the context-build path.
fn can_resurface_as_model_visible(message: &ThreadMessageRecord) -> bool {
    matches!(
        message.status,
        MessageStatus::Draft
            | MessageStatus::Interrupted
            | MessageStatus::Superseded
            | MessageStatus::Queued
            | MessageStatus::DeferredBusy
    )
}

fn summary_covers_hidden_content(
    messages: &[ThreadMessageRecord],
    summary: &SummaryArtifact,
) -> bool {
    messages.iter().any(|message| {
        summary.start_sequence <= message.sequence
            && message.sequence <= summary.end_sequence
            && !is_model_context_visible(message)
            && (can_resurface_as_model_visible(message)
                || matches!(
                    message.status,
                    MessageStatus::Redacted | MessageStatus::Deleted
                ))
    })
}

fn summary_covers_redacted_or_deleted_content(
    messages: &[ThreadMessageRecord],
    summary: &SummaryArtifact,
) -> bool {
    messages.iter().any(|message| {
        summary.start_sequence <= message.sequence
            && message.sequence <= summary.end_sequence
            && matches!(
                message.status,
                MessageStatus::Redacted | MessageStatus::Deleted
            )
    })
}
