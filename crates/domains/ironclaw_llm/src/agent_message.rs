//! Canonical parts-list message vocabulary (`AgentMessage`).
//!
//! `AgentMessage` is the canonical cleanup of this crate's provider-facing
//! `ChatMessage` shape (docs/internal/design/2026-08-12-unbound-turns.md
//! §4.4): a pure parts list, where `ChatMessage` carries a `content` string
//! plus a `content_parts` overlay, flat tool-call/result side fields, and
//! `reasoning`/`reasoning_details` side fields. It is used in exactly two
//! places — the prepared-context accept door (`ironclaw_threads`) and the
//! terminal assistant output of an unbound run — and is validated at the
//! message-list level (pairing rules make the list, not the single message,
//! the validated unit).
//!
//! Deliberate deviations from the design sketch, resolved during
//! implementation (each mirrors a repo hard rule):
//! - Attachments reuse [`ironclaw_common::AttachmentRef`] instead of minting
//!   an `ArtifactRef` twin (mirror-DTO ban; the threads tier stores exactly
//!   this type beside transcript rows).
//! - Opaque provider reasoning reuses [`ReasoningDetails`] instead of a new
//!   `ReasoningContent` type — the round-trip guarantees (signature-only
//!   blocks survive; providers 400 when prior reasoning is dropped) already
//!   live on that type.
//! - Inline provider image URLs (`ChatMessage` `content_parts`) have no
//!   representation: the vocabulary bans inline bytes and provider URLs —
//!   callers land bytes in the artifact store first and pass refs.

use serde::{Deserialize, Serialize};

use ironclaw_common::AttachmentRef;
use ironclaw_host_api::ids::CapabilityId;

use crate::provider::ReasoningDetails;

/// Byte budget for one `Text` part. Mirrors the transcript snippet ceiling
/// (`LOOP_CONTEXT_SNIPPET_MODEL_CONTENT_MAX_BYTES` in `ironclaw_loop_contracts`,
/// 64 KiB) so the seam introduces no new size behavior.
pub const AGENT_MESSAGE_TEXT_PART_MAX_BYTES: usize = 64 * 1024;

/// Byte budget for one whole message-list request. Mirrors the transcript
/// total-content ceiling (`LOOP_CONTEXT_TOTAL_MODEL_CONTENT_MAX_BYTES`,
/// 256 KiB).
pub const AGENT_MESSAGE_LIST_MAX_BYTES: usize = 256 * 1024;

/// Byte budget for serialized tool-call arguments; the same bound the safety
/// layer enforces on provider-emitted arguments
/// ([`ironclaw_safety::PROVIDER_ARGUMENTS_MAX_BYTES`]).
pub const AGENT_MESSAGE_TOOL_ARGUMENTS_MAX_BYTES: usize =
    ironclaw_safety::PROVIDER_ARGUMENTS_MAX_BYTES;

/// Upper bound on messages per request; mirrors the default transcript
/// context-window message budget (`max_messages = 128` in the default loop
/// context strategy).
pub const AGENT_MESSAGE_LIST_MAX_MESSAGES: usize = 128;

/// One canonical message: a role plus an ordered parts list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub role: AgentMessageRole,
    pub content: Vec<ContentPart>,
}

/// There is deliberately no `System` role: the system prompt is a separate,
/// host-composed field on the prepared-context accept request, so system content can
/// never be smuggled through the message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageRole {
    User,
    Assistant,
    Tool,
}

/// One content part. Validity per role is enforced by
/// [`validate_agent_messages`] (fail-closed, before anything is journaled):
///
/// | Part | User | Assistant | Tool |
/// |---|---|---|---|
/// | `Text` | ✓ | ✓ | ✓ |
/// | `Image` / `File` | ✓ | ✓ | ✗ |
/// | `ToolCall` | ✗ | ✓ | ✗ |
/// | `ToolResult` | ✗ | ✗ | ✓ (exactly one) |
/// | `Reasoning` | ✗ | ✓ | ✗ |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentPart {
    /// Inline text.
    Text {
        text: String,
    },
    /// Durable reference; bytes stay in the attachment/artifact store. Never
    /// inline bytes, never provider URLs.
    Image {
        attachment: AttachmentRef,
    },
    File {
        attachment: AttachmentRef,
    },
    /// Assistant-only: a capability request the model made.
    ToolCall(ToolCallContent),
    /// Tool-only: the outcome paired to a prior `ToolCall`.
    ToolResult(ToolResultContent),
    /// Assistant-only: opaque provider reasoning artifacts that must
    /// round-trip on replay — some providers reject histories that drop them.
    Reasoning {
        reasoning: ReasoningDetails,
    },
}

impl ContentPart {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text { text: value.into() }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::File { .. } => "file",
            Self::ToolCall(_) => "tool_call",
            Self::ToolResult(_) => "tool_result",
            Self::Reasoning { .. } => "reasoning",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallContent {
    /// Provider-shaped call id; pairs this call with its later `ToolResult`.
    pub call_id: String,
    /// Normalized capability identity, never a raw provider tool name.
    pub capability: CapabilityId,
    /// Bounded by [`AGENT_MESSAGE_TOOL_ARGUMENTS_MAX_BYTES`] (serialized).
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultContent {
    /// Must pair with a `ToolCall` earlier in the message list.
    pub call_id: String,
    pub outcome: ToolResultOutcome,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultOutcome {
    Text { text: String },
    Json { value: serde_json::Value },
    Artifacts { attachments: Vec<AttachmentRef> },
}

/// Typed, fail-closed rejection for an invalid message list. Raised at the
/// accept boundary — nothing invalid ever reaches a journal or a provider.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentMessageError {
    #[error("message {index} has no content parts")]
    EmptyMessage { index: usize },
    #[error("message {index} ({role}) may not carry a {part_kind} part")]
    InvalidPartForRole {
        index: usize,
        role: &'static str,
        part_kind: &'static str,
    },
    #[error("tool message {index} must carry exactly one tool_result part, found {found}")]
    ToolMessageResultCount { index: usize, found: usize },
    #[error("tool call id must not be empty (message {index})")]
    EmptyToolCallId { index: usize },
    #[error("duplicate tool call id {call_id:?}")]
    DuplicateToolCallId { call_id: String },
    #[error("tool result {call_id:?} has no earlier tool call to pair with")]
    UnpairedToolResult { call_id: String },
    #[error("tool call {call_id:?} has no later tool result")]
    UnpairedToolCall { call_id: String },
    #[error("text part exceeds {max} bytes (message {index}, {bytes} bytes)")]
    TextPartTooLarge {
        index: usize,
        bytes: usize,
        max: usize,
    },
    #[error("tool arguments exceed {max} bytes (message {index}, {bytes} bytes)")]
    ToolArgumentsTooLarge {
        index: usize,
        bytes: usize,
        max: usize,
    },
    #[error("message list exceeds {max} bytes ({bytes} bytes)")]
    ListTooLarge { bytes: usize, max: usize },
    #[error("message list exceeds {max} messages ({found})")]
    TooManyMessages { found: usize, max: usize },
    #[error("attachment reference is invalid (message {index}): {reason}")]
    InvalidAttachment { index: usize, reason: String },
}

impl AgentMessageRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

fn part_allowed(role: AgentMessageRole, part: &ContentPart) -> bool {
    matches!(
        (role, part),
        (_, ContentPart::Text { .. })
            | (
                AgentMessageRole::User | AgentMessageRole::Assistant,
                ContentPart::Image { .. } | ContentPart::File { .. },
            )
            | (
                AgentMessageRole::Assistant,
                ContentPart::ToolCall(_) | ContentPart::Reasoning { .. },
            )
            | (AgentMessageRole::Tool, ContentPart::ToolResult(_))
    )
}

fn serialized_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

/// Validate a caller-authored message list fail-closed: role×part validity,
/// tool-call/result pairing and ordering, and byte/message bounds.
pub fn validate_agent_messages(messages: &[AgentMessage]) -> Result<(), AgentMessageError> {
    if messages.len() > AGENT_MESSAGE_LIST_MAX_MESSAGES {
        return Err(AgentMessageError::TooManyMessages {
            found: messages.len(),
            max: AGENT_MESSAGE_LIST_MAX_MESSAGES,
        });
    }

    let mut total_bytes = 0usize;
    let mut open_calls: Vec<String> = Vec::new();
    let mut seen_call_ids: Vec<String> = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        if message.content.is_empty() {
            return Err(AgentMessageError::EmptyMessage { index });
        }
        let mut tool_results_in_message = 0usize;
        for part in &message.content {
            if !part_allowed(message.role, part) {
                return Err(AgentMessageError::InvalidPartForRole {
                    index,
                    role: message.role.as_str(),
                    part_kind: part.kind_name(),
                });
            }
            match part {
                ContentPart::Text { text } => {
                    if text.len() > AGENT_MESSAGE_TEXT_PART_MAX_BYTES {
                        return Err(AgentMessageError::TextPartTooLarge {
                            index,
                            bytes: text.len(),
                            max: AGENT_MESSAGE_TEXT_PART_MAX_BYTES,
                        });
                    }
                    total_bytes = total_bytes.saturating_add(text.len());
                }
                ContentPart::Image { attachment } | ContentPart::File { attachment } => {
                    validate_attachment(index, attachment)?;
                    total_bytes = total_bytes.saturating_add(
                        attachment
                            .extracted_text
                            .as_ref()
                            .map(String::len)
                            .unwrap_or(0),
                    );
                }
                ContentPart::ToolCall(call) => {
                    if call.call_id.is_empty() {
                        return Err(AgentMessageError::EmptyToolCallId { index });
                    }
                    if seen_call_ids.iter().any(|seen| seen == &call.call_id) {
                        return Err(AgentMessageError::DuplicateToolCallId {
                            call_id: call.call_id.clone(),
                        });
                    }
                    let arguments_len = serialized_len(&call.arguments);
                    if arguments_len > AGENT_MESSAGE_TOOL_ARGUMENTS_MAX_BYTES {
                        return Err(AgentMessageError::ToolArgumentsTooLarge {
                            index,
                            bytes: arguments_len,
                            max: AGENT_MESSAGE_TOOL_ARGUMENTS_MAX_BYTES,
                        });
                    }
                    total_bytes = total_bytes.saturating_add(arguments_len);
                    seen_call_ids.push(call.call_id.clone());
                    open_calls.push(call.call_id.clone());
                }
                ContentPart::ToolResult(result) => {
                    tool_results_in_message += 1;
                    if result.call_id.is_empty() {
                        return Err(AgentMessageError::EmptyToolCallId { index });
                    }
                    let Some(open_at) = open_calls.iter().position(|open| open == &result.call_id)
                    else {
                        return Err(AgentMessageError::UnpairedToolResult {
                            call_id: result.call_id.clone(),
                        });
                    };
                    open_calls.remove(open_at);
                    let outcome_len = match &result.outcome {
                        ToolResultOutcome::Text { text } => text.len(),
                        ToolResultOutcome::Json { value } => serialized_len(value),
                        ToolResultOutcome::Artifacts { attachments } => {
                            for attachment in attachments {
                                validate_attachment(index, attachment)?;
                            }
                            0
                        }
                    };
                    if outcome_len > AGENT_MESSAGE_TEXT_PART_MAX_BYTES {
                        return Err(AgentMessageError::TextPartTooLarge {
                            index,
                            bytes: outcome_len,
                            max: AGENT_MESSAGE_TEXT_PART_MAX_BYTES,
                        });
                    }
                    total_bytes = total_bytes.saturating_add(outcome_len);
                }
                ContentPart::Reasoning { reasoning } => {
                    total_bytes = total_bytes.saturating_add(serialized_len(
                        &serde_json::to_value(reasoning).unwrap_or(serde_json::Value::Null),
                    ));
                }
            }
        }
        if message.role == AgentMessageRole::Tool && tool_results_in_message != 1 {
            return Err(AgentMessageError::ToolMessageResultCount {
                index,
                found: tool_results_in_message,
            });
        }
    }

    if let Some(call_id) = open_calls.first() {
        return Err(AgentMessageError::UnpairedToolCall {
            call_id: call_id.clone(),
        });
    }
    if total_bytes > AGENT_MESSAGE_LIST_MAX_BYTES {
        return Err(AgentMessageError::ListTooLarge {
            bytes: total_bytes,
            max: AGENT_MESSAGE_LIST_MAX_BYTES,
        });
    }
    Ok(())
}

/// Byte caps for attachment metadata strings. The id is an opaque
/// channel-provided token, the MIME type is bounded by RFC 6838 practice,
/// and filenames/storage keys are single path segments — none of them are
/// content carriers, so the caps only exclude abuse.
const ATTACHMENT_ID_MAX_BYTES: usize = 256;
const ATTACHMENT_MIME_TYPE_MAX_BYTES: usize = 255;
const ATTACHMENT_FILENAME_MAX_BYTES: usize = 512;
const ATTACHMENT_STORAGE_KEY_MAX_BYTES: usize = 1024;

fn validate_attachment(index: usize, attachment: &AttachmentRef) -> Result<(), AgentMessageError> {
    if attachment.id.is_empty() {
        return Err(AgentMessageError::InvalidAttachment {
            index,
            reason: "attachment id must not be empty".to_string(),
        });
    }
    if attachment.id.len() > ATTACHMENT_ID_MAX_BYTES {
        return Err(AgentMessageError::InvalidAttachment {
            index,
            reason: format!("attachment id exceeds {ATTACHMENT_ID_MAX_BYTES} bytes"),
        });
    }
    if attachment.mime_type.is_empty() {
        return Err(AgentMessageError::InvalidAttachment {
            index,
            reason: "attachment mime_type must not be empty".to_string(),
        });
    }
    if attachment.mime_type.len() > ATTACHMENT_MIME_TYPE_MAX_BYTES {
        return Err(AgentMessageError::InvalidAttachment {
            index,
            reason: format!("attachment mime_type exceeds {ATTACHMENT_MIME_TYPE_MAX_BYTES} bytes"),
        });
    }
    if let Some(filename) = &attachment.filename
        && filename.len() > ATTACHMENT_FILENAME_MAX_BYTES
    {
        return Err(AgentMessageError::InvalidAttachment {
            index,
            reason: format!("attachment filename exceeds {ATTACHMENT_FILENAME_MAX_BYTES} bytes"),
        });
    }
    if let Some(storage_key) = &attachment.storage_key
        && storage_key.len() > ATTACHMENT_STORAGE_KEY_MAX_BYTES
    {
        return Err(AgentMessageError::InvalidAttachment {
            index,
            reason: format!(
                "attachment storage_key exceeds {ATTACHMENT_STORAGE_KEY_MAX_BYTES} bytes"
            ),
        });
    }
    if let Some(extracted) = &attachment.extracted_text
        && extracted.len() > AGENT_MESSAGE_TEXT_PART_MAX_BYTES
    {
        return Err(AgentMessageError::TextPartTooLarge {
            index,
            bytes: extracted.len(),
            max: AGENT_MESSAGE_TEXT_PART_MAX_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_common::AttachmentKind;

    fn capability(name: &str) -> CapabilityId {
        CapabilityId::new(name).expect("valid capability id")
    }

    fn user_text(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentPart::text(text)],
        }
    }

    fn attachment(id: &str) -> AttachmentRef {
        AttachmentRef {
            id: id.to_string(),
            kind: AttachmentKind::Document,
            mime_type: "application/pdf".to_string(),
            filename: Some("report.pdf".to_string()),
            size_bytes: Some(2048),
            storage_key: Some("attachments/report.pdf".to_string()),
            extracted_text: None,
        }
    }

    fn paired_tool_history() -> Vec<AgentMessage> {
        vec![
            user_text("look this up"),
            AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![
                    ContentPart::text("checking"),
                    ContentPart::ToolCall(ToolCallContent {
                        call_id: "call-1".to_string(),
                        capability: capability("web.search"),
                        arguments: serde_json::json!({"query": "near.ai"}),
                    }),
                ],
            },
            AgentMessage {
                role: AgentMessageRole::Tool,
                content: vec![ContentPart::ToolResult(ToolResultContent {
                    call_id: "call-1".to_string(),
                    outcome: ToolResultOutcome::Text {
                        text: "200 OK".to_string(),
                    },
                    is_error: false,
                })],
            },
            AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![ContentPart::text("done")],
            },
        ]
    }

    #[test]
    fn valid_histories_pass_validation() {
        validate_agent_messages(&paired_tool_history()).expect("paired history validates");
    }

    #[test]
    fn role_part_matrix_is_enforced() {
        let invalid = [
            (
                AgentMessageRole::User,
                ContentPart::ToolCall(ToolCallContent {
                    call_id: "c".into(),
                    capability: capability("web.search"),
                    arguments: serde_json::json!({}),
                }),
            ),
            (
                AgentMessageRole::User,
                ContentPart::Reasoning {
                    reasoning: ReasoningDetails::from_text("hmm".to_string())
                        .expect("non-empty reasoning"),
                },
            ),
            (
                AgentMessageRole::Tool,
                ContentPart::Image {
                    attachment: attachment("att-1"),
                },
            ),
            (
                AgentMessageRole::Assistant,
                ContentPart::ToolResult(ToolResultContent {
                    call_id: "c".into(),
                    outcome: ToolResultOutcome::Text { text: "x".into() },
                    is_error: false,
                }),
            ),
        ];
        for (role, part) in invalid {
            let message = AgentMessage {
                role,
                content: vec![part],
            };
            let result = validate_agent_messages(std::slice::from_ref(&message));
            assert!(
                matches!(
                    result,
                    Err(AgentMessageError::InvalidPartForRole { .. })
                        | Err(AgentMessageError::ToolMessageResultCount { .. })
                ),
                "role {role:?} with invalid part must be rejected, got {result:?}"
            );
        }
    }

    #[test]
    fn unpaired_tool_results_and_calls_are_rejected() {
        let orphan_result = vec![
            user_text("hi"),
            AgentMessage {
                role: AgentMessageRole::Tool,
                content: vec![ContentPart::ToolResult(ToolResultContent {
                    call_id: "missing".into(),
                    outcome: ToolResultOutcome::Text { text: "x".into() },
                    is_error: false,
                })],
            },
        ];
        assert!(matches!(
            validate_agent_messages(&orphan_result),
            Err(AgentMessageError::UnpairedToolResult { .. })
        ));

        let dangling_call = vec![AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentPart::ToolCall(ToolCallContent {
                call_id: "dangling".into(),
                capability: capability("web.search"),
                arguments: serde_json::json!({}),
            })],
        }];
        assert!(matches!(
            validate_agent_messages(&dangling_call),
            Err(AgentMessageError::UnpairedToolCall { .. })
        ));
    }

    #[test]
    fn duplicate_call_ids_are_rejected() {
        let mut history = paired_tool_history();
        history.push(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentPart::ToolCall(ToolCallContent {
                call_id: "call-1".into(),
                capability: capability("web.search"),
                arguments: serde_json::json!({}),
            })],
        });
        assert!(matches!(
            validate_agent_messages(&history),
            Err(AgentMessageError::DuplicateToolCallId { .. })
        ));
    }

    #[test]
    fn oversized_text_parts_are_rejected() {
        let message = AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentPart::text(
                "x".repeat(AGENT_MESSAGE_TEXT_PART_MAX_BYTES + 1),
            )],
        };
        assert!(matches!(
            validate_agent_messages(std::slice::from_ref(&message)),
            Err(AgentMessageError::TextPartTooLarge { .. })
        ));
    }

    #[test]
    fn serde_shape_is_tagged_and_round_trips() {
        let history = paired_tool_history();
        let json = serde_json::to_value(&history).expect("serializes");
        assert_eq!(json[0]["role"], "user");
        assert_eq!(json[0]["content"][0]["kind"], "text");
        let back: Vec<AgentMessage> = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, history);
    }
}
