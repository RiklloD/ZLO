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
use crate::wire::{MessageKind, TimelineMessage};
use ironclaw_product_contracts::inbound_requests::ProductInboundAttachment;

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
    #[serde(default)]
    pub kind: QueueItemKind,
    /// A deferred steer's target run.
    #[serde(default)]
    pub steer_run: Option<String>,
}

impl QueueItem {
    /// Taken by a delivery in flight (`sending`): not editable, removable
    /// or sendable until it settles.
    pub fn is_sending(&self) -> bool {
        self.status == "sending"
    }
}

/// What a queue item is (t3code's queued run vs. steer follow-up).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueItemKind {
    /// A message queued for a later turn.
    #[default]
    Message,
    /// A steer the agent never read, queued again ahead of newer messages.
    RecoveredSteer,
    /// A steer waiting for the running turn to take input.
    DeferredSteer,
    #[serde(other)]
    Other,
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
    /// The running turn is a context compaction: follow-ups queue.
    #[serde(default)]
    pub compacting: bool,
    /// What `send_now` / `promote` did: `steered`, `started`,
    /// `waiting_for_turn` or `restarted`.
    #[serde(default)]
    pub delivery: Option<String>,
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
    /// Queue words, with files: the backend lands them beside the thread
    /// and the queued text names them.
    Add {
        text: String,
        attachments: Vec<ProductInboundAttachment>,
    },
    Edit {
        item_id: String,
        text: String,
        attachments: Vec<ProductInboundAttachment>,
    },
    /// t3code's Send now: steer the item into the running turn, or start it
    /// on an idle thread (a held queue stays held). Atomic on the backend:
    /// an item the queue already took answers `not_queued`.
    SendNow(String),
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
            Self::Add { text, attachments } => {
                let mut body = json!({"action": "add", "text": text});
                if !attachments.is_empty() {
                    body["attachments"] = json!(attachments);
                }
                body
            }
            Self::Edit {
                item_id,
                text,
                attachments,
            } => {
                let mut body = json!({"action": "edit", "item_id": item_id, "text": text});
                if !attachments.is_empty() {
                    body["attachments"] = json!(attachments);
                }
                body
            }
            Self::SendNow(id) => json!({"action": "send_now", "item_id": id}),
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

/// A fork merged back into a thread, as [`Client::merged_in`] reads it.
#[derive(Debug, Clone)]
pub struct MergedFork {
    pub title: String,
    pub at: Millis,
    /// Not sent to the model yet: it rides on the next message.
    pub pending: bool,
    pub messages: Vec<TimelineMessage>,
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
    /// For a fork: `{run, messages}`, `messages` being how many of the
    /// source's history messages it was cut after.
    #[serde(default)]
    pub source_point: Option<Value>,
    /// For a merge-back: `{messages}`, where the fork's own work starts.
    #[serde(default)]
    pub base_point: Option<Value>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: Millis,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lineage {
    /// The task that created this thread, when it is a delegated child.
    #[serde(default)]
    pub delegated_from: Option<TaskView>,
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

/// A thread's git worktree (`POST /threads/{id}/worktree`). The live
/// fields are present only while the folder exists.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeView {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub base_commit: String,
    #[serde(default)]
    pub source_dir: String,
    /// `active`, `kept`, `merged` or `removed`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub exists: bool,
    #[serde(default)]
    pub ahead: Option<u64>,
    #[serde(default)]
    pub behind: Option<u64>,
    #[serde(default)]
    pub dirty_count: Option<u64>,
    #[serde(default)]
    pub setup_log: Option<String>,
}

impl WorktreeView {
    /// The thread works in it now.
    pub fn bound(&self) -> bool {
        self.exists && self.state != "removed"
    }
}

/// Fork options; all default to the engine's choice.
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    pub run: Option<u32>,
    pub title: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Seed the fork with a digest of the conversation (a fresh context)
    /// instead of branching the session.
    pub summary: bool,
}

/// `ok: false` → [`ClientError::Refused`], anything else decodes as `T`.
pub(crate) fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ClientError> {
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
            json!({"reason": "cancelled from ZLO"}),
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

    pub async fn thread_lineage(&self, thread_id: &str) -> Result<Lineage, ClientError> {
        self.orch_get(format!("{V2}/threads/{}/lineage", seg(thread_id)))
            .await
    }

    /// What a fork shows above its own messages: the source thread's
    /// timeline up to the fork point, and that source's own source when it
    /// was a fork too (oldest first, at most 8 forks deep).
    pub async fn fork_prefix(&self, thread_id: &str) -> Result<Vec<TimelineMessage>, ClientError> {
        let mut prefix: Vec<TimelineMessage> = Vec::new();
        let mut thread = thread_id.to_string();
        for _ in 0..8 {
            let Some(from) = self.thread_lineage(&thread).await?.forked_from else {
                break;
            };
            let cut = from
                .source_point
                .as_ref()
                .and_then(|p| p["messages"].as_u64())
                .unwrap_or(u64::MAX) as usize;
            let mut messages = self.timeline(&from.source_thread, 500).await?.messages;
            messages.sort_by_key(|m| m.sequence);
            // The cut counts the engine's history, which skips checkpoints.
            let mut seen = 0;
            let end = messages
                .iter()
                .position(|m| {
                    if m.kind != MessageKind::CheckpointReference {
                        seen += 1;
                    }
                    seen > cut
                })
                .unwrap_or(messages.len());
            messages.truncate(end);
            messages.append(&mut prefix);
            prefix = messages;
            thread = from.source_thread;
        }
        Ok(prefix)
    }

    /// The forks merged back into this thread, oldest first: each fork's
    /// title, when it merged, and the fork messages the merge carried.
    pub async fn merged_in(&self, thread_id: &str) -> Result<Vec<MergedFork>, ClientError> {
        let mut out = Vec::new();
        for merge in self.thread_lineage(thread_id).await?.merges_in {
            let point = |p: &Option<Value>, default: u64| {
                p.as_ref()
                    .and_then(|p| p["messages"].as_u64())
                    .unwrap_or(default) as usize
            };
            let (base, cut) = (
                point(&merge.base_point, 0),
                point(&merge.source_point, u64::MAX),
            );
            let mut messages = self.timeline(&merge.source_thread, 500).await?.messages;
            messages.sort_by_key(|m| m.sequence);
            // Both points count the engine's history, which skips checkpoints.
            let mut seen = 0;
            messages.retain(|m| {
                if m.kind == MessageKind::CheckpointReference {
                    return false;
                }
                seen += 1;
                seen > base && seen <= cut
            });
            let title = self
                .thread_label(&merge.source_thread)
                .await
                .unwrap_or_else(|_| "a fork".into()); // silent-ok: the label is cosmetic
            out.push(MergedFork {
                title,
                at: merge.created_at,
                pending: merge.status == "pending",
                messages,
            });
        }
        out.sort_by_key(|m| m.at);
        Ok(out)
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
        if options.summary {
            body["summary"] = json!(true);
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

    /// The thread's worktree; `Ok(None)` when it has none.
    pub async fn thread_worktree(
        &self,
        thread_id: &str,
    ) -> Result<Option<WorktreeView>, ClientError> {
        match self
            .orch_post(
                format!("{V2}/threads/{}/worktree", seg(thread_id)),
                json!({"action": "status"}),
            )
            .await
        {
            Ok(view) => Ok(Some(view)),
            Err(ClientError::Refused { code, .. }) if code == "no_worktree" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove the thread's worktree and branch only while they hold no
    /// work (no uncommitted changes, no commits). Whether it was removed.
    pub async fn clean_thread_worktree(&self, thread_id: &str) -> Result<bool, ClientError> {
        let view: serde_json::Value = self
            .orch_post(
                format!("{V2}/threads/{}/worktree", seg(thread_id)),
                json!({"action": "clean"}),
            )
            .await?;
        Ok(view["cleaned"].as_bool().unwrap_or(false))
    }

    /// Start the thread in a new git worktree made from `source_dir`
    /// (default: the thread's working folder). Only committed work is in it.
    pub async fn start_thread_worktree(
        &self,
        thread_id: &str,
        source_dir: Option<&str>,
    ) -> Result<WorktreeView, ClientError> {
        let mut body = json!({"action": "create"});
        if let Some(dir) = source_dir {
            body["source_dir"] = json!(dir);
        }
        self.orch_post(format!("{V2}/threads/{}/worktree", seg(thread_id)), body)
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
    /// The setters land the same state however often they run, so a
    /// change re-applied on a fresher copy (`edit_model_picker`) never flips
    /// it back the way a toggle would.
    #[test]
    fn picker_setters_are_idempotent() {
        let mut p = super::PickerPrefs::default();
        p.set_provider_hidden("muse-code", true);
        p.set_provider_hidden("muse-code", true);
        assert_eq!(p.hidden_providers, vec!["muse-code".to_string()]);
        p.set_model_hidden("glm", "glm-5", true);
        p.set_model_hidden("glm", "glm-5", true);
        assert!(p.model_hidden("glm", "glm-5") && p.hidden_models.len() == 1);
        p.set_model_hidden("glm", "glm-5", false);
        p.set_model_hidden("glm", "glm-5", false);
        assert!(p.hidden_models.is_empty());
        p.set_favorite("codex", "gpt-6.2-sol", true);
        p.set_provider_hidden("muse-code", false);
        assert!(p.favorite("codex", "gpt-6.2-sol") && p.hidden_providers.is_empty());
    }

    use super::*;

    #[test]
    fn a_worktree_reads_the_engine_document_and_a_missing_one_is_refused() {
        let view: WorktreeView = decode(json!({
            "threadId": "t", "path": "/w/fix-1", "branch": "zlo/fix-1", "baseCommit": "abc1234",
            "state": "kept", "exists": true, "ahead": 2, "behind": 0, "dirtyCount": 1,
            "repoRoot": "/r", "sourceDir": "/r", "createdAt": 1
        }))
        .unwrap();
        assert!(view.bound());
        assert_eq!(view.branch, "zlo/fix-1");
        assert_eq!(view.ahead, Some(2));
        let removed: WorktreeView = decode(json!({"state": "removed", "exists": false})).unwrap();
        assert!(!removed.bound());
        let missing =
            decode::<WorktreeView>(json!({"ok": false, "error": "no_worktree", "message": "none"}));
        assert!(matches!(missing, Err(ClientError::Refused { code, .. }) if code == "no_worktree"));
    }

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

    #[test]
    fn send_now_and_queued_files_reach_the_wire() {
        assert_eq!(
            QueueAction::SendNow("q1".into()).body(),
            json!({"action": "send_now", "item_id": "q1"})
        );
        let file = ProductInboundAttachment {
            mime_type: "text/plain".into(),
            filename: Some("a.txt".into()),
            data_base64: "aGk=".into(),
            path: None,
        };
        let add = QueueAction::Add {
            text: "read it".into(),
            attachments: vec![file],
        };
        let body = add.body();
        assert_eq!(body["action"], "add");
        assert_eq!(body["attachments"][0]["filename"], "a.txt");
        let plain = QueueAction::Add {
            text: "words".into(),
            attachments: Vec::new(),
        };
        assert!(plain.body().get("attachments").is_none());
        let doc: QueueDoc =
            serde_json::from_value(json!({"compacting": true, "delivery": "started",
            "items": [{"id": "q", "text": "t", "status": "sending"}]}))
            .unwrap();
        assert!(doc.compacting && doc.items[0].is_sending());
    }

    #[test]
    fn route_providers_carry_the_launch_kind() {
        let legacy: RouteProvider = serde_json::from_value(
            json!({"provider": "antigravity", "runtime": "antigravity", "authenticated": false, "launch": "legacy"}),
        )
        .unwrap();
        assert_eq!(legacy.launch, Some(CatalogLaunch::Legacy));
        assert!(legacy.needs_native_cli());
        let native: RouteProvider =
            serde_json::from_value(json!({"provider": "antigravity", "launch": "native"})).unwrap();
        assert!(!native.needs_native_cli());
        let unknown: RouteProvider =
            serde_json::from_value(json!({"provider": "x", "launch": "something-new"})).unwrap();
        assert_eq!(unknown.launch, Some(CatalogLaunch::Other));
        let none: RouteProvider = serde_json::from_value(json!({"provider": "x"})).unwrap();
        assert_eq!(none.launch, None);
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
    /// Which agent program a CLI runtime found, when it can be one of
    /// several (Antigravity's `agy` or its legacy ACP runtime).
    pub launch: Option<CatalogLaunch>,
}

/// The agent program behind a CLI runtime row (the gateway's
/// `CatalogLaunch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogLaunch {
    /// The vendor's own CLI: its sign-in command signs it in.
    Native,
    /// A standalone runtime another app installed: it has no sign-in
    /// command; installing the vendor's CLI is the way to sign in.
    Legacy,
    /// A program this client does not know yet.
    #[serde(other)]
    Other,
}

impl RouteProvider {
    /// Signed out on a runtime that has no sign-in command of its own.
    pub fn needs_native_cli(&self) -> bool {
        !self.authenticated && self.launch == Some(CatalogLaunch::Legacy)
    }

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

/// Every provider a thread can run on, and the picker preferences.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderCatalog {
    pub providers: Vec<RouteProvider>,
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

    /// Remove a provider from ZLO (`true`) or bring it back. A removed
    /// provider is out of the model picker entirely.
    pub fn set_provider_hidden(&mut self, provider: &str, hidden: bool) {
        set(&mut self.hidden_providers, provider.to_string(), hidden);
    }

    pub fn set_model_hidden(&mut self, provider: &str, model: &str, hidden: bool) {
        set(
            &mut self.hidden_models,
            PickerModel::new(provider, model),
            hidden,
        );
    }

    pub fn set_favorite(&mut self, provider: &str, model: &str, on: bool) {
        set(&mut self.favorites, PickerModel::new(provider, model), on);
    }
}

/// Put `item` in `list` (once) or take it out: unlike [`toggle`], applying
/// it twice, or to a fresher copy, gives the same result.
fn set<T: PartialEq>(list: &mut Vec<T>, item: T, on: bool) {
    let at = list.iter().position(|x| *x == item);
    match (at, on) {
        (None, true) => list.push(item),
        (Some(ix), false) => {
            list.remove(ix);
        }
        _ => {}
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
    /// The thread route's provider lists without a thread (`GET /providers`).
    pub async fn providers(&self) -> Result<ProviderCatalog, ClientError> {
        self.orch_get(format!("{V2}/providers")).await
    }

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
    /// Re-read the picker preferences, apply `edit`, and save them. The
    /// chat's picker and Settings each hold a copy; saving a copy read
    /// before the other view changed it would undo that change (a removed
    /// provider coming back), so every edit starts from the gateway's.
    /// Edits from this app run one at a time: two read before either saved
    /// would each write back a document missing the other's change. (The
    /// gateway takes the whole document, with no version to check.)
    pub async fn edit_model_picker(
        &self,
        thread_id: &str,
        edit: impl FnOnce(&mut PickerPrefs),
    ) -> Result<PickerPrefs, ClientError> {
        static EDITS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _one_at_a_time = EDITS.lock().await;
        let mut prefs = self.providers().await?.picker;
        edit(&mut prefs);
        self.set_model_picker(thread_id, &prefs).await?;
        Ok(prefs)
    }

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

impl Client {
    /// A runtime-native thread operation (`POST /threads/{id}/action`):
    /// a Codex review (runs as a turn), revert of the last turn, or
    /// compaction. A runtime without it answers [`ClientError::Refused`].
    pub async fn thread_action(
        &self,
        thread_id: &str,
        action: &ironclaw_product_contracts::orchestration::ThreadAction,
    ) -> Result<ironclaw_product_contracts::orchestration::ThreadActionOutcome, ClientError> {
        let body = serde_json::to_value(action)
            .map_err(|e| ClientError::Decode(format!("thread action: {e}")))?;
        self.orch_post(format!("{V2}/threads/{}/action", seg(thread_id)), body)
            .await
    }
}
