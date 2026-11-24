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
use ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef;
use ironclaw_product_contracts::product_wire::{
    RebornExtensionOnboardingPayload, RebornExtensionSurface, SettingsToolPermissionState,
};
use serde::Deserialize;

use crate::plugins::{Consent, PluginState};

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
    /// JSON text; automation runs carry `{"source":"automation_trigger","trigger_id":…}`.
    #[serde(default)]
    pub metadata_json: Option<String>,
}

impl ThreadSummary {
    /// The automation that started this thread, if one did.
    pub fn automation_trigger_id(&self) -> Option<String> {
        let meta: serde_json::Value = serde_json::from_str(self.metadata_json.as_deref()?).ok()?;
        (meta.get("source")?.as_str()? == "automation_trigger")
            .then(|| meta.get("trigger_id")?.as_str().map(str::to_string))
            .flatten()
    }

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
    /// Set on a user row that steered its run (the engine's steer ledger,
    /// CLI threads): drives "Steer · waiting to be read".
    #[serde(default)]
    pub steer: Option<SteerAnnotation>,
    /// On a run's last row: steers a Stop left unread ("Steer delivery not
    /// confirmed").
    #[serde(default)]
    pub steer_notices: Vec<SteerNoticeView>,
    /// The steer this row is (CLI threads), when the engine knows it.
    #[serde(default)]
    pub steer_id: Option<String>,
    /// An assistant row the run moved on from (text before a tool call or
    /// a later reply): commentary, drawn subdued and folded into the
    /// turn's work. Absent on older gateways and on final answers.
    #[serde(default)]
    pub commentary: bool,
}

/// `RebornTimelineSteerMark`: steer annotations of one message, keyed by
/// its id, beside the page's messages.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TimelineSteerMark {
    pub message_id: String,
    #[serde(default)]
    pub steer: Option<SteerAnnotation>,
    #[serde(default)]
    pub notices: Vec<SteerNoticeView>,
}

/// A steer's read state on its timeline row (`steer`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SteerAnnotation {
    #[serde(default)]
    pub id: String,
    /// `pending`, `read`, `dropped` or `unconfirmed`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub waiting_to_be_read: bool,
}

/// A steer the agent never confirmed reading before Stop (`steer_notices`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SteerNoticeView {
    #[serde(default)]
    pub steer_id: String,
    #[serde(default)]
    pub title: String,
    /// The steer's own words.
    #[serde(default)]
    pub text: String,
    /// The notice as a sentence, with the words quoted.
    #[serde(default)]
    pub content: String,
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
    /// Steer marks for this page's messages (the typed form of each row's
    /// `steer` / `steer_notices`).
    #[serde(default)]
    pub steer_marks: Vec<TimelineSteerMark>,
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
    /// The run a busy answer refers to (steered into, or queued behind).
    #[serde(default)]
    pub active_run_id: Option<String>,
    /// What the message became (`started` / `steered` / `queued` /
    /// `restarted`). Absent from servers older than the field.
    #[serde(default)]
    pub delivery: Option<SubmitDelivery>,
    /// The steer's ledger id, keying its "waiting to be read" state.
    #[serde(default)]
    pub steer_id: Option<String>,
    /// The queue item holding it (a queued message, or a steer waiting for
    /// the turn to take input).
    #[serde(default)]
    pub queued_id: Option<String>,
    /// A steer the turn could not take yet: it goes in once the turn can,
    /// or runs as the next turn if this one ends first.
    #[serde(default)]
    pub waiting_for_turn: bool,
}

/// `RebornSubmitDelivery`: what a submitted message became.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitDelivery {
    Started,
    Steered,
    Queued,
    Restarted,
    #[serde(other)]
    Other,
}

/// How a send landed, read from the ack's typed fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// A new turn runs it (`restarted` counts: the turn started over with it).
    Started,
    /// Part of the running turn. `waiting`: the turn takes it once it can.
    Steered { waiting: bool },
    /// In the thread's queue for a later turn.
    Queued,
    /// The running turn refused it; nothing was kept.
    Refused,
    /// An older gateway's `deferred_busy` without `delivery`: steered or
    /// queued, unknown until the timeline or queue reads it back.
    Unconfirmed,
}

impl SubmitAck {
    /// Where the message went. An older gateway without `delivery` is read
    /// by outcome alone; its `deferred_busy` stays unresolved until read back.
    pub fn landed(&self) -> Landed {
        if self.outcome.as_deref() == Some("rejected_busy") {
            return Landed::Refused;
        }
        match self.delivery {
            Some(SubmitDelivery::Steered) => Landed::Steered {
                waiting: self.waiting_for_turn,
            },
            Some(SubmitDelivery::Queued) => Landed::Queued,
            Some(SubmitDelivery::Started | SubmitDelivery::Restarted) => Landed::Started,
            Some(SubmitDelivery::Other) | None => match self.outcome.as_deref() {
                Some("deferred_busy") => Landed::Unconfirmed,
                _ => Landed::Started,
            },
        }
    }
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
    /// The lifecycle state, plugin states included (`consent_required`,
    /// `tampered`); unknown states decode as `Other`.
    pub installation_state: PluginState,
    /// What the plugin asks for, when it waits for consent or changed
    /// after it (older backends send none).
    #[serde(default)]
    pub consent: Option<Consent>,
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

    pub fn id(&self) -> &str {
        self.package_ref.id.as_str()
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

#[cfg(test)]
mod landed_tests {
    use super::*;

    fn ack(v: serde_json::Value) -> SubmitAck {
        serde_json::from_value(v).expect("ack decodes")
    }

    #[test]
    fn the_typed_delivery_decides_where_a_send_went() {
        let steered = ack(
            serde_json::json!({"outcome": "deferred_busy", "delivery": "steered",
            "steer_id": "s1", "notice": "Queued is not read"}),
        );
        assert_eq!(steered.landed(), Landed::Steered { waiting: false });
        let waiting = ack(
            serde_json::json!({"outcome": "deferred_busy", "delivery": "steered",
            "waiting_for_turn": true, "queued_id": "q1"}),
        );
        assert_eq!(waiting.landed(), Landed::Steered { waiting: true });
        // The notice text never decides it.
        let queued = ack(
            serde_json::json!({"outcome": "deferred_busy", "delivery": "queued",
            "notice": "Sent to the running turn"}),
        );
        assert_eq!(queued.landed(), Landed::Queued);
        let started = ack(serde_json::json!({"outcome": "submitted", "delivery": "started"}));
        assert_eq!(started.landed(), Landed::Started);
        let restarted = ack(serde_json::json!({"outcome": "submitted", "delivery": "restarted"}));
        assert_eq!(restarted.landed(), Landed::Started);
        let refused = ack(serde_json::json!({"outcome": "rejected_busy"}));
        assert_eq!(refused.landed(), Landed::Refused);
    }

    #[test]
    fn an_older_gateway_is_read_by_outcome_alone() {
        let old = ack(serde_json::json!({"outcome": "deferred_busy",
            "notice": "Queued: it runs when the current turn ends"}));
        assert_eq!(old.landed(), Landed::Unconfirmed, "never guessed");
        let old = ack(serde_json::json!({"outcome": "submitted"}));
        assert_eq!(old.landed(), Landed::Started);
    }
}
