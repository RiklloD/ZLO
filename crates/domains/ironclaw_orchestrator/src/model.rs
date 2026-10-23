//! Orchestration record vocabulary: identifiers, closed state sets, and the
//! row shapes the store persists. Times are unix milliseconds.

use std::fmt;

use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, ThreadId, UserId};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;

pub type Millis = i64;

pub fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default() // silent-ok: a clock before 1970 is not a recoverable state
}

/// Opaque, prefixed record identifiers minted by this crate.
macro_rules! record_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn generate() -> Self {
                let hex = uuid::Uuid::new_v4().simple().to_string();
                Self(format!("{}_{}", $prefix, &hex[..16]))
            }

            pub fn new(raw: impl Into<String>) -> Result<Self, OrchestratorError> {
                let raw = raw.into();
                validate_token(stringify!($name), &raw)?;
                Ok(Self(raw))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = OrchestratorError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

record_id!(TaskId, "task");
record_id!(QueueItemId, "q");
record_id!(TransferId, "xfer");
record_id!(RecoveryId, "rec");
record_id!(TurnLogId, "turn");
// Not generated in practice, but the same validated token shape.
record_id!(RuntimeId, "rt");
record_id!(ProfileId, "profile");

fn validate_token(kind: &str, raw: &str) -> Result<(), OrchestratorError> {
    if raw.is_empty() || raw.len() > 200 || raw.chars().any(char::is_control) {
        return Err(OrchestratorError::invalid(format!("invalid {kind}")));
    }
    Ok(())
}

/// Closed string sets persisted as text columns.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(raw: &str) -> Result<Self, OrchestratorError> {
                match raw {
                    $($text => Ok(Self::$variant),)+
                    other => Err(OrchestratorError::Corrupt(format!(
                        "unknown {} value {other:?}", stringify!($name)
                    ))),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(
    /// Delegated task lifecycle. `Waiting` = parked on a usage-limit reset;
    /// `WaitingChildren` = the child's turn ended while its own children run.
    TaskStatus {
        Starting => "starting",
        Running => "running",
        Waiting => "waiting",
        WaitingChildren => "waiting_children",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
        Interrupted => "interrupted",
    }
);

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

/// SQL fragment matching non-terminal task rows.
pub(crate) const TASK_ACTIVE_SQL: &str =
    "status NOT IN ('completed','failed','cancelled','interrupted')";

text_enum!(
    /// How a finished task result reaches its parent.
    Delivery {
        None => "none",
        Pending => "pending",
        Read => "read",
        Delivered => "delivered",
        Skipped => "skipped",
    }
);

text_enum!(TaskRole {
    Implementation => "implementation",
    Research => "research",
    Review => "review",
    Design => "design",
    Test => "test",
    General => "general",
});

impl TaskRole {
    pub const ALL: [TaskRole; 6] = [
        Self::Implementation,
        Self::Research,
        Self::Review,
        Self::Design,
        Self::Test,
        Self::General,
    ];

    pub fn hint(self) -> &'static str {
        match self {
            Self::Implementation => {
                "Make the change end to end, run the relevant checks, and report what changed."
            }
            Self::Research => {
                "Investigate and report findings with sources/evidence. Do not modify files unless asked."
            }
            Self::Review => {
                "Review critically. Report concrete problems with file:line references, severity, and suggested fixes. Do not modify files unless asked."
            }
            Self::Design => {
                "Propose a design with trade-offs and a recommendation. Do not modify files unless asked."
            }
            Self::Test => {
                "Write or run tests as asked and report exact results (commands, pass/fail, failures)."
            }
            Self::General => "Complete the task and report the outcome.",
        }
    }
}

text_enum!(QueueStatus {
    Pending => "pending",
    Sending => "sending",
    Sent => "sent",
    Cancelled => "cancelled",
});

text_enum!(Author {
    User => "user",
    Agent => "agent",
    Schedule => "schedule",
    System => "system",
});

text_enum!(TransferKind {
    Fork => "fork",
    MergeBack => "merge_back",
    SubagentSpawn => "subagent_spawn",
    SubagentResult => "subagent_result",
    RestartNote => "restart_note",
});

text_enum!(TransferStatus {
    Pending => "pending",
    Consumed => "consumed",
});

text_enum!(
    /// `NextMessage` rides along with the next user turn; `Now` is delivered
    /// as its own context turn as soon as the target is idle.
    TransferDelivery {
        NextMessage => "next_message",
        Now => "now",
    }
);

text_enum!(TransferStrategy {
    NativeFork => "native_fork",
    PortableContext => "portable_context",
    DeltaContext => "delta_context",
    PromptOnly => "prompt_only",
    ResultCard => "result_card",
    Note => "note",
});

text_enum!(RecoveryKind {
    UsageLimit => "usage_limit",
    Restart => "restart",
});

text_enum!(RecoveryStatus {
    Armed => "armed",
    Fired => "fired",
    Cancelled => "cancelled",
    Superseded => "superseded",
    Exhausted => "exhausted",
    Expired => "expired",
    Skipped => "skipped",
});

text_enum!(TurnState {
    Idle => "idle",
    Running => "running",
    Error => "error",
});

// `Interrupted`: running when the app quit. Released by Continue, never
// automatically.
text_enum!(HoldReason {
    Stopped => "stopped",
    Cancelled => "cancelled",
    User => "user",
    Interrupted => "interrupted",
});

text_enum!(
    /// `t3_thread send` modes (T3's `t3_thread_send`).
    SendMode {
        Auto => "auto",
        Queue => "queue",
        Steer => "steer",
        Restart => "restart",
    }
);

/// The caller identity a thread belongs to. Runtimes act as this scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadScope {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

/// One orchestrated thread. The transcript stays in `ironclaw_threads` (or the
/// runtime's native session); this row is the engine's view of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRecord {
    pub thread_id: ThreadId,
    pub scope: ThreadScope,
    /// Stage 3 profiles. Nullable until profiles land.
    pub profile_id: Option<ProfileId>,
    pub runtime_id: RuntimeId,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub title: Option<String>,
    /// Session id inside the runtime when it is not the IronClaw thread id.
    pub native_session_id: Option<String>,
    pub turn_state: TurnState,
    pub active_run_id: Option<String>,
    pub turn_started_at: Option<Millis>,
    pub turn_ended_at: Option<Millis>,
    pub last_user_at: Option<Millis>,
    pub last_error: Option<String>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

impl ThreadRecord {
    pub fn new(thread_id: ThreadId, scope: ThreadScope, runtime_id: RuntimeId) -> Self {
        let now = now_ms();
        Self {
            thread_id,
            scope,
            profile_id: None,
            runtime_id,
            provider: None,
            model: None,
            title: None,
            native_session_id: None,
            turn_state: TurnState::Idle,
            active_run_id: None,
            turn_started_at: None,
            turn_ended_at: None,
            last_user_at: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_running(&self) -> bool {
        self.turn_state == TurnState::Running
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: TaskId,
    pub parent_thread: ThreadId,
    pub child_thread: Option<ThreadId>,
    pub root_thread: ThreadId,
    pub depth: u32,
    pub profile_id: Option<ProfileId>,
    pub parent_profile_id: Option<ProfileId>,
    pub title: String,
    pub role: TaskRole,
    pub runtime_id: RuntimeId,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub progress: Option<serde_json::Value>,
    pub usage: Option<crate::runtime::TurnUsage>,
    pub created_at: Millis,
    pub started_at: Option<Millis>,
    pub finished_at: Option<Millis>,
    pub client_request_id: Option<String>,
    pub delivery: Delivery,
    pub delivered_at: Option<Millis>,
    pub cancel_requested: bool,
    pub limit_reset_at: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: QueueItemId,
    pub thread_id: ThreadId,
    pub position: f64,
    pub text: String,
    pub status: QueueStatus,
    pub created_by: Author,
    pub created_at: Millis,
    pub updated_at: Millis,
    pub sent_at: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRecord {
    pub id: TransferId,
    pub kind: TransferKind,
    pub source_thread: ThreadId,
    pub target_thread: Option<ThreadId>,
    pub source_point: Option<serde_json::Value>,
    pub base_point: Option<serde_json::Value>,
    pub strategy: TransferStrategy,
    pub status: TransferStatus,
    pub delivery: TransferDelivery,
    pub handoff: Option<String>,
    pub created_by: Author,
    pub created_at: Millis,
    pub consumed_at: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRecord {
    pub id: RecoveryId,
    pub thread_id: ThreadId,
    pub kind: RecoveryKind,
    /// Usage limit: when the provider lifts the limit. Restart: when the turn was cut.
    pub reset_at: Millis,
    pub status: RecoveryStatus,
    pub detail: serde_json::Value,
    pub attempts: u32,
    pub created_at: Millis,
    pub fired_at: Option<Millis>,
}

/// One finished turn, with its usage when the runtime reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnLogRecord {
    pub id: TurnLogId,
    pub thread_id: ThreadId,
    pub run_id: Option<String>,
    pub status: crate::runtime::TurnEndStatus,
    pub started_at: Option<Millis>,
    pub ended_at: Millis,
    pub usage: Option<crate::runtime::TurnUsage>,
    pub error: Option<String>,
}

/// Engine settings (T3 defaults), persisted per key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub continue_after_restart: bool,
    pub resume_on_usage_limit: bool,
    pub usage_limit_grace_ms: i64,
    pub max_depth: u32,
    pub max_running_per_parent: u32,
    pub handoff_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            continue_after_restart: true,
            resume_on_usage_limit: true,
            usage_limit_grace_ms: 60_000,
            max_depth: 3,
            max_running_per_parent: 8,
            handoff_bytes: 48_000,
        }
    }
}

/// Clip to at most `max` characters, never splitting a code point.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((idx, _)) => {
            let cut = text[..idx].char_indices().last().map_or(0, |(i, _)| i);
            format!("{}…", &text[..cut])
        }
    }
}
