//! Orchestrator operation descriptors: the delegated-task, thread-queue,
//! fork and merge-back reads and mutations the WebUI routes call through the
//! product surface. Declared here (not in the product crate) per the
//! transport/product boundary, like [`crate::notification_setup`]; the
//! behavior lives in `ironclaw_assistant`'s orchestration decorator over the
//! `ironclaw_orchestrator` engine.
//!
//! Outputs are the engine's JSON documents (camelCase, the T3 orchestrator
//! wire): correctable failures come back as `{"ok": false, "error": <code>,
//! "message": ...}` with HTTP 200 so clients can show the reason; a thread
//! or task the caller does not own is a 404.

use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};

/// Body-free thread selector for the thread-scoped reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorThreadRequest {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorTaskRequest {
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorTaskCancelRequest {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One queue mutation. `action`: `add` (text), `edit` (item_id, text),
/// `remove` (item_id), `reorder` (item_id, before_id; none = end), `clear`,
/// `hold`, `release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorQueueMutationRequest {
    pub thread_id: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
}

/// Fork `thread_id` from run `run` (1-based; latest when absent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorForkRequest {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Switch the provider/model of `thread_id`'s next turns. Model `default`
/// clears the pick. The answer says whether the thread switched in place,
/// moved runtime before its first turn, or continued in a fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorRouteRequest {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort of the thread's next turns, one of its provider's
    /// `efforts`; `default` clears it. Alone it changes nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Replaces the user's model-picker preferences (hidden providers and
    /// models, favorites) instead of switching the thread's route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picker: Option<serde_json::Value>,
    /// Turns idle compaction of Claude CLI threads on or off (engine-wide,
    /// like `picker`) instead of switching the thread's route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_compact: Option<bool>,
}

/// Merge fork `thread_id` back into its source (or `target_thread_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorMergeBackRequest {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<u32>,
}

/// Delegated tasks of one thread.
pub const ORCHESTRATOR_THREAD_TASKS_VIEW: ProductView<
    OrchestratorThreadRequest,
    serde_json::Value,
> = ProductView::unpaginated("orchestrator.thread_tasks");
/// One task (status, result, usage). Reading a finished result acknowledges it.
pub const ORCHESTRATOR_TASK_VIEW: ProductView<OrchestratorTaskRequest, serde_json::Value> =
    ProductView::unpaginated("orchestrator.task");
/// The thread's durable queue and hold state.
pub const ORCHESTRATOR_THREAD_QUEUE_VIEW: ProductView<
    OrchestratorThreadRequest,
    serde_json::Value,
> = ProductView::unpaginated("orchestrator.thread_queue");
/// Forks out of and merges into/out of the thread.
pub const ORCHESTRATOR_THREAD_LINEAGE_VIEW: ProductView<
    OrchestratorThreadRequest,
    serde_json::Value,
> = ProductView::unpaginated("orchestrator.thread_lineage");
/// Runs (turns) with status, and the per-turn usage log.
pub const ORCHESTRATOR_THREAD_RUNS_VIEW: ProductView<OrchestratorThreadRequest, serde_json::Value> =
    ProductView::unpaginated("orchestrator.thread_runs");

/// The thread's provider/model route and every provider it can switch to:
/// `{threadId, runtime, provider, model, fresh, providers: [{provider, name,
/// runtime, authenticated, models}]}`.
pub const ORCHESTRATOR_THREAD_ROUTE_VIEW: ProductView<
    OrchestratorThreadRequest,
    serde_json::Value,
> = ProductView::unpaginated("orchestrator.thread_route");

/// Every provider the caller's threads can run on, with the model-picker
/// preferences: `{providers, picker}`, shaped like the thread route's. For
/// screens without a thread (profile defaults, a fresh install).
pub const ORCHESTRATOR_PROVIDERS_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("orchestrator.providers");

/// Latest per-turn telemetry of a thread (the desktop status bar).
pub const ORCHESTRATOR_THREAD_TELEMETRY_VIEW: ProductView<
    OrchestratorThreadRequest,
    ThreadTelemetryResponse,
> = ProductView::unpaginated("orchestrator.thread_telemetry");

/// Context fill, generation speed and cache hit rate of the thread's latest
/// turn, whichever runtime ran it. Every measurement is `null` until a
/// runtime has reported one; `updatedAt` is unix milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTelemetryResponse {
    pub thread_id: String,
    pub runtime_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub context_used: Option<u64>,
    pub context_max: Option<u64>,
    pub context_pct: Option<f64>,
    pub tps: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub updated_at: Option<i64>,
}

pub const ORCHESTRATOR_TASK_CANCEL_COMMAND_ID: &str = "orchestrator.task_cancel";
pub const ORCHESTRATOR_TASK_CANCEL_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorTaskCancelRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_TASK_CANCEL_COMMAND_ID);

pub const ORCHESTRATOR_QUEUE_MUTATE_COMMAND_ID: &str = "orchestrator.queue_mutate";
pub const ORCHESTRATOR_QUEUE_MUTATE_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorQueueMutationRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_QUEUE_MUTATE_COMMAND_ID);

pub const ORCHESTRATOR_FORK_COMMAND_ID: &str = "orchestrator.thread_fork";
pub const ORCHESTRATOR_FORK_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorForkRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_FORK_COMMAND_ID);

pub const ORCHESTRATOR_ROUTE_COMMAND_ID: &str = "orchestrator.thread_route_set";
pub const ORCHESTRATOR_ROUTE_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorRouteRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_ROUTE_COMMAND_ID);

pub const ORCHESTRATOR_MERGE_BACK_COMMAND_ID: &str = "orchestrator.thread_merge_back";
pub const ORCHESTRATOR_MERGE_BACK_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorMergeBackRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_MERGE_BACK_COMMAND_ID);

/// What a schedule mutation does. `create` binds the schedule to
/// `thread_id`: its runs post there (or, with `bind_to_current_thread:
/// false`, into a fresh thread per run on that thread's profile and model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorScheduleAction {
    Create,
    Update,
    Delete,
    RunNow,
    /// Webhook schedules: a new backend-made signing secret; the URL stays.
    RotateSecret,
}

/// One schedule mutation from the desktop. `schedule` is the `t3_schedule`
/// spec (`{type:'interval', everyMs}`, `{type:'fixed_time', timeOfDay,
/// weekdays, timezone}`, `{type:'cron', expression, timezone}` or
/// `{type:'webhook'}`); `signature` is `{header, prefix, encoding,
/// generate: true}` — the desktop never sends a secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorScheduleMutationRequest {
    pub action: OrchestratorScheduleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<String>,
    /// `create` only: the caller's thread the schedule belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_to_current_thread: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
}

/// Validate a schedule spec (and optionally its prompt) without storing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorSchedulePreviewRequest {
    pub schedule: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorScheduleRequest {
    pub schedule_id: String,
}

/// Every schedule of the caller (`t3_schedule` rows, agent- or
/// desktop-made): `{ok, schedules: [...]}`. Webhook URLs are masked.
pub const ORCHESTRATOR_SCHEDULES_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("orchestrator.schedules");

/// `{ok, schedule, describe, nextRuns: [unix ms; ≤3]}` or the error document.
pub const ORCHESTRATOR_SCHEDULE_PREVIEW_VIEW: ProductView<
    OrchestratorSchedulePreviewRequest,
    serde_json::Value,
> = ProductView::unpaginated("orchestrator.schedule_preview");

/// A webhook schedule's full URL and signing secret, for an explicit
/// Reveal/Copy. Never logged; the list view carries neither.
pub const ORCHESTRATOR_SCHEDULE_SECRET_VIEW: ProductView<
    OrchestratorScheduleRequest,
    serde_json::Value,
> = ProductView::unpaginated("orchestrator.schedule_secret");

pub const ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND_ID: &str = "orchestrator.schedule_mutate";
pub const ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorScheduleMutationRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND_ID);
