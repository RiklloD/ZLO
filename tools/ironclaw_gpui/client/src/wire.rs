//! Read-only views of gateway responses.
//!
//! Types the gateway owns in `ironclaw_product_contracts` are used as-is. The
//! few responses whose server types live in heavy domain crates
//! (`ironclaw_threads`, `ironclaw_assistant`) or in the web server crate get a
//! narrow view here: only the fields the app renders, unknown fields ignored,
//! unknown enum values mapped to `Other` so a newer gateway never breaks
//! decoding.

use chrono::{DateTime, Utc};
use ironclaw_extension_contracts::state::LifecyclePublicState;
use ironclaw_product_contracts::outbound::{
    CapabilityActivityView, CapabilityDisplayPreviewView, FinalReplyView, GatePromptView,
    ProductProjectionState, ProgressUpdateView,
};
use ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef;
use ironclaw_product_contracts::product_wire::{
    RebornExtensionOnboardingPayload, RebornExtensionSurface, SettingsToolPermissionState,
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
    #[serde(default)]
    pub attachments: AttachmentLimits,
}

/// `session.attachments`: what the composer may attach. The server
/// re-validates every upload; these limits only fail fast in the UI.
#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentLimits {
    /// MIME types and `.ext` suffixes, each MIME followed by its extensions.
    #[serde(default)]
    pub accept: Vec<String>,
    pub max_count: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for AttachmentLimits {
    /// Mirrors `MAX_INLINE_*` in `ironclaw_assistant`, for an older gateway.
    fn default() -> Self {
        Self {
            accept: Vec::new(),
            max_count: 10,
            max_file_bytes: 10 * 1024 * 1024,
            max_total_bytes: 10 * 1024 * 1024,
        }
    }
}

/// One landed attachment on a timeline message (`AttachmentRef`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AttachmentView {
    pub id: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Set once the bytes landed; without it there is nothing to download.
    #[serde(default)]
    pub storage_key: Option<String>,
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
    #[serde(default)]
    pub attachments: Vec<AttachmentView>,
}

/// Content the web UI sends for a message that is only attachments.
pub const ATTACHMENTS_ONLY_CONTENT: &str = "__ironclaw_attachments_only_v1__";

impl TimelineMessage {
    /// Rows a person reads in the transcript. Tool-result references and
    /// checkpoints are bookkeeping; superseded rows were replaced.
    pub fn is_visible(&self) -> bool {
        matches!(
            self.kind,
            MessageKind::User | MessageKind::Assistant | MessageKind::System
        ) && !matches!(self.status, MessageStatus::Superseded)
            && (!self.attachments.is_empty()
                || self
                    .content
                    .as_deref()
                    .is_some_and(|c| !c.trim().is_empty()))
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

/// `GET /extensions` (`RebornExtensionListResponse` lives in
/// `ironclaw_assistant`, so only the rendered fields are mirrored here).
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionList {
    pub extensions: Vec<InstalledExtension>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstalledExtension {
    pub package_ref: LifecyclePackageRef,
    pub display_name: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    pub installation_state: LifecyclePublicState,
    #[serde(default)]
    pub activation_error: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub onboarding: Option<RebornExtensionOnboardingPayload>,
    #[serde(default)]
    pub surfaces: Vec<RebornExtensionSurface>,
}

impl InstalledExtension {
    /// Channels connect by pairing a chat account rather than by a form.
    pub fn is_channel(&self) -> bool {
        self.surfaces
            .iter()
            .any(|s| matches!(s, RebornExtensionSurface::Channel { .. }))
    }
}

/// The `value` of a `tool.<capability>` entry from `GET /settings/tools`.
/// The entry value is untyped JSON on the wire; this is the shape the
/// gateway writes for tool rows.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolPermission {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub state: SettingsToolPermissionState,
    #[serde(default)]
    pub default_state: Option<SettingsToolPermissionState>,
    #[serde(default)]
    pub locked: bool,
    /// `override` when this tool has its own setting; otherwise the state
    /// follows the global auto-approve switch (`global`) or a hard floor.
    #[serde(default)]
    pub effective_source: Option<String>,
}

impl ToolPermission {
    pub fn has_override(&self) -> bool {
        self.effective_source.as_deref() == Some("override")
    }
}

/// `POST /threads/{id}/commands` (`RebornExecuteProductCommandResponse`, in
/// `ironclaw_assistant`). Exactly one of `result` / `rejection` is set.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandOutcome {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub result: Option<CommandResult>,
    #[serde(default)]
    pub rejection: Option<CommandRejection>,
    #[serde(default)]
    pub effect: Option<CommandEffect>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandResult {
    pub title: String,
    #[serde(default)]
    pub fields: Vec<CommandField>,
    #[serde(default)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommandField {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandRejection {
    pub message: String,
}

/// Client-side effect of a command; new kinds decode as `Other`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandEffect {
    OpenThread {
        thread_id: String,
    },
    #[serde(other)]
    Other,
}

/// A minted channel pairing code (`/extensions/{id}/pairing/mint`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PairingCode {
    pub code: String,
    #[serde(default)]
    pub deep_link: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PairingStatus {
    pub connected: bool,
    #[serde(default)]
    pub pending: Option<PairingCode>,
}

/// `POST /extensions/register-hosted-mcp`.
#[derive(Debug, Clone, Deserialize)]
pub struct HostedMcpRegistered {
    pub message: String,
    pub package_ref: LifecyclePackageRef,
}
