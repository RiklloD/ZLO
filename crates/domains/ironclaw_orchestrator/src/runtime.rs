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
use ironclaw_product_contracts::orchestration::{ThreadAction, ThreadActionOutcome};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::model::{Millis, RuntimeId, ThreadRecord, ThreadScope};
use crate::telemetry::TurnTelemetry;

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
    /// The runtime's agent reads host files itself (CLI agents). Host paths
    /// in its delegation briefs may then reach a sandboxed child as
    /// attachments; a sandboxed parent cannot widen its own reach that way.
    #[serde(default)]
    pub host_files: bool,
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

/// A steer a runtime accepted into a turn whose agent never read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadSteer {
    /// The runtime's own id for this steer (the message uuid it wrote),
    /// unique per steer: the idempotency key of its recovery.
    pub id: String,
    pub text: String,
}

/// What [`TurnEventSink::steers_unread`] could not keep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteerRecovery {
    /// Steers that are not queued (after retries), oldest first.
    pub undelivered: Vec<UnreadSteer>,
}

impl TurnOutcome {
    /// The turn's outcome once steers in `lost` could not be kept for a
    /// later turn: the person must see that their words did not arrive, so
    /// a turn that would have completed ends failed, and the error names
    /// each lost message. Any other end (failed, a Stop, a restart cut) keeps
    /// its status and gains the same note.
    pub fn with_undelivered_steers(mut self, lost: &[UnreadSteer]) -> Self {
        if lost.is_empty() {
            return self;
        }
        let quoted: Vec<String> = lost.iter().map(|s| format!("\"{}\"", s.text)).collect();
        let note = format!(
            "Not delivered: {} sent during this turn could not be saved for a later turn. \
             Send it again: {}",
            if lost.len() == 1 {
                "a message"
            } else {
                "messages"
            },
            quoted.join(", ")
        );
        self.error = Some(match self.error.take() {
            Some(error) => format!("{error}\n{note}"),
            None => note,
        });
        if self.status == TurnEndStatus::Completed {
            self.status = TurnEndStatus::Failed;
        }
        self
    }
}

/// Input for one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnInput {
    pub text: String,
    /// Why the engine starts this turn. User-typed turns reach a runtime here
    /// only on threads that do not run on IronClaw's loop.
    pub origin: TurnOrigin,
    /// Model hint for runtimes that pick per turn.
    pub model: Option<String>,
    /// The thread's active goal (`orch_goals`). Runtimes put it in the
    /// turn's system prompt, or ahead of the prompt when they have none.
    pub goal: Option<String>,
    /// The thread's reasoning effort pick, one of its provider's `efforts`.
    pub effort: Option<String>,
    /// The thread's profile as a CLI agent sees it; `None` without a
    /// profile or a [`ProfileContextSource`].
    pub profile: Option<TurnProfile>,
    /// Host paths the text names that the runtime should hand its agent as
    /// attachments (see [`RuntimeCapabilities::host_files`]). Candidates
    /// only: the runtime checks each one before reading it. Runtimes whose
    /// agent reads the host directly ignore them.
    pub files: Vec<std::path::PathBuf>,
    /// The thread's own git worktree: the turn's working directory, over
    /// the profile folder and the runtime's default (see
    /// [`TurnRuntime::supports_work_dir`]).
    pub work_dir: Option<std::path::PathBuf>,
}

/// A profile's identity, folder and skills for one CLI turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnProfile {
    /// SOUL.md text; empty means none.
    pub soul: String,
    /// The profile's working folder, when it exists on disk.
    pub folder: Option<std::path::PathBuf>,
    pub skills: Option<ProfileSkills>,
}

/// The skills a profile shows, mirrored to disk for CLI agents.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileSkills {
    /// A Claude Code plugin folder: `.claude-plugin/plugin.json` plus
    /// `skills/<name>/SKILL.md` and each skill's other files.
    pub plugin_dir: std::path::PathBuf,
    /// One line per skill (name, description, SKILL.md path), for agents
    /// without a native skill system. Every skill in full; a runtime that
    /// manages its listing renders its own from [`Self::entries`].
    pub index: String,
    /// The skills the index lists, in its order.
    pub entries: Vec<ProfileSkillEntry>,
}

/// One skill of a [`ProfileSkills`] mirror.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileSkillEntry {
    pub name: String,
    /// The frontmatter description; empty when it has none.
    pub description: String,
    /// The mirrored SKILL.md.
    pub skill_md: std::path::PathBuf,
}

/// Builds [`TurnProfile`]s; plugged in by the host, which owns the skill
/// store and the profile folders.
#[async_trait]
pub trait ProfileContextSource: Send + Sync {
    async fn turn_profile(
        &self,
        profile: &crate::profiles::ProfileRecord,
    ) -> std::result::Result<TurnProfile, OrchestratorError>;
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
    /// The user typed it into a thread that runs on this runtime.
    UserPrompt,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    Steered,
    Restarted,
    /// Not delivered, for no reason the engine can act on: the caller keeps
    /// the text (queues it, or reports the refusal).
    Rejected,
    /// The turn is running but cannot take input yet (session not open,
    /// prompt not written): the engine holds the text and steers it once
    /// the runtime reports [`TurnEventSink::turn_ready`] (t3code's "steer
    /// when ready"), or sends it as the next turn if this one ends first.
    NotReady,
    /// The runtime no longer has the turn (it ended, or is closing its
    /// input): the text starts the next turn right away instead of waiting
    /// behind other queued messages.
    TurnEnded,
}

/// The engine's id for one steer, a UUID. A runtime that tracks whether
/// its agent read a steer uses it as the native message id (Claude's
/// `uuid`), reports it in [`TurnEventSink::steer_updated`] and as
/// [`UnreadSteer::id`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SteerId(String);

impl SteerId {
    fn validate(raw: &str) -> Result<(), RuntimeError> {
        if raw.is_empty() || raw.len() > 128 || raw.chars().any(char::is_control) {
            return Err(RuntimeError::Rejected("invalid steer id".into()));
        }
        Ok(())
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, RuntimeError> {
        let raw = raw.into();
        Self::validate(&raw)?;
        Ok(Self(raw))
    }

    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SteerId {
    type Error = RuntimeError;
    fn try_from(value: String) -> Result<Self, RuntimeError> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl std::fmt::Display for SteerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One steer the engine asks a runtime to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerRequest {
    pub id: SteerId,
    pub text: String,
    pub how: SteerHow,
}

/// Whether the agent has read a steer (t3code `steer.updated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerReadState {
    /// Accepted by the runtime, not read by the agent yet.
    Pending,
    /// The agent read it.
    Read,
    /// The runtime dropped it unread; the engine sends it as a later turn.
    Dropped,
    /// Written to the agent, with no word either way whether it was read
    /// (the agent exited, the connection ended, its turn ended first): kept
    /// as a "Steer delivery not confirmed" notice and never sent again,
    /// since replaying words the agent may have read would deliver them
    /// twice.
    Unconfirmed,
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
    /// When the message was written, if the runtime records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Millis>,
    /// The engine's [`SteerId`] when this user row is a steer the runtime
    /// journaled under it: the transcript marks exactly that row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer_id: Option<String>,
}

/// One provider and its models, as `t3_capabilities` shows them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    pub provider: String,
    pub name: String,
    pub authenticated: bool,
    pub current: bool,
    pub models: Vec<String>,
    /// Reasoning efforts a turn may pick; empty: the provider has no knob.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Per-model efforts where they differ by model; a model missing here
    /// takes `efforts`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub model_efforts: std::collections::BTreeMap<String, Vec<String>>,
    /// The LLM provider actually serving the models when `provider` is a
    /// runtime rather than a vendor (IronClaw's loop on the profile's
    /// active provider): clients draw this provider's logo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    /// Which agent program a CLI runtime found, when it can be one of
    /// several: setup offers the next step that program supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<CatalogLaunch>,
}

/// The agent program behind a CLI runtime's catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogLaunch {
    /// The vendor's own CLI (`agy`): it signs in itself.
    Native,
    /// A standalone runtime another app installed (Antigravity's legacy ACP
    /// runtime): it has no sign-in command of its own; installing the
    /// native CLI is the way to sign in.
    Legacy,
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

    /// [`Self::steer`] with the engine's id for this steer. A runtime that
    /// reports read state ([`Self::reports_steer_reads`]) overrides this and
    /// keys its reports by `request.id`; the default forwards to `steer`.
    async fn steer_message(
        &self,
        thread: &ThreadRecord,
        request: &SteerRequest,
    ) -> Result<SteerOutcome, RuntimeError> {
        self.steer(thread, &request.text, request.how).await
    }

    /// The runtime reports [`TurnEventSink::steer_updated`] for its steers:
    /// a steer starts "waiting to be read" and one still pending when its
    /// turn ends is sent again. Off: a delivered steer counts as read.
    fn reports_steer_reads(&self) -> bool {
        false
    }

    /// A steer cancels the running turn's tool calls before the agent reads
    /// it. Automatic input (delegated results, context handoffs) only steers
    /// a runtime that answers false; true, the conservative default, queues
    /// it for the next turn instead.
    fn steer_interrupts_tools(&self) -> bool {
        true
    }

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

    /// The runtime runs a turn in [`TurnInput::work_dir`] when it is set
    /// (worktree isolation needs it; runtimes that cannot are refused).
    fn supports_work_dir(&self) -> bool {
        false
    }

    /// The folder its agents work in when nothing else names one.
    fn default_work_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Providers/models this runtime can run for `scope`.
    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(Vec::new())
    }

    /// How this runtime's sessions shed context.
    fn compaction(&self) -> RuntimeCompaction {
        RuntimeCompaction::default()
    }

    /// Tokens of what the runtime put in `thread`'s prompts besides the
    /// transcript on its latest turn (its system note, the profile's SOUL,
    /// the skills as that turn listed them, the tools it added), for context
    /// reports. `profile` and `goal` are the thread's now, for a runtime
    /// that has no record of a turn. Empty when it adds nothing ZLO can
    /// measure.
    fn injected_context(
        &self,
        _thread: &ThreadId,
        _profile: Option<&TurnProfile>,
        _goal: Option<&str>,
    ) -> crate::context_report::ContextSample {
        crate::context_report::ContextSample::default()
    }

    /// A runtime-native operation on an idle thread (review, revert,
    /// compact). Runtimes without one answer `Unsupported`.
    async fn thread_action(
        &self,
        _thread: &ThreadRecord,
        _action: ThreadAction,
    ) -> Result<ThreadActionOutcome, RuntimeError> {
        Err(RuntimeError::Unsupported("thread actions"))
    }
}

/// A runtime's compaction entry points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeCompaction {
    /// A message that compacts the session when sent as a turn (a CLI's
    /// own slash command).
    pub manual_prompt: Option<&'static str>,
    /// The runtime compacts by itself near the window's limit.
    pub automatic: bool,
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

    /// Steers the runtime accepted into run `run_id` but its agent never read
    /// (dropped, or the turn ended first), oldest first. Reported before that
    /// turn's `turn_finished`; the engine queues each one, keyed by
    /// [`UnreadSteer::id`] so a repeated report never queues it twice, and
    /// returns the ones it could not keep. The runtime must then tell the
    /// person (the turn ends failed, naming them): they are the person's
    /// words. A sink that keeps nothing returns every steer.
    async fn steers_unread(
        &self,
        _thread_id: &ThreadId,
        _run_id: &str,
        steers: Vec<UnreadSteer>,
    ) -> SteerRecovery {
        SteerRecovery {
            undelivered: steers,
        }
    }

    /// The agent read (or the runtime dropped) steer `steer_id` of run
    /// `run_id` (t3code `steer.updated`). A steer still `Pending` when its
    /// turn ends was never read: the engine sends it as the next turn, or,
    /// after a Stop, notes "Steer delivery not confirmed".
    async fn steer_updated(
        &self,
        _thread_id: &ThreadId,
        _run_id: &str,
        _steer_id: &SteerId,
        _state: SteerReadState,
        _at: Millis,
    ) {
    }

    /// The running turn can take steers now (its prompt is written, the
    /// session is open): the engine delivers steers it held after a
    /// [`SteerOutcome::NotReady`]. Runtimes that never answer `NotReady`
    /// need not call it.
    async fn turn_ready(&self, _thread_id: &ThreadId, _run_id: Option<&str>) {}

    /// Latest context/speed/cache figures of a thread; reported after every
    /// model call or turn, whichever the runtime can measure.
    async fn turn_telemetry(&self, _thread_id: &ThreadId, _telemetry: TurnTelemetry, _at: Millis) {}

    /// Whether idle threads should compact their context while the prompt
    /// cache is warm (`Settings::idle_compact_claude`). Off unless the sink
    /// reads a setting.
    async fn idle_compact_enabled(&self) -> bool {
        false
    }
}
