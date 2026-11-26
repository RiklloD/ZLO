//! Several machines: orchestrator tools that run on a linked environment,
//! tasks delegated there, and the receiving side's fence.
//!
//! **Linking side.** A tool call whose input names `environment` (a linked
//! environment's id or label, or `auto` for `t3_delegate`) runs there through
//! the host's [`PeerLinkService`]. `t3_delegate` to a linked environment is
//! recorded here as an ordinary task without a child thread: its child is an
//! ordinary thread *there*, and a follower on the tick polls it and finalizes
//! the task here, so the parent wakes exactly as for a local child. The
//! follower survives restarts (`orch_remote_tasks`), backs off while the other
//! machine is unreachable, and carries Stop across (`cancel_pending`).
//!
//! **Receiving side.** [`PeerReceiver`] runs a linked environment's calls
//! here. Each link gets one hidden parent thread (`peer-link-<device>`), so a
//! link's tasks are its children and `t3_task` already sees only those. Below
//! full access every thread-targeting call is fenced to threads the link
//! started, schedules are refused, and children never run on "Full access"
//! (`orch_link_origins`, read by [`Engine::link_origin`]).

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::environment_access::PeerLinkAccess;
use ironclaw_product_contracts::peer::{
    BalancePreference, EnvironmentResources, PeerCaller, PeerLinkError, PeerLinkService,
    PeerLinkSummary, PeerToolHandler, PeerToolRequest,
};
use libsql::Value as Sql;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::delegation::DelegateRequest;
use crate::engine::{CallContext, Engine, Result, clip_title, json_ok};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::store::{Cols, Store, TaskPatch, s};

pub(crate) const PEER_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_remote_tasks (
  task_id TEXT PRIMARY KEY,
  environment_id TEXT NOT NULL,
  environment_label TEXT NOT NULL,
  request_key TEXT NOT NULL,
  launch TEXT NOT NULL,
  remote_task_id TEXT,
  remote_thread_id TEXT,
  next_poll_at INTEGER NOT NULL,
  failures INTEGER NOT NULL DEFAULT 0,
  cancel_pending INTEGER NOT NULL DEFAULT 0,
  cancel_reason TEXT,
  last_error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS orch_remote_tasks_poll ON orch_remote_tasks(next_poll_at);
CREATE TABLE IF NOT EXISTS orch_link_origins (
  thread_id TEXT PRIMARY KEY,
  device_id TEXT NOT NULL,
  label TEXT NOT NULL,
  access TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
"#;

/// How often a followed remote task is read while all is well.
const POLL_MS: i64 = 5_000;
/// Longest back-off for an unreachable machine.
const MAX_BACKOFF_MS: i64 = 5 * 60_000;
/// Consecutive defects (answers that make no sense) before a task fails.
const MAX_DEFECTS: i64 = 8;
/// The runtime id a remote task reports.
const PEER_RUNTIME: &str = "peer";

/// A task here whose child runs on a linked environment.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTaskRecord {
    pub task_id: TaskId,
    pub environment_id: String,
    pub environment_label: String,
    /// Retry key the remote launch carries (idempotent there).
    pub request_key: String,
    /// The `t3_delegate` input sent there.
    pub launch: Value,
    pub remote_task_id: Option<String>,
    pub remote_thread_id: Option<String>,
    pub next_poll_at: Millis,
    pub failures: i64,
    pub cancel_pending: bool,
    pub cancel_reason: Option<String>,
    pub last_error: Option<String>,
}

/// Where linked work here came from (receiving side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkOrigin {
    pub device_id: String,
    pub label: String,
    pub access: PeerLinkAccess,
}

fn remote_row(r: &libsql::Row) -> Result<RemoteTaskRecord> {
    let mut c = Cols::new(r);
    Ok(RemoteTaskRecord {
        task_id: TaskId::new(c.text()?)?,
        environment_id: c.text()?,
        environment_label: c.text()?,
        request_key: c.text()?,
        launch: serde_json::from_str(&c.text()?)?,
        remote_task_id: c.otext()?,
        remote_thread_id: c.otext()?,
        next_poll_at: c.int()?,
        failures: c.int()?,
        cancel_pending: c.int()? != 0,
        cancel_reason: c.otext()?,
        last_error: c.otext()?,
    })
}

const REMOTE_COLS: &str = "task_id, environment_id, environment_label, request_key, launch, \
    remote_task_id, remote_thread_id, next_poll_at, failures, cancel_pending, cancel_reason, last_error";

impl Store {
    pub(crate) async fn insert_remote_task(&self, r: &RemoteTaskRecord) -> Result<()> {
        let now = now_ms();
        self.exec(
            "INSERT INTO orch_remote_tasks (task_id, environment_id, environment_label, request_key, \
             launch, remote_task_id, remote_thread_id, next_poll_at, failures, cancel_pending, \
             cancel_reason, last_error, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, NULL, NULL, ?9, ?9)",
            vec![
                s(r.task_id.as_str()),
                s(&r.environment_id),
                s(&r.environment_label),
                s(&r.request_key),
                s(serde_json::to_string(&r.launch)?),
                r.remote_task_id.as_deref().map_or(Sql::Null, s),
                r.remote_thread_id.as_deref().map_or(Sql::Null, s),
                Sql::Integer(r.next_poll_at),
                Sql::Integer(now),
            ],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn remote_task(&self, task: &TaskId) -> Result<Option<RemoteTaskRecord>> {
        self.row(
            &format!("SELECT {REMOTE_COLS} FROM orch_remote_tasks WHERE task_id = ?1"),
            vec![s(task.as_str())],
            remote_row,
        )
        .await
    }

    /// Rows due for the follower: open tasks, and cancels still to carry.
    pub(crate) async fn due_remote_tasks(&self, now: Millis) -> Result<Vec<RemoteTaskRecord>> {
        self.rows(
            "SELECT r.task_id, r.environment_id, r.environment_label, r.request_key, r.launch, \
             r.remote_task_id, r.remote_thread_id, r.next_poll_at, r.failures, r.cancel_pending, \
             r.cancel_reason, r.last_error FROM orch_remote_tasks r \
             JOIN orch_tasks t ON t.id = r.task_id \
             WHERE r.next_poll_at <= ?1 AND (r.cancel_pending = 1 OR t.status NOT IN \
             ('completed', 'failed', 'cancelled', 'interrupted')) ORDER BY r.next_poll_at LIMIT 64",
            vec![Sql::Integer(now)],
            remote_row,
        )
        .await
    }

    pub(crate) async fn set_remote_launched(
        &self,
        task: &TaskId,
        remote_task: &str,
        remote_thread: Option<&str>,
        next_poll_at: Millis,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_remote_tasks SET remote_task_id = ?2, remote_thread_id = ?3, \
             next_poll_at = ?4, failures = 0, last_error = NULL, updated_at = ?5 WHERE task_id = ?1",
            vec![
                s(task.as_str()),
                s(remote_task),
                remote_thread.map_or(Sql::Null, s),
                Sql::Integer(next_poll_at),
                Sql::Integer(now_ms()),
            ],
        )
        .await?;
        Ok(())
    }

    /// Schedule the next read; `failure` counts a consecutive failure.
    pub(crate) async fn set_remote_poll(
        &self,
        task: &TaskId,
        next_poll_at: Millis,
        failure: Option<&str>,
    ) -> Result<()> {
        match failure {
            Some(error) => {
                self.exec(
                    "UPDATE orch_remote_tasks SET next_poll_at = ?2, failures = failures + 1, \
                     last_error = ?3, updated_at = ?4 WHERE task_id = ?1",
                    vec![
                        s(task.as_str()),
                        Sql::Integer(next_poll_at),
                        s(error),
                        Sql::Integer(now_ms()),
                    ],
                )
                .await?
            }
            None => {
                self.exec(
                    "UPDATE orch_remote_tasks SET next_poll_at = ?2, failures = 0, last_error = NULL, \
                     updated_at = ?3 WHERE task_id = ?1",
                    vec![s(task.as_str()), Sql::Integer(next_poll_at), Sql::Integer(now_ms())],
                )
                .await?
            }
        };
        Ok(())
    }

    pub(crate) async fn set_remote_cancel(
        &self,
        task: &TaskId,
        pending: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_remote_tasks SET cancel_pending = ?2, \
             cancel_reason = COALESCE(?3, cancel_reason), next_poll_at = CASE WHEN ?2 = 1 THEN 0 \
             ELSE next_poll_at END, updated_at = ?4 WHERE task_id = ?1",
            vec![
                s(task.as_str()),
                Sql::Integer(i64::from(pending)),
                reason.map_or(Sql::Null, s),
                Sql::Integer(now_ms()),
            ],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn put_link_origin(
        &self,
        thread: &ThreadId,
        origin: &LinkOrigin,
    ) -> Result<()> {
        self.exec(
            "INSERT INTO orch_link_origins (thread_id, device_id, label, access, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(thread_id) DO UPDATE SET label = excluded.label, \
             access = excluded.access",
            vec![
                s(thread.as_str()),
                s(&origin.device_id),
                s(&origin.label),
                s(origin.access.as_str()),
                Sql::Integer(now_ms()),
            ],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn link_origin(&self, thread: &ThreadId) -> Result<Option<LinkOrigin>> {
        self.row(
            "SELECT device_id, label, access FROM orch_link_origins WHERE thread_id = ?1",
            vec![s(thread.as_str())],
            |r| {
                let mut c = Cols::new(r);
                let device_id = c.text()?;
                let label = c.text()?;
                let access = c.text()?;
                Ok(LinkOrigin {
                    device_id,
                    label,
                    // An unreadable access is fenced, never full.
                    access: PeerLinkAccess::parse(&access).unwrap_or(PeerLinkAccess::ReadOnly),
                })
            },
        )
        .await
    }
}

/// The receiving side's hidden parent thread of one link.
pub(crate) fn link_thread_id(device_id: &str) -> Result<ThreadId> {
    Ok(ThreadId::new(format!("peer-link-{device_id}"))?)
}

/// A key that names the calling thread without revealing it.
fn caller_key(thread: &ThreadId) -> String {
    let digest = Sha256::digest(thread.as_str().as_bytes());
    hex::encode(&digest[..12])
}

/// The retry key a remote launch carries: the caller's namespace plus its
/// own key, so two callers reusing a key never collide on the far side.
fn remote_request_key(caller: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("{caller}\u{0}{key}").as_bytes());
    format!("peer_{}", hex::encode(&digest[..16]))
}

fn peer_error(error: PeerLinkError) -> OrchestratorError {
    OrchestratorError::request(error.code(), error.to_string())
}

fn without_environment(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(map) = input.as_object_mut() {
        map.remove("environment");
    }
    input
}

/// The `environment` an input names, unless it names this machine.
fn named_environment(input: &Value) -> Option<String> {
    input
        .get("environment")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty() && !matches!(*raw, "local" | "here" | "this"))
        .map(str::to_string)
}

impl Engine {
    /// Install the host's peer links (first call wins).
    pub fn set_peer_links(&self, links: Arc<dyn PeerLinkService>) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "first call wins: the host wires one peer-link service per process"
        )]
        let _ = self.peer_links.set(links);
    }

    pub(crate) fn peers(&self) -> Result<&Arc<dyn PeerLinkService>> {
        self.peer_links.get().ok_or_else(|| {
            OrchestratorError::request(
                "peer_links_unavailable",
                "Linked environments are not available on this machine.",
            )
        })
    }

    /// Where linked work here came from, when it did.
    pub async fn link_origin(&self, thread: &ThreadId) -> Result<Option<LinkOrigin>> {
        self.store.link_origin(thread).await
    }

    /// The link `raw` names: an id, or a label (ignoring case).
    async fn resolve_link(&self, raw: &str) -> Result<PeerLinkSummary> {
        let links = self.peers()?.links().await.map_err(peer_error)?;
        let lower = raw.to_ascii_lowercase();
        links
            .iter()
            .find(|l| l.environment_id == raw)
            .or_else(|| links.iter().find(|l| l.label.to_ascii_lowercase() == lower))
            .cloned()
            .ok_or_else(|| {
                let names: Vec<&str> = links.iter().map(|l| l.label.as_str()).collect();
                OrchestratorError::request(
                    "not_linked",
                    if names.is_empty() {
                        format!(
                            "No linked environment named {raw:?}. Ask the person to link it \
                             (t3_environment_link)."
                        )
                    } else {
                        format!(
                            "No linked environment named {raw:?}. Linked: {}. Or ask the person \
                             to link it (t3_environment_link).",
                            names.join(", ")
                        )
                    },
                )
            })
    }

    /// The calling agent's own access: "Full access" profiles may run
    /// full-access work elsewhere, every other agent at most supervised work.
    async fn caller_mode(&self, thread: &ThreadId) -> PeerLinkAccess {
        if let Ok(Some(origin)) = self.store.link_origin(thread).await {
            // Linked work never reaches further than the link it came from.
            return origin.access.min(PeerLinkAccess::Operate);
        }
        match self.store.thread_profile(thread).await {
            Ok(Some(profile)) if profile.settings.auto_approve(false) => PeerLinkAccess::Full,
            _ => PeerLinkAccess::Operate,
        }
    }

    async fn peer_request(&self, ctx: &CallContext, input: Value) -> Result<PeerToolRequest> {
        let origin_label = match self.peers()?.local_environment().await {
            Ok((_, label)) => label,
            Err(_) => "another machine".to_string(),
        };
        Ok(PeerToolRequest {
            input,
            mode_limit: self.caller_mode(&ctx.thread_id).await,
            caller_key: caller_key(&ctx.thread_id),
            origin_label,
        })
    }

    /// Run a tool whose input names another environment there. `None` when
    /// the input names this machine (the caller runs it here).
    pub(crate) async fn forward_tool(
        &self,
        name: &str,
        ctx: &CallContext,
        input: &Value,
    ) -> Option<Result<Value>> {
        let raw = named_environment(input)?;
        if self.peer_links.get().is_none() {
            return Some(Err(OrchestratorError::request(
                "peer_links_unavailable",
                "Linked environments are not available on this machine; omit environment.",
            )));
        }
        Some(self.forward_named(name, ctx, input, &raw).await)
    }

    async fn forward_named(
        &self,
        name: &str,
        ctx: &CallContext,
        input: &Value,
        raw: &str,
    ) -> Result<Value> {
        match name {
            "t3_request_secret" | "t3_environment_link" => {
                return Err(OrchestratorError::invalid(format!(
                    "{name} acts as this thread and cannot run on another environment; omit environment."
                )));
            }
            "t3_delegate" if raw.eq_ignore_ascii_case("auto") => {
                return match self.pick_environment(ctx).await? {
                    None => {
                        let req: DelegateRequest =
                            serde_json::from_value(without_environment(input)).map_err(|e| {
                                OrchestratorError::invalid(format!("invalid arguments: {e}"))
                            })?;
                        let mut view = self.delegate(ctx, req).await?;
                        view["environment"] = json!("local");
                        Ok(view)
                    }
                    Some(link) => self.remote_delegate(ctx, &link, input).await,
                };
            }
            _ => {}
        }
        let link = self.resolve_link(raw).await?;
        if name == "t3_delegate" {
            return self.remote_delegate(ctx, &link, input).await;
        }
        if name == "t3_task"
            && let Some(local) = self.remote_task_for_local_view(input).await?
        {
            // A task id from here that follows a remote child: answer locally.
            return Ok(local);
        }
        let request = self.peer_request(ctx, without_environment(input)).await?;
        let mut value = self
            .peers()?
            .call_tool(&link.environment_id, name, request)
            .await
            .map_err(peer_error)?;
        if let Some(map) = value.as_object_mut() {
            map.insert("environment".into(), json!(link.label));
            map.insert("environmentId".into(), json!(link.environment_id));
        }
        Ok(value)
    }

    /// `t3_task` with a local task id needs no hop.
    async fn remote_task_for_local_view(&self, input: &Value) -> Result<Option<Value>> {
        let Some(id) = input.get("task_id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Ok(id) = TaskId::new(id) else {
            return Ok(None);
        };
        if self.store.remote_task(&id).await?.is_none() {
            return Ok(None);
        }
        Err(OrchestratorError::invalid(
            "That task is followed from this machine: call t3_task without environment.",
        ))
    }

    /// `t3_delegate` to a linked environment: a local task, launched there.
    async fn remote_delegate(
        &self,
        ctx: &CallContext,
        link: &PeerLinkSummary,
        input: &Value,
    ) -> Result<Value> {
        let req: DelegateRequest = serde_json::from_value(without_environment(input))
            .map_err(|e| OrchestratorError::invalid(format!("invalid arguments: {e}")))?;
        let text = req.task.trim().to_string();
        if text.is_empty() {
            return Err(OrchestratorError::invalid("task is required"));
        }
        let inherit = match req.context.as_deref().map(str::trim) {
            None | Some("") | Some("brief") => false,
            Some("inherit") => true,
            Some(_) => {
                return Err(OrchestratorError::invalid(
                    "context must be inherit or brief",
                ));
            }
        };
        if link.access == PeerLinkAccess::ReadOnly {
            return Err(OrchestratorError::request(
                "peer_refused",
                format!(
                    "The link to {} is read-only; ask the person to link it again with more access.",
                    link.label
                ),
            ));
        }
        let role = match req.role.as_deref() {
            None | Some("") => TaskRole::General,
            Some(raw) => TaskRole::parse(raw).map_err(|_| {
                OrchestratorError::invalid(
                    "role must be one of implementation, research, review, design, test, general",
                )
            })?,
        };
        let wait = match req.mode.as_deref() {
            None | Some("async") => false,
            Some("wait") => true,
            Some(_) => return Err(OrchestratorError::invalid("mode must be async or wait")),
        };
        let timeout = req.timeout_s.unwrap_or(600.0).clamp(1.0, 3600.0);
        let parent = self.ensure_thread(&ctx.thread_id, &ctx.scope, None).await?;
        let request_id = req
            .client_request_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(rid) = &request_id
            && let Some(existing) = self.store.task_by_request(&parent.thread_id, rid).await?
        {
            return self.maybe_wait(existing, wait, timeout).await;
        }
        let settings = self.store.settings().await?;
        let parent_task = self.store.task_for_child(&parent.thread_id).await?;
        let depth = parent_task.as_ref().map_or(1, |t| t.depth + 1);
        if depth > settings.max_depth {
            return Err(OrchestratorError::request(
                "depth_exceeded",
                format!("Delegation depth limit ({}) reached.", settings.max_depth),
            ));
        }
        let active = self.store.tasks_for_parent(&parent.thread_id, true).await?;
        if active.len() as u32 >= settings.max_running_per_parent {
            return Err(OrchestratorError::request(
                "too_many_tasks",
                format!(
                    "{} delegated tasks are already active for this thread.",
                    active.len()
                ),
            ));
        }
        let title = clip_title(
            req.title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(&text),
        );
        let target = req.target.clone().unwrap_or_default();
        let task = TaskRecord {
            id: TaskId::generate(),
            parent_thread: parent.thread_id.clone(),
            child_thread: None,
            root_thread: parent_task
                .as_ref()
                .map_or_else(|| parent.thread_id.clone(), |t| t.root_thread.clone()),
            depth,
            profile_id: None,
            parent_profile_id: parent.profile_id.clone(),
            title: title.clone(),
            role,
            runtime_id: RuntimeId::new(PEER_RUNTIME)?,
            provider: target.provider.clone(),
            model: target.model.clone(),
            prompt: text.clone(),
            status: TaskStatus::Starting,
            result: None,
            error: None,
            progress: None,
            usage: None,
            created_at: now_ms(),
            started_at: None,
            finished_at: None,
            client_request_id: request_id.clone(),
            delivery: Delivery::None,
            delivered_at: None,
            cancel_requested: false,
            limit_reset_at: None,
        };
        let key = remote_request_key(
            &caller_key(&ctx.thread_id),
            request_id.as_deref().unwrap_or(task.id.as_str()),
        );
        let mut launch = json!({
            "task": text,
            "title": title,
            "role": role.as_str(),
            "context": "brief",
            "mode": "async",
            "client_request_id": key,
            "parent": {
                "runtime": parent.runtime_id.as_str(),
                "provider": parent.provider,
                "model": parent.model,
            },
        });
        // The conversation so far travels as text: the other machine has no
        // copy of this thread.
        if inherit && let Some(context) = self.inherited_context(&parent, &settings).await? {
            launch["context"] = json!("inherit");
            launch["handoff_context"] = json!(context);
        }
        if req.target.is_some() {
            launch["target"] = serde_json::to_value(&target)?;
        } else if inherit {
            // Same route as this thread, as a local inherit would keep.
            launch["target"] = json!({
                "provider": parent.provider.clone().unwrap_or_else(|| parent.runtime_id.as_str().to_string()),
                "model": parent.model,
            });
        }
        // This thread's folder: its pushed base and what is only here.
        if let Some(workspace) = self.peer_workspace(&parent).await {
            launch["workspace"] = workspace;
        }
        if let Some(isolation) = &req.isolation {
            launch["isolation"] = json!(isolation);
        }
        self.store.insert_task(&task).await?;
        self.store.set_hold(&parent.thread_id, None).await?;
        self.store
            .insert_remote_task(&RemoteTaskRecord {
                task_id: task.id.clone(),
                environment_id: link.environment_id.clone(),
                environment_label: link.label.clone(),
                request_key: key,
                launch,
                remote_task_id: None,
                remote_thread_id: None,
                next_poll_at: now_ms() + POLL_MS,
                failures: 0,
                cancel_pending: false,
                cancel_reason: None,
                last_error: None,
            })
            .await?;
        let record = self
            .store
            .remote_task(&task.id)
            .await?
            .ok_or_else(|| OrchestratorError::Corrupt("remote task vanished".into()))?;
        if let Err(error) = self.launch_remote(ctx.thread_id.clone(), &record).await {
            if !error.is_retryable() {
                let message = error.to_string();
                self.fail_remote(
                    &task.id,
                    &format!("could not start on {}: {message}", link.label),
                )
                .await?;
                return Err(OrchestratorError::request(
                    error.code(),
                    format!("Could not start the task on {}: {message}", link.label),
                ));
            }
            // Retried by the follower with the same key.
            self.store
                .set_remote_poll(&task.id, now_ms() + POLL_MS, Some(&error.to_string()))
                .await?;
        }
        let current = self.require_task(&task.id).await?;
        let mut view = self.maybe_wait(current, wait, timeout).await?;
        view["environment"] = json!(link.label);
        view["environmentId"] = json!(link.environment_id);
        Ok(view)
    }

    /// Start (or re-find, by its key) the remote child.
    async fn launch_remote(
        &self,
        caller: ThreadId,
        record: &RemoteTaskRecord,
    ) -> std::result::Result<(), PeerLinkError> {
        let peers = self.peers().map_err(|_| PeerLinkError::Unavailable)?;
        let origin_label = peers
            .local_environment()
            .await
            .map(|(_, label)| label)
            .unwrap_or_else(|_| "another machine".into());
        let request = PeerToolRequest {
            input: record.launch.clone(),
            mode_limit: self.caller_mode(&caller).await,
            caller_key: caller_key(&caller),
            origin_label,
        };
        let answer = peers
            .call_tool(&record.environment_id, "t3_delegate", request)
            .await?;
        if answer.get("ok") == Some(&Value::Bool(false)) {
            return Err(PeerLinkError::Refused(
                answer["message"]
                    .as_str()
                    .unwrap_or("the other machine refused the task")
                    .to_string(),
            ));
        }
        let Some(remote_task) = answer["taskId"].as_str() else {
            return Err(PeerLinkError::Refused(
                "the other machine answered without a task".into(),
            ));
        };
        self.store
            .set_remote_launched(
                &record.task_id,
                remote_task,
                answer["childThreadId"].as_str(),
                now_ms() + POLL_MS,
            )
            .await
            .map_err(|_| PeerLinkError::Unavailable)?;
        // Waiting on the remote child to start is the follower's job.
        if let Err(error) = self
            .store
            .move_task(
                &record.task_id,
                &[TaskStatus::Starting],
                false,
                TaskStatus::Running,
                TaskPatch {
                    started_at: Some(now_ms()),
                    ..TaskPatch::default()
                },
            )
            .await
        {
            tracing::debug!(%error, "remote task start not recorded");
        }
        Ok(())
    }

    async fn fail_remote(&self, task: &TaskId, message: &str) -> Result<()> {
        let current = self.require_task(task).await?;
        if !current.status.is_terminal() {
            self.finalize(
                &current,
                TaskStatus::Failed,
                None,
                Some(message.to_string()),
            )
            .await?;
        }
        Ok(())
    }

    /// Cancel intent for a task, carried to its remote child by the follower.
    pub(crate) async fn mark_remote_cancel(&self, task: &TaskId, reason: &str) -> Result<()> {
        if self.store.remote_task(task).await?.is_some() {
            self.store
                .set_remote_cancel(task, true, Some(reason))
                .await?;
        }
        Ok(())
    }

    /// Tick step: follow every due remote task.
    pub(crate) async fn follow_remote_tasks(&self) -> Result<()> {
        let Some(peers) = self.peer_links.get().cloned() else {
            return Ok(());
        };
        let now = now_ms();
        for record in self.store.due_remote_tasks(now).await? {
            if let Err(error) = self.follow_one(&peers, &record).await {
                tracing::debug!(%error, task = %record.task_id, "remote task follow failed");
            }
        }
        Ok(())
    }

    fn backoff(failures: i64) -> i64 {
        let exp = failures.clamp(0, 10) as u32;
        (POLL_MS * 2_i64.pow(exp)).min(MAX_BACKOFF_MS)
    }

    async fn follow_one(
        &self,
        peers: &Arc<dyn PeerLinkService>,
        record: &RemoteTaskRecord,
    ) -> Result<()> {
        let task = self.require_task(&record.task_id).await?;
        let caller = task.parent_thread.clone();
        // Never launched yet (the first try failed retryably): try again.
        let Some(remote_task) = record.remote_task_id.clone() else {
            if task.status.is_terminal() {
                return self
                    .store
                    .set_remote_cancel(&record.task_id, false, None)
                    .await;
            }
            return match self.launch_remote(caller, record).await {
                Ok(()) => Ok(()),
                Err(error) if error.is_retryable() => {
                    self.store
                        .set_remote_poll(
                            &record.task_id,
                            now_ms() + Self::backoff(record.failures + 1),
                            Some(&error.to_string()),
                        )
                        .await
                }
                Err(error) => {
                    self.fail_remote(
                        &record.task_id,
                        &format!("could not start on {}: {error}", record.environment_label),
                    )
                    .await
                }
            };
        };
        let request = |input: Value| async {
            let mode_limit = self.caller_mode(&caller).await;
            PeerToolRequest {
                input,
                mode_limit,
                caller_key: caller_key(&caller),
                origin_label: String::new(),
            }
        };
        if record.cancel_pending {
            let reason = record
                .cancel_reason
                .clone()
                .unwrap_or_else(|| "cancelled by the parent".into());
            let call = peers
                .call_tool(
                    &record.environment_id,
                    "t3_task",
                    request(json!({"action": "cancel", "task_id": remote_task, "reason": reason}))
                        .await,
                )
                .await;
            return match call {
                Ok(_) | Err(PeerLinkError::NotLinked(_) | PeerLinkError::Revoked(_)) => {
                    self.store
                        .set_remote_cancel(&record.task_id, false, None)
                        .await?;
                    if !task.status.is_terminal() {
                        self.finalize(&task, TaskStatus::Cancelled, None, Some(reason))
                            .await?;
                    }
                    Ok(())
                }
                Err(error) => {
                    self.store
                        .set_remote_poll(
                            &record.task_id,
                            now_ms() + Self::backoff(record.failures + 1),
                            Some(&error.to_string()),
                        )
                        .await
                }
            };
        }
        let answer = peers
            .call_tool(
                &record.environment_id,
                "t3_task",
                request(json!({"action": "status", "task_id": remote_task})).await,
            )
            .await;
        let view = match answer {
            Ok(view) => view,
            Err(error @ (PeerLinkError::NotLinked(_) | PeerLinkError::Revoked(_))) => {
                return self
                    .fail_remote(
                        &record.task_id,
                        &format!("{error}; the task there may still be running"),
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .store
                    .set_remote_poll(
                        &record.task_id,
                        now_ms() + Self::backoff(record.failures + 1),
                        Some(&error.to_string()),
                    )
                    .await;
            }
        };
        let status = view["status"]
            .as_str()
            .and_then(|raw| TaskStatus::parse(raw).ok());
        let Some(status) = status else {
            if record.failures + 1 >= MAX_DEFECTS {
                return self
                    .fail_remote(
                        &record.task_id,
                        &format!(
                            "{} kept answering without a task status: {}",
                            record.environment_label,
                            clip(&view.to_string(), 300)
                        ),
                    )
                    .await;
            }
            return self
                .store
                .set_remote_poll(
                    &record.task_id,
                    now_ms() + Self::backoff(record.failures + 1),
                    Some("answer without a status"),
                )
                .await;
        };
        if let Some(progress) = view.get("progress").filter(|p| !p.is_null()) {
            let mut progress = progress.clone();
            progress["environment"] = json!(record.environment_label);
            self.store
                .set_task_progress(&record.task_id, &progress)
                .await?;
        }
        if status.is_terminal() {
            let result = view["summary"]
                .as_str()
                .map(|r| format!("{r}\n\n(Ran on {}.)", record.environment_label));
            let error = view["error"].as_str().map(str::to_string);
            let status = match status {
                TaskStatus::Interrupted => TaskStatus::Failed,
                other => other,
            };
            self.finalize(&task, status, result, error).await?;
            return Ok(());
        }
        self.store
            .set_remote_poll(&record.task_id, now_ms() + POLL_MS, None)
            .await
    }

    /// `environment: "auto"`: the machine with the most headroom, weighted
    /// by each link's preference. `None` picks this machine.
    async fn pick_environment(&self, ctx: &CallContext) -> Result<Option<PeerLinkSummary>> {
        let peers = self.peers()?;
        let mut best: Option<(f64, Option<PeerLinkSummary>)> = None;
        let local = self.local_resources().await;
        if !local.is_full() {
            best = Some((local.pressure(), None));
        }
        let mode = self.caller_mode(&ctx.thread_id).await;
        for link in peers.links().await.map_err(peer_error)? {
            if link.revoked
                || link.access == PeerLinkAccess::ReadOnly
                || mode == PeerLinkAccess::ReadOnly
            {
                continue;
            }
            let Some(weight) = link.preference.weight() else {
                continue;
            };
            let Ok(resources) = peers.resources(&link.environment_id).await else {
                continue;
            };
            if resources.is_full() {
                continue;
            }
            let score = resources.pressure() * weight;
            if best.as_ref().is_none_or(|(s, _)| score < *s) {
                best = Some((score, Some(link)));
            }
        }
        match best {
            Some((_, choice)) => Ok(choice),
            None => Err(OrchestratorError::request(
                "all_machines_busy",
                "Every machine is busy or excluded from automatic selection; name one with environment.",
            )),
        }
    }

    /// This machine's headroom (CPU load, memory, running turns).
    pub async fn local_resources(&self) -> EnvironmentResources {
        let mut resources = crate::resources::probe().await;
        resources.active_turns = self.store.running_threads().await.unwrap_or(0);
        resources
    }

    /// `t3_capabilities`' `environments`: this machine and every link.
    pub(crate) async fn environments_view(&self) -> Value {
        let Some(peers) = self.peer_links.get() else {
            return Value::Null;
        };
        let local = match peers.local_environment().await {
            Ok((id, label)) => json!({"environmentId": id.as_str(), "label": label, "local": true}),
            Err(_) => json!({"local": true}),
        };
        let links: Vec<Value> = match peers.links().await {
            Ok(links) => links
                .into_iter()
                .map(|l| {
                    json!({
                        "environmentId": l.environment_id, "label": l.label,
                        "access": l.access.as_str(), "preference": l.preference,
                        "expiresAt": l.expires_at, "revoked": l.revoked,
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        json!({
            "this": local,
            "linked": links,
            "hint": "Pass environment (id or label) to any t3_* tool to run it there; \
                     t3_delegate also takes environment='auto' to pick the machine with the most headroom.",
        })
    }
}

/// Runs linked environments' tool calls on this machine, as `scope` (the
/// installation's operator).
pub struct PeerReceiver {
    engine: Arc<Engine>,
    scope: ThreadScope,
}

impl PeerReceiver {
    pub fn new(engine: Arc<Engine>, scope: ThreadScope) -> Self {
        Self { engine, scope }
    }

    async fn run(&self, caller: &PeerCaller, tool: &str, input: Value) -> Result<Value> {
        let engine = &self.engine;
        let link_thread = link_thread_id(caller.device_id.as_str())?;
        let origin = LinkOrigin {
            device_id: caller.device_id.as_str().to_string(),
            label: caller.label.clone(),
            access: caller.access,
        };
        let mut parent = ThreadRecord::new(
            link_thread.clone(),
            self.scope.clone(),
            engine.default_runtime.clone(),
        );
        parent.title = Some(format!("Linked from {}", caller.label));
        engine.store.ensure_thread(&parent).await?;
        engine.store.put_link_origin(&link_thread, &origin).await?;
        let ctx = CallContext {
            thread_id: link_thread.clone(),
            scope: self.scope.clone(),
            provider: None,
            model: None,
        };
        let operate = caller.access >= PeerLinkAccess::Operate;
        let full = caller.access == PeerLinkAccess::Full;
        let refuse = |what: &str| {
            Err(OrchestratorError::request(
                "peer_refused",
                format!(
                    "The link from {} has {} access here, which does not allow {what}.",
                    caller.label,
                    caller.access.as_str()
                ),
            ))
        };
        let action = input["action"].as_str().unwrap_or_default().to_string();
        match tool {
            "t3_capabilities" => engine.capabilities(&ctx).await,
            "t3_task" => {
                if action == "cancel" && !operate {
                    return refuse("stopping tasks");
                }
                engine.call_tool_checked(tool, &ctx, &input).await
            }
            "t3_delegate" => {
                if !operate {
                    return refuse("starting tasks");
                }
                let mut req: DelegateRequest = serde_json::from_value(input.clone())
                    .map_err(|e| OrchestratorError::invalid(format!("invalid arguments: {e}")))?;
                // The sender's conversation rides in the brief (`handoff_context`).
                if let Some(context) = input["handoff_context"]
                    .as_str()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                {
                    req.task = format!("{context}\n\nTask:\n{}", req.task);
                }
                req.context = Some("brief".into());
                req.peer_origin = Some(origin.clone());
                engine.delegate(&ctx, req).await
            }
            "t3_thread" | "t3_queue" => {
                let reading = match tool {
                    "t3_thread" => {
                        matches!(action.as_str(), "runs" | "lineage" | "worktree_status")
                    }
                    _ => action == "list",
                };
                if !reading && !operate {
                    return refuse("changing threads");
                }
                let Some(thread) = input["thread"].as_str().filter(|t| !t.trim().is_empty()) else {
                    return Err(OrchestratorError::invalid(
                        "Name the thread: a linked environment has no thread of its own here.",
                    ));
                };
                let thread = ThreadId::new(thread)?;
                if !full && !self.started_by_link(&thread, &link_thread).await? {
                    return refuse("threads it did not start");
                }
                engine.call_tool_checked(tool, &ctx, &input).await
            }
            "t3_schedule" => {
                if !full {
                    return refuse("schedules");
                }
                engine.call_tool_checked(tool, &ctx, &input).await
            }
            other => Err(OrchestratorError::invalid(format!(
                "{other} cannot run through a link."
            ))),
        }
    }

    /// Whether `thread` is a link child (or a descendant of one).
    async fn started_by_link(&self, thread: &ThreadId, link_thread: &ThreadId) -> Result<bool> {
        let Some(task) = self.engine.store.task_for_child(thread).await? else {
            return Ok(false);
        };
        Ok(&task.root_thread == link_thread || &task.parent_thread == link_thread)
    }
}

#[async_trait]
impl PeerToolHandler for PeerReceiver {
    async fn call_peer_tool(&self, caller: PeerCaller, tool: &str, input: Value) -> Value {
        match self.run(&caller, tool, input).await {
            Ok(value) => json_ok(value),
            Err(error) => {
                if !error.is_correctable() {
                    tracing::debug!(tool, %error, "peer tool failed");
                }
                error.to_tool_json()
            }
        }
    }

    async fn resources(&self) -> EnvironmentResources {
        self.engine.local_resources().await
    }

    fn tool_catalog(&self) -> Vec<ironclaw_product_contracts::peer::PeerToolSpec> {
        crate::tools::TOOLS
            .iter()
            // These act as a calling thread, which a link has none of.
            .filter(|t| !matches!(t.name, "t3_request_secret" | "t3_environment_link"))
            .map(|t| {
                let mut schema = (t.schema)();
                if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                    props.remove("environment");
                }
                ironclaw_product_contracts::peer::PeerToolSpec {
                    name: t.name.to_string(),
                    description: t.description.to_string(),
                    input_schema: schema,
                }
            })
            .collect()
    }
}

/// Balance preference parsing for hosts.
pub fn parse_preference(raw: &str) -> Option<BalancePreference> {
    BalancePreference::parse(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_keys_are_namespaced_per_caller() {
        let a = remote_request_key("caller-a", "same");
        let b = remote_request_key("caller-b", "same");
        assert_ne!(a, b);
        assert_eq!(a, remote_request_key("caller-a", "same"));
    }

    #[test]
    fn environment_names_skip_this_machine() {
        assert_eq!(
            named_environment(&json!({"environment": "vps"})),
            Some("vps".into())
        );
        assert_eq!(named_environment(&json!({"environment": "local"})), None);
        assert_eq!(named_environment(&json!({"environment": " "})), None);
        assert_eq!(named_environment(&json!({})), None);
    }

    #[test]
    fn backoff_grows_to_a_ceiling() {
        assert_eq!(Engine::backoff(0), POLL_MS);
        assert_eq!(Engine::backoff(1), POLL_MS * 2);
        assert_eq!(Engine::backoff(50), MAX_BACKOFF_MS);
    }
}
