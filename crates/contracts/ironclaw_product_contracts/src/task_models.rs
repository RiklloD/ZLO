//! Task models: one operator-wide model per background task (thread titles
//! today), set once in Providers and used by every profile. A task call is a
//! single short completion with no tools and no agent run.
//!
//! The selection and the completion are answered by
//! [`crate::operator_llm::LlmConfigService`]; the thread-title command is
//! answered by the orchestrator surface, which owns the inbox title.

use serde::{Deserialize, Serialize};

use crate::descriptors::ProductSurfaceCommandDescriptor;

/// A background job that runs on its own model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTask {
    ThreadTitles,
}

/// A provider from the LLM catalog and one of its models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskModelChoice {
    pub provider_id: String,
    pub model: String,
}

/// Every task's model; `None` turns the task off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_titles: Option<TaskModelChoice>,
}

impl TaskModels {
    pub fn get(&self, task: BackgroundTask) -> Option<&TaskModelChoice> {
        match task {
            BackgroundTask::ThreadTitles => self.thread_titles.as_ref(),
        }
    }
}

/// One short completion on a task's model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCompletionRequest {
    pub task: BackgroundTask,
    pub system: String,
    pub prompt: String,
    pub max_tokens: u32,
}

/// `text` is `None` when the task has no model set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCompletionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Input `{}`. Output: [`TaskModels`].
pub const TASK_MODELS_GET_COMMAND_ID: &str = "llm.task_models.get";
pub const TASK_MODELS_GET_COMMAND: ProductSurfaceCommandDescriptor<serde_json::Value, TaskModels> =
    ProductSurfaceCommandDescriptor::new(TASK_MODELS_GET_COMMAND_ID);

/// Input and output: [`TaskModels`] (the whole set, replaced).
pub const TASK_MODELS_SET_COMMAND_ID: &str = "llm.task_models.set";
pub const TASK_MODELS_SET_COMMAND: ProductSurfaceCommandDescriptor<TaskModels, TaskModels> =
    ProductSurfaceCommandDescriptor::new(TASK_MODELS_SET_COMMAND_ID);

/// Internal: the orchestrator surface asks for a completion through this.
/// No route mounts it.
pub const TASK_COMPLETE_COMMAND_ID: &str = "llm.task.complete";
pub const TASK_COMPLETE_COMMAND: ProductSurfaceCommandDescriptor<
    TaskCompletionRequest,
    TaskCompletionResult,
> = ProductSurfaceCommandDescriptor::new(TASK_COMPLETE_COMMAND_ID);

/// Replies between two title checks of the same thread. The backend titles
/// a thread after its first reply, then re-reads it every this many replies
/// and renames it when the topic moved.
pub const RETITLE_EVERY_REPLIES: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_models_round_trip_with_tasks_off() {
        let empty: TaskModels = serde_json::from_str("{}").expect("empty");
        assert_eq!(empty.get(BackgroundTask::ThreadTitles), None);
        let set: TaskModels = serde_json::from_value(serde_json::json!({
            "thread_titles": {"provider_id": "glm", "model": "glm-4.6"}
        }))
        .expect("set");
        assert_eq!(
            set.get(BackgroundTask::ThreadTitles)
                .map(|c| c.model.as_str()),
            Some("glm-4.6")
        );
        assert_eq!(
            serde_json::to_value(&empty).expect("ser"),
            serde_json::json!({})
        );
    }
}
