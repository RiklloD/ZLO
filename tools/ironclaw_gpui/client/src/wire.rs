//! Read-only views of gateway responses.
//!
//! Types the gateway owns in `ironclaw_product_contracts` are used as-is. The
//! few responses whose server types live in heavy domain crates
//! (`ironclaw_threads`, `ironclaw_assistant`) or in the web server crate get a
//! narrow view here: only the fields the app renders, unknown fields ignored,
//! unknown enum values mapped to `Other` so a newer gateway never breaks
//! decoding.

use chrono::{DateTime, Utc};
use ironclaw_product_contracts::outbound::{
    CapabilityActivityView, CapabilityDisplayPreviewView, FinalReplyView, GatePromptView,
    ProductProjectionState, ProgressUpdateView,
};
use serde::Deserialize;

/// `GET /session`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub tenant_id: String,
    pub user_id: String,
    #[serde(default)]
    pub session_channel_extension_id: Option<String>,
    #[serde(default)]
    pub capabilities: serde_json::Value,
    #[serde(default)]
    pub features: serde_json::Value,
}

/// One `SessionThreadRecord`, as rendered in the thread list.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl ThreadSummary {
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("New conversation")
    }
}

/// `GET /threads`.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadPage {
    pub threads: Vec<ThreadSummary>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// `POST /threads`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedThread {
    pub thread: ThreadSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    User,
    Assistant,
    System,
    Summary,
    CheckpointReference,
    ToolResultReference,
    CapabilityDisplayPreview,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Accepted,
    Queued,
    Submitted,
    RejectedBusy,
    DeferredBusy,
    Draft,
    Finalized,
    Interrupted,
    Superseded,
    Redacted,
    #[serde(other)]
    Other,
}

/// One durable `ThreadMessageRecord`.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineMessage {
    pub message_id: String,
    pub sequence: u64,
    pub kind: MessageKind,
    pub status: MessageStatus,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub turn_run_id: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

impl TimelineMessage {
    /// Rows a person reads in the transcript. Tool-result references and
    /// checkpoints are bookkeeping; superseded rows were replaced.
    pub fn is_visible(&self) -> bool {
        matches!(
            self.kind,
            MessageKind::User | MessageKind::Assistant | MessageKind::System
        ) && !matches!(self.status, MessageStatus::Superseded)
            && self
                .content
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
    }
}

/// `GET /threads/{id}/timeline`.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelinePage {
    pub thread: ThreadSummary,
    pub messages: Vec<TimelineMessage>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// `POST /channels/{ext}/messages` (`RebornSubmitTurnResponse`).
#[derive(Debug, Clone, Deserialize)]
pub struct SubmitAck {
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notice: Option<String>,
}

/// One SSE frame (`WebChatV2EventFrame`): `{cursor, type, ...payload}`.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamFrame {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(flatten)]
    pub event: StreamEvent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Accepted {
        ack: SubmitAck,
    },
    Running {
        progress: ProgressUpdateView,
    },
    CapabilityProgress {
        progress: ProgressUpdateView,
    },
    CapabilityActivity {
        activity: CapabilityActivityView,
    },
    CapabilityDisplayPreview {
        preview: CapabilityDisplayPreviewView,
    },
    Gate {
        prompt: GatePromptView,
    },
    AuthRequired {
        prompt: serde_json::Value,
    },
    FinalReply {
        reply: FinalReplyView,
    },
    Cancelled {
        response: serde_json::Value,
    },
    Failed {
        run_state: serde_json::Value,
    },
    ProjectionSnapshot {
        state: ProductProjectionState,
    },
    ProjectionUpdate {
        state: ProductProjectionState,
    },
    KeepAlive,
    #[serde(other)]
    Unknown,
}

/// `stream_error` SSE event: a classified, redacted transport error.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamErrorFrame {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}
