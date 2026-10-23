//! The runtime abstraction the engine runs turns on.
//!
//! A runtime is anything that can hold a conversation and run one turn at a
//! time on it: IronClaw's own agent loop (stage 2), and later the Claude CLI
//! (stream-json), Antigravity (ACP) and Muse drivers (stage 4). The engine
//! never knows which one it talks to; it calls [`TurnRuntime`] to act and
//! hears back through [`TurnEventSink`] when a turn starts and ends.
//!
//! Contract every implementation keeps:
//! - `start_turn` on an idle thread starts a turn and reports it through the
//!   sink (`turn_started`, later exactly one `turn_finished`);
//! - a runtime whose native session dies with the process (CLI children)
//!   must report `TurnEndStatus::Interrupted` with `cut_by_restart = true`
//!   when it can tell, or simply never report; the engine's live-work rows
//!   cover the second case;
//! - errors are sanitized: no secrets, host paths or raw provider bodies.

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use serde::{Deserialize, Serialize};

use crate::model::{Millis, RuntimeId, ThreadRecord, ThreadScope};

/// What a runtime can do natively. The engine adapts to missing features
/// (portable handoffs instead of native forks, interrupt-and-restart instead
/// of in-turn steering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeCapabilities {
    /// `fork_native` branches the runtime's own session.
    pub native_fork: bool,
    /// `steer` injects text into the running turn instead of restarting it.
    pub steer_in_turn: bool,
    /// `turn_finished` carries [`TurnUsage`].
    pub reports_usage: bool,
    /// The runtime itself reports turns a restart cut (`cut_by_restart`)
    /// once it is back, so the engine's live-work rows are not needed as a
    /// second source. CLI runtimes whose sessions die with the process leave
    /// this off.
    #[serde(default)]
    pub reports_restart_cuts: bool,
}

/// Token accounting for one turn (sum over its model calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Model context window, when the runtime knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl TurnUsage {
    pub fn add(&mut self, other: &TurnUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        if other.context_window.is_some() {
            self.context_window = other.context_window;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndStatus {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TurnEndStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }
}

/// How one turn ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub status: TurnEndStatus,
    /// Final assistant text when the runtime has it at hand; the engine
    /// reads history otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Provider usage limit: when it resets. Arms an automatic resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit_resets_at: Option<Millis>,
    /// The turn was cut by a backend restart or crash, not by anyone's
    /// decision. Arms restart continuation.
    #[serde(default)]
    pub cut_by_restart: bool,
}

impl TurnOutcome {
    pub fn completed(final_text: Option<String>) -> Self {
        Self {
            status: TurnEndStatus::Completed,
            final_text,
            error: None,
            usage: None,
            usage_limit_resets_at: None,
            cut_by_restart: false,
        }
    }

    pub fn ended(status: TurnEndStatus, error: Option<String>) -> Self {
        Self {
            status,
            final_text: None,
            error,
            usage: None,
            usage_limit_resets_at: None,
            cut_by_restart: false,
        }
    }
}

/// Input for one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnInput {
    pub text: String,
    /// Why the engine starts this turn; user-typed turns arrive through the
    /// product surface, never through here.
    pub origin: TurnOrigin,
    /// Model hint for runtimes that pick per turn.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrigin {
    DelegatedBrief,
    QueueDrain,
    ResultDelivery,
    ContextHandoff,
    AutoContinue,
    AgentSend,
}

/// Result of `start_turn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAdmission {
    /// A new turn started.
    Started { run_id: Option<String> },
    /// The thread was busy and the runtime folded the text into the running
    /// turn (IronClaw's native busy-thread steering).
    Steered { run_id: Option<String> },
    /// The thread was busy and nothing was delivered.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerHow {
    /// Inject at the next safe point of the running turn.
    Steer,
    /// Stop the running turn and start over with this text.
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerOutcome {
    Steered,
    Restarted,
    Rejected,
}

/// Request to create a thread in a runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateThread {
    pub scope: ThreadScope,
    pub title: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Stable per logical creation; retries return the same thread.
    pub idempotency_key: String,
}

/// A created (or forked) thread.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedThread {
    pub thread_id: ThreadId,
    pub native_session_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRole {
    User,
    Assistant,
    Tool,
    System,
}

/// One transcript message, runtime-neutral.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: HistoryRole,
    pub text: String,
    /// Tool calls as `name(hint)` lines, for handoffs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Scaffolding the user never typed (context handoffs, auto-continue).
    #[serde(default)]
    pub hidden: bool,
}

/// One provider and its models, as `t3_capabilities` shows them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    pub provider: String,
    pub name: String,
    pub authenticated: bool,
    pub current: bool,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("the runtime does not support {0}")]
    Unsupported(&'static str),
    #[error("thread not found in the runtime")]
    ThreadNotFound,
    #[error("the runtime rejected the request: {0}")]
    Rejected(String),
    #[error("the runtime is unavailable: {0}")]
    Unavailable(String),
}

impl RuntimeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "unsupported",
            Self::ThreadNotFound => "thread_not_found",
            Self::Rejected(_) => "runtime_rejected",
            Self::Unavailable(_) => "runtime_unavailable",
        }
    }
}

/// The seam between the engine and whatever executes turns.
#[async_trait]
pub trait TurnRuntime: Send + Sync {
    fn id(&self) -> RuntimeId;

    fn capabilities(&self) -> RuntimeCapabilities;

    async fn create_thread(&self, request: CreateThread) -> Result<CreatedThread, RuntimeError>;

    /// Start a turn on `thread`. Must not start a second concurrent turn.
    async fn start_turn(
        &self,
        thread: &ThreadRecord,
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError>;

    /// Deliver `text` into the running turn (or restart it).
    async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError>;

    /// Stop the running turn, if any. Idempotent.
    async fn cancel(&self, thread: &ThreadRecord, reason: &str) -> Result<(), RuntimeError>;

    /// The thread's transcript, oldest first, at most `limit` messages from
    /// the end.
    async fn history(
        &self,
        thread: &ThreadRecord,
        limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError>;

    /// Branch the runtime's own session (only when `native_fork`).
    async fn fork_native(
        &self,
        _source: &ThreadRecord,
        _request: CreateThread,
    ) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("native fork"))
    }

    /// Providers/models this runtime can run for `scope`.
    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(Vec::new())
    }
}

/// Where runtimes report turn lifecycle. Implemented by the engine.
#[async_trait]
pub trait TurnEventSink: Send + Sync {
    async fn turn_started(&self, thread_id: &ThreadId, run_id: Option<String>, at: Millis);

    async fn turn_finished(
        &self,
        thread_id: &ThreadId,
        run_id: Option<String>,
        outcome: TurnOutcome,
        at: Millis,
    );
}
