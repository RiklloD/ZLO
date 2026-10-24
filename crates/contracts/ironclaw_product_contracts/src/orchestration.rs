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

pub const ORCHESTRATOR_MERGE_BACK_COMMAND_ID: &str = "orchestrator.thread_merge_back";
pub const ORCHESTRATOR_MERGE_BACK_COMMAND: ProductSurfaceCommandDescriptor<
    OrchestratorMergeBackRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(ORCHESTRATOR_MERGE_BACK_COMMAND_ID);
