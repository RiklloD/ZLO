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
    /// The thread a delegated child was created from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    /// Started by an automation (not the person).
    #[serde(default)]
    pub automation: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoalResponse {
    pub thread_id: String,
    /// `None` when the thread has no goal (or it was cleared).
    #[serde(default)]
    pub goal: Option<GoalRecord>,
}

/// Set or replace the goal; `objective: None` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGoalSetRequest {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GoalStatus>,
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
