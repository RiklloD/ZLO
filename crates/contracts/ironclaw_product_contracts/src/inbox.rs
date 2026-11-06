//! Inbox and goal descriptors and wire DTOs (v2 stage 5, t3code's inbox).
//!
//! The inbox is one list of the caller's threads, already joined on the
//! backend: profile, title, git branch, every engine that ran in the thread
//! (delegated children included) and the lifecycle state. Pin, snooze,
//! settle and visit are durable per-user records in `ironclaw_orchestrator`;
//! the state is derived from them and the thread's live orchestration state.
//!
//! A goal is one persisted objective per thread, shown above the composer.

use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};

/// The section a row belongs to (T3's precedence: snoozed, settled, pinned,
/// working, inbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxState {
    Pinned,
    Inbox,
    Working,
    Snoozed,
    Settled,
}

/// What the row asks of the person, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxStatus {
    /// A running turn waits on the person: a tool approval, a sign-in, or
    /// a question the agent asked through one.
    NeedsInput,
    /// A turn is running.
    Working,
    /// The thread is idle while delegated children run.
    Background,
    /// The provider's usage limit stopped it; resumes at `limit_reset_at`.
    Limited,
    /// A crash or quit cut a turn; Continue finishes it.
    Interrupted,
    Failed,
    /// Paused after Stop.
    Stopped,
    /// A snooze ended (or woke early) and the person has not looked yet.
    Woke,
    /// A turn finished since the person last opened the thread.
    Done,
}

/// One engine that ran in the thread: the parent's own or a child's.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InboxEngine {
    pub runtime_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxRow {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub profile_id: String,
    pub state: InboxState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<InboxStatus>,
    /// Parent first, then children in the order they were delegated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engines: Vec<InboxEngine>,
    /// Unix ms of the latest activity: the row's recency.
    pub last_activity_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reset_at: Option<i64>,
    /// Unix ms the running turn started; set only while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_since: Option<i64>,
    /// The thread a delegated child was created from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    /// Started by an automation (not the person).
    #[serde(default)]
    pub automation: bool,
    /// A generated title is being written; poll until it lands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_pending: bool,
    /// Imported from a terminal session: the source id
    /// (`crate::session_import`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<String>,
    /// The git branch of the thread's own worktree, while its folder exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
}

/// A profile as the inbox shows it: badge, branch, picker order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxProfile {
    pub profile_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxListResponse {
    /// Pinned by pin time, snoozed by wake time, settled newest first,
    /// everything else by recency.
    pub rows: Vec<InboxRow>,
    /// Most recently used first (the New thread picker order).
    pub profiles: Vec<InboxProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_profile_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxAction {
    Pin,
    Unpin,
    /// Until `until` (unix ms).
    Snooze,
    Wake,
    Settle,
    Unsettle,
    /// The person opened the thread: clears Done and Woke.
    Visit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxMutationRequest {
    pub thread_id: String,
    pub action: InboxAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRecord {
    pub objective: String,
    pub status: GoalStatus,
    pub updated_at: i64,
    /// The shell command that verifies the goal; goals stored before checks
    /// existed (and goals without one) have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<GoalCheckRecord>,
}

/// A command that verifies a goal (`/goal <text> --check "<cmd>"`): it runs
/// in the thread's folder after each completed turn; exit 0 meets the goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCheckSpec {
    pub command: String,
    /// Failed runs before the check stops and asks the person (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Per-run time limit in seconds (default 120, at most 600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Where a goal's check stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCheckStatus {
    /// Set, never run.
    NotRun,
    /// A turn finished (or "Run check now"); runs once the thread is idle.
    Pending,
    /// The thread asks first and this command was never approved.
    AwaitingApproval,
    Running,
    /// Exited 0: the goal is met.
    Passed,
    /// Exited non-zero; the failure went back to the agent as a follow-up.
    Failed,
    /// Out of attempts, refused, or unable to run: the person decides.
    NeedsYou,
    /// The person stopped automatic retries.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCheckRecord {
    /// Bumped on every change to the check. "Run check now" names the
    /// revision the person saw, so it never approves a command they did not.
    #[serde(default)]
    pub revision: u64,
    pub command: String,
    pub max_attempts: u32,
    pub timeout_secs: u64,
    pub status: GoalCheckStatus,
    /// Failed runs since the check was set (or restarted).
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// The bounded tail of the last run's output, or why it could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    /// Unix ms the last run ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<i64>,
}

/// A person's action on a goal's check (the strip's buttons).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCheckAction {
    /// Run it now. Counts as approval of the command on an Ask-first
    /// thread, and restarts the attempt count once retries ended. Needs
    /// `check_revision`: the revision of the check the person was shown.
    RunNow,
    StopRetrying,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoalResponse {
    pub thread_id: String,
    /// `None` when the thread has no goal (or it was cleared).
    #[serde(default)]
    pub goal: Option<GoalRecord>,
}

/// Set or replace the goal; a request naming nothing (no objective, status,
/// check or action) clears it. `check` sets (or replaces) the goal's check,
/// `clear_check` removes it, `check_action` acts on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGoalSetRequest {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GoalStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<GoalCheckSpec>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_check: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_action: Option<GoalCheckAction>,
    /// With `check_action: run_now`: the [`GoalCheckRecord::revision`] the
    /// person approved. A check changed since is refused, not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_revision: Option<u64>,
}

impl ThreadGoalSetRequest {
    /// Names nothing to set: the request clears the goal.
    pub fn is_clear(&self) -> bool {
        self.objective.is_none()
            && self.status.is_none()
            && self.check.is_none()
            && !self.clear_check
            && self.check_action.is_none()
    }
}

// Outputs are JSON documents: the success shape named on each descriptor, or
// the orchestrator's correctable error document `{"ok": false, "error",
// "message"}` (HTTP 200), like `crate::profiles`.

/// Output: [`InboxListResponse`].
pub const INBOX_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("inbox.list");

/// Output: [`InboxListResponse`], re-read after the write.
pub const INBOX_MUTATE_COMMAND_ID: &str = "inbox.mutate";
pub const INBOX_MUTATE_COMMAND: ProductSurfaceCommandDescriptor<
    InboxMutationRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(INBOX_MUTATE_COMMAND_ID);

/// Input: `{"thread_id"}`. Output: [`ThreadGoalResponse`].
pub const THREAD_GOAL_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("goals.get");

/// Output: [`ThreadGoalResponse`].
pub const THREAD_GOAL_SET_COMMAND_ID: &str = "goals.set";
pub const THREAD_GOAL_SET_COMMAND: ProductSurfaceCommandDescriptor<
    ThreadGoalSetRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(THREAD_GOAL_SET_COMMAND_ID);
