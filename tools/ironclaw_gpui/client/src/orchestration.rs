//! Typed calls to the orchestrator routes (`CONTRACT.md`, "Orchestrator
//! routes"): delegated tasks of a thread, its durable queue, runs, lineage,
//! fork and merge-back.
//!
//! The gateway forwards the engine's camelCase documents as untyped JSON
//! (`ProductView<_, Value>`), so the shapes below are this client's reading
//! of them. Unknown fields are ignored and every optional field defaults, so
//! an engine that adds fields never breaks the app. A correctable failure is
//! HTTP 200 with `ok: false`; it comes back as [`ClientError::Refused`].

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

/// Unix milliseconds, as the engine sends them.
pub type Millis = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Starting,
    Running,
    Waiting,
    WaitingChildren,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    #[serde(other)]
    Unknown,
}

impl TaskStatus {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Waiting | Self::WaitingChildren
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Waiting => "Waiting for usage limit",
            Self::WaitingChildren => "Waiting for its subagents",
            Self::Completed => "Done",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Interrupted => "Interrupted",
            Self::Unknown => "Unknown",
        }
    }
}

/// One delegated child task (`TaskView`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub task_id: String,
    #[serde(default)]
    pub title: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub parent_thread_id: String,
    #[serde(default)]
    pub child_thread_id: Option<String>,
    #[serde(default)]
    pub created_at: Millis,
    #[serde(default)]
    pub started_at: Option<Millis>,
    #[serde(default)]
    pub finished_at: Option<Millis>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub limit_reset_at: Option<Millis>,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub pending_children: u32,
}

impl TaskView {
    /// "model" or "provider · model", whichever the engine knows.
    pub fn engine_label(&self) -> String {
        match (self.provider.as_deref(), self.model.as_deref()) {
            (Some(p), Some(m)) => format!("{p} · {m}"),
            (None, Some(m)) => m.to_string(),
            (Some(p), None) => p.to_string(),
            (None, None) => self.runtime.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTasks {
    #[serde(default)]
    pub tasks: Vec<TaskView>,
}

/// Why a thread's queue is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// The person pressed Stop.
    Stopped,
    Cancelled,
    /// Held by hand from the queue panel.
    User,
    /// The app quit mid-turn; Continue (release) resumes the cut turn.
    Interrupted,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
    /// Fractional, so a reorder never renumbers the queue.
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: Millis,
}

/// The queue document every queue route answers with.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueDoc {
    #[serde(default)]
    pub held: bool,
    #[serde(default)]
    pub hold_reason: Option<HoldReason>,
    #[serde(default)]
    pub items: Vec<QueueItem>,
    /// Set on `add`.
    #[serde(default)]
    pub queued_id: Option<String>,
    /// The thread's runtime; anything but `ironclaw` has no run stream.
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub active_run_id: Option<String>,
}

impl QueueDoc {
    /// The running turn of a thread off IronClaw's loop (a profile on a CLI
    /// provider). Those turns are seen by polling, not through the stream.
    pub fn external_run(&self) -> Option<&str> {
        let external = self.runtime.as_deref().is_some_and(|r| r != "ironclaw");
        (external && self.running).then(|| self.active_run_id.as_deref().unwrap_or(""))
    }
}

/// A queue mutation (`POST /threads/{id}/queue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    Add(String),
    Edit {
        item_id: String,
        text: String,
    },
    Remove(String),
    /// Move `item_id` before `before_id`, or to the end when `None`.
    Reorder {
        item_id: String,
        before_id: Option<String>,
    },
    Clear,
    Hold,
    /// Also the Continue of an interrupted thread.
    Release,
}

impl QueueAction {
    fn body(&self) -> Value {
        match self {
            Self::Add(text) => json!({"action": "add", "text": text}),
            Self::Edit { item_id, text } => {
                json!({"action": "edit", "item_id": item_id, "text": text})
            }
            Self::Remove(id) => json!({"action": "remove", "item_id": id}),
            Self::Reorder { item_id, before_id } => {
                let mut body = json!({"action": "reorder", "item_id": item_id});
                if let Some(before) = before_id {
                    body["before_id"] = json!(before);
                }
                body
            }
            Self::Clear => json!({"action": "clear"}),
            Self::Hold => json!({"action": "hold"}),
            Self::Release => json!({"action": "release"}),
        }
    }
}

/// One transfer record (fork, merge-back, ...).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    pub source_thread: String,
    #[serde(default)]
    pub target_thread: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: Millis,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lineage {
    /// The fork transfer that created this thread; its `source_thread` is
    /// the thread it was forked from.
    #[serde(default)]
    pub forked_from: Option<Transfer>,
    #[serde(default)]
    pub forks: Vec<Transfer>,
    #[serde(default)]
    pub merges_in: Vec<Transfer>,
    #[serde(default)]
    pub merges_out: Vec<Transfer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub ordinal: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRuns {
    #[serde(default)]
    pub runs: Vec<RunSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Forked {
    pub fork_thread_id: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedBack {
    #[serde(default)]
    pub target_thread_id: Option<String>,
    #[serde(default)]
    pub delivery: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// Fork options; all default to the engine's choice.
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    pub run: Option<u32>,
    pub title: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// `ok: false` → [`ClientError::Refused`], anything else decodes as `T`.
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ClientError> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let code = text("error").unwrap_or_else(|| "refused".to_string());
        let message = text("message").unwrap_or_else(|| code.replace('_', " "));
        return Err(ClientError::Refused { code, message });
    }
    serde_json::from_value(value).map_err(|e| ClientError::Decode(e.to_string()))
}

impl Client {
    async fn orch_get<T: DeserializeOwned>(&self, path: String) -> Result<T, ClientError> {
        decode(self.get::<Value>(&path).await?)
    }

    async fn orch_post<T: DeserializeOwned>(
        &self,
        path: String,
        body: Value,
    ) -> Result<T, ClientError> {
        decode(self.post::<_, Value>(&path, &body).await?)
    }

    /// Delegated child tasks of a thread, live and finished.
    pub async fn thread_tasks(&self, thread_id: &str) -> Result<ThreadTasks, ClientError> {
        self.orch_get(format!("{V2}/threads/{}/tasks", seg(thread_id)))
            .await
    }

    /// One task. Reading a finished result acknowledges its delivery.
    pub async fn task(&self, task_id: &str) -> Result<TaskView, ClientError> {
        self.orch_get(format!("{V2}/tasks/{}", seg(task_id))).await
    }

    /// Stop a child task and everything it delegated.
    pub async fn cancel_task(&self, task_id: &str) -> Result<TaskView, ClientError> {
        self.orch_post(
            format!("{V2}/tasks/{}/cancel", seg(task_id)),
            json!({"reason": "cancelled from IronClaw Desktop"}),
        )
        .await
    }

    pub async fn thread_queue(&self, thread_id: &str) -> Result<QueueDoc, ClientError> {
        self.orch_get(format!("{V2}/threads/{}/queue", seg(thread_id)))
            .await
    }

    pub async fn mutate_queue(
        &self,
        thread_id: &str,
        action: &QueueAction,
    ) -> Result<QueueDoc, ClientError> {
        self.orch_post(
            format!("{V2}/threads/{}/queue", seg(thread_id)),
            action.body(),
        )
        .await
    }

    pub async fn thread_runs(&self, thread_id: &str) -> Result<ThreadRuns, ClientError> {
        self.orch_get(format!("{V2}/threads/{}/runs", seg(thread_id)))
            .await
    }

    pub async fn thread_lineage(&self, thread_id: &str) -> Result<Lineage, ClientError> {
        self.orch_get(format!("{V2}/threads/{}/lineage", seg(thread_id)))
            .await
    }

    pub async fn fork_thread(
        &self,
        thread_id: &str,
        options: &ForkOptions,
    ) -> Result<Forked, ClientError> {
        let mut body = json!({});
        if let Some(run) = options.run {
            body["run"] = json!(run);
        }
        for (key, value) in [
            ("title", &options.title),
            ("provider", &options.provider),
            ("model", &options.model),
        ] {
            if let Some(value) = value {
                body[key] = json!(value);
            }
        }
        self.orch_post(format!("{V2}/threads/{}/fork", seg(thread_id)), body)
            .await
    }

    /// Merge a fork back; the target defaults to the thread it came from.
    pub async fn merge_back(
        &self,
        fork_thread_id: &str,
        target_thread_id: Option<&str>,
    ) -> Result<MergedBack, ClientError> {
        let body = match target_thread_id {
            Some(target) => json!({"target_thread_id": target}),
            None => json!({}),
        };
        self.orch_post(
            format!("{V2}/threads/{}/merge-back", seg(fork_thread_id)),
            body,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_takes_its_own_efforts_or_the_providers() {
        let provider: RouteProvider = serde_json::from_value(json!({
            "provider": "anthropic", "name": "Anthropic", "runtime": "ironclaw",
            "authenticated": true, "models": ["claude-opus-5-5", "claude-3-5-haiku"],
            "efforts": [],
            "modelEfforts": {"claude-opus-5-5": ["low", "max"], "claude-3-5-haiku": []}
        }))
        .unwrap();
        assert_eq!(provider.efforts_for("claude-opus-5-5"), ["low", "max"]);
        assert!(provider.efforts_for("claude-3-5-haiku").is_empty());
        assert!(provider.efforts_for("default").is_empty());
        let older: RouteProvider =
            serde_json::from_value(json!({"provider": "codex", "efforts": ["low"]})).unwrap();
        assert_eq!(older.efforts_for("gpt-6-sol"), ["low"]);
    }

    #[test]
    fn refusal_and_engine_documents_decode() {
        let refused = decode::<QueueDoc>(json!({
            "ok": false, "error": "not_queued", "message": "Already sent."
        }));
        assert!(matches!(
            refused,
            Err(ClientError::Refused { ref code, ref message })
                if code == "not_queued" && message == "Already sent."
        ));

        let queue: QueueDoc = decode(json!({
            "ok": true, "threadId": "t", "held": true, "holdReason": "interrupted",
            "items": [{"id": "q1", "text": "next", "position": 1.5, "status": "pending",
                       "createdAt": 1, "updatedAt": 1, "createdBy": "user"}],
            "queuedId": "q1"
        }))
        .unwrap();
        assert_eq!(queue.hold_reason, Some(HoldReason::Interrupted));
        assert_eq!(queue.items.len(), 1);

        let tasks: ThreadTasks = decode(json!({"threadId": "t", "tasks": [{
            "taskId": "k", "title": "Research", "role": "worker", "status": "waiting_children",
            "workState": "working", "runtime": "ironclaw", "provider": null, "model": "glm-5",
            "parentThreadId": "t", "childThreadId": "c", "depth": 1, "createdAt": 5,
            "startedAt": 6, "finishedAt": null, "durationSeconds": 1.5, "progress": {"x": 1},
            "usage": null, "summary": null, "error": null, "delivery": "pending",
            "limitResetAt": null, "cancelRequested": false, "pendingChildren": 2,
            "brandNewField": true
        }]}))
        .unwrap();
        let task = &tasks.tasks[0];
        assert!(task.status.is_live());
        assert_eq!(task.engine_label(), "glm-5");
        assert_eq!(task.pending_children, 2);

        let reorder = QueueAction::Reorder {
            item_id: "q2".into(),
            before_id: Some("q1".into()),
        };
        assert_eq!(
            reorder.body(),
            json!({"action": "reorder", "item_id": "q2", "before_id": "q1"})
        );
    }
}

/// One provider a thread can switch to (`GET /threads/{id}/route`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RouteProvider {
    pub provider: String,
    pub name: String,
    pub runtime: String,
    pub authenticated: bool,
    pub models: Vec<String>,
    /// Reasoning efforts of the provider's default model (t3code's
    /// "Reasoning"); empty: none.
    pub efforts: Vec<String>,
    /// Each listed model's own efforts; a model missing here takes `efforts`.
    pub model_efforts: std::collections::BTreeMap<String, Vec<String>>,
    /// The vendor behind a runtime row (IronClaw's loop on ChatGPT): its
    /// logo stands for the row.
    pub brand: Option<String>,
}

impl RouteProvider {
    /// The efforts `model` takes; `"default"` is the provider's default.
    pub fn efforts_for(&self, model: &str) -> &[String] {
        self.model_efforts.get(model).unwrap_or(&self.efforts)
    }
}

/// A thread's provider/model and its alternatives.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadRoute {
    pub thread_id: String,
    pub runtime: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// The thread's reasoning effort; `None`: the provider's default.
    pub effort: Option<String>,
    /// No user turn yet: any switch stays in this thread.
    pub fresh: bool,
    pub providers: Vec<RouteProvider>,
    /// The user's picker preferences (the same for every thread).
    pub picker: PickerPrefs,
}

/// One model of one provider in the picker preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PickerModel {
    pub provider: String,
    pub model: String,
}

impl PickerModel {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// What the model picker hides and pins (backend settings, t3code's shape).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PickerPrefs {
    pub hidden_providers: Vec<String>,
    pub hidden_models: Vec<PickerModel>,
    pub favorites: Vec<PickerModel>,
}

impl PickerPrefs {
    pub fn provider_hidden(&self, provider: &str) -> bool {
        self.hidden_providers.iter().any(|p| p == provider)
    }

    pub fn model_hidden(&self, provider: &str, model: &str) -> bool {
        self.hidden_models
            .iter()
            .any(|m| m.provider == provider && m.model == model)
    }

    pub fn favorite(&self, provider: &str, model: &str) -> bool {
        self.favorites
            .iter()
            .any(|m| m.provider == provider && m.model == model)
    }

    pub fn toggle_provider(&mut self, provider: &str) {
        toggle(&mut self.hidden_providers, provider.to_string());
    }

    pub fn toggle_model(&mut self, provider: &str, model: &str) {
        toggle(&mut self.hidden_models, PickerModel::new(provider, model));
    }

    pub fn toggle_favorite(&mut self, provider: &str, model: &str) {
        toggle(&mut self.favorites, PickerModel::new(provider, model));
    }
}

fn toggle<T: PartialEq>(list: &mut Vec<T>, item: T) {
    match list.iter().position(|x| *x == item) {
        Some(ix) => {
            list.remove(ix);
        }
        None => list.push(item),
    }
}

/// Where a route switch left the conversation.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RouteChange {
    /// `in_place`, `rebound` or `forked`.
    pub moved: String,
    /// Set when the switch continued in a new thread.
    pub fork_thread_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Client {
    pub async fn thread_route(&self, thread_id: &str) -> Result<ThreadRoute, ClientError> {
        self.orch_get(format!("{V2}/threads/{}/route", seg(thread_id)))
            .await
    }

    /// Model `default` clears the pick (IronClaw's loop only). The switch
    /// keeps `effort` only; `None` returns to the provider's default.
    pub async fn set_thread_route(
        &self,
        thread_id: &str,
        provider: &str,
        model: &str,
        effort: Option<&str>,
    ) -> Result<RouteChange, ClientError> {
        let mut body = json!({"provider": provider, "model": model});
        if let Some(effort) = effort {
            body["effort"] = json!(effort);
        }
        self.orch_post(format!("{V2}/threads/{}/route", seg(thread_id)), body)
            .await
    }

    /// Set the reasoning effort alone; `default` clears it.
    pub async fn set_thread_effort(
        &self,
        thread_id: &str,
        effort: &str,
    ) -> Result<RouteChange, ClientError> {
        self.orch_post(
            format!("{V2}/threads/{}/route", seg(thread_id)),
            json!({ "effort": effort }),
        )
        .await
    }

    /// Replace the picker preferences; read through any of the user's threads.
    pub async fn set_model_picker(
        &self,
        thread_id: &str,
        prefs: &PickerPrefs,
    ) -> Result<(), ClientError> {
        let _: Value = self
            .orch_post(
                format!("{V2}/threads/{}/route", seg(thread_id)),
                json!({ "picker": prefs }),
            )
            .await?;
        Ok(())
    }
}
