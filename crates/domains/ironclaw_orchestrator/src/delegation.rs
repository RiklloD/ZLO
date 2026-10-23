//! Delegation: `t3_delegate`, task status/list/cancel, and Stop propagation
//! through the whole task tree (grandchildren of finished children included).

use std::collections::HashSet;
use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{CallContext, Engine, Result, clip_title, json_ok};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::profiles::{ProfileOwner, parse_profile_slug};
use crate::runtime::*;
use crate::store::TaskPatch;

/// `t3_delegate` / route input.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DelegateRequest {
    pub task: String,
    pub title: Option<String>,
    pub role: Option<String>,
    pub target: Option<Target>,
    pub mode: Option<String>,
    pub timeout_s: Option<f64>,
    pub client_request_id: Option<String>,
}

/// Provider/model (and, from stage 3, profile) for a child or fork.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Target {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub profile: Option<String>,
}

fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Engine {
    /// The live catalog for `scope`, merged across registered runtimes.
    pub(crate) async fn catalog(&self, scope: &ThreadScope) -> Vec<(RuntimeId, CatalogProvider)> {
        let mut out = Vec::new();
        for runtime in self.all_runtimes() {
            match runtime.catalog(scope).await {
                Ok(list) => out.extend(list.into_iter().map(|p| (runtime.id(), p))),
                Err(error) => tracing::debug!(%error, "orchestrator catalog read failed"),
            }
        }
        out
    }

    /// An active profile of the caller's user, for `target.profile`.
    async fn delegation_profile(&self, raw: &str, scope: &ThreadScope) -> Result<ProfileId> {
        let id = parse_profile_slug(raw)?;
        match self
            .store
            .profile(&ProfileOwner::from(scope), &id, false)
            .await?
        {
            Some(record) => Ok(record.profile_id),
            None => Err(OrchestratorError::request(
                "profile_unavailable",
                format!("Unknown profile '{raw}'. t3_capabilities lists the available profiles."),
            )),
        }
    }

    /// Resolve a target to (runtime, provider, model). An empty target
    /// inherits the caller's route on the default runtime.
    pub(crate) async fn resolve_target(
        &self,
        target: &Target,
        ctx: &CallContext,
        inherit_runtime: &RuntimeId,
    ) -> Result<(RuntimeId, Option<String>, Option<String>)> {
        let provider = trimmed(&target.provider);
        let model = trimmed(&target.model);
        if provider.is_none() && model.is_none() {
            return Ok((
                inherit_runtime.clone(),
                ctx.provider.clone(),
                ctx.model.clone(),
            ));
        }
        let catalog = self.catalog(&ctx.scope).await;
        if catalog.is_empty() {
            // The runtime advertises no catalog: pass the hint through.
            return Ok((inherit_runtime.clone(), provider, model));
        }
        if let Some(provider) = provider {
            let Some((rt, p)) = catalog.iter().find(|(_, p)| p.provider == provider) else {
                let names: Vec<&str> = catalog.iter().map(|(_, p)| p.provider.as_str()).collect();
                return Err(OrchestratorError::request(
                    "provider_unavailable",
                    format!(
                        "Unknown provider '{provider}'. Available: {}",
                        names.join(", ")
                    ),
                ));
            };
            if !p.authenticated {
                return Err(OrchestratorError::request(
                    "provider_unavailable",
                    format!("Provider '{provider}' is not authenticated."),
                ));
            }
            let model = match model {
                Some(m) if !p.models.is_empty() && !p.models.contains(&m) => {
                    let sample: Vec<&str> = p.models.iter().take(12).map(String::as_str).collect();
                    return Err(OrchestratorError::request(
                        "model_unavailable",
                        format!(
                            "Model '{m}' is not advertised by '{provider}'. Try: {}",
                            sample.join(", ")
                        ),
                    ));
                }
                Some(m) => m,
                None => p.models.first().cloned().ok_or_else(|| {
                    OrchestratorError::request(
                        "model_unavailable",
                        format!("Provider '{provider}' advertises no models."),
                    )
                })?,
            };
            return Ok((rt.clone(), Some(provider), Some(model)));
        }
        // Model only: prefer the caller's provider, then the current one.
        let model = model.unwrap_or_default();
        let mut ordered: Vec<&(RuntimeId, CatalogProvider)> = catalog.iter().collect();
        ordered.sort_by_key(|(_, p)| (Some(&p.provider) != ctx.provider.as_ref(), !p.current));
        ordered
            .into_iter()
            .find(|(_, p)| p.authenticated && p.models.contains(&model))
            .map(|(rt, p)| (rt.clone(), Some(p.provider.clone()), Some(model.clone())))
            .ok_or_else(|| {
                OrchestratorError::request(
                    "model_unavailable",
                    format!("No authenticated provider advertises model '{model}'."),
                )
            })
    }

    pub async fn capabilities(&self, ctx: &CallContext) -> Result<Value> {
        let settings = self.store.settings().await?;
        let profiles: Vec<Value> = self
            .store
            .profiles(&ProfileOwner::from(&ctx.scope))
            .await?
            .into_iter()
            .map(|p| json!({"name": p.profile_id.as_str(), "displayName": p.settings.display_name}))
            .collect();
        let providers: Vec<Value> = self
            .catalog(&ctx.scope)
            .await
            .into_iter()
            .map(|(rt, p)| {
                json!({
                    "provider": p.provider, "name": p.name, "runtime": rt.as_str(),
                    "authenticated": p.authenticated, "current": p.current,
                    "canDelegate": p.authenticated && !p.models.is_empty(),
                    "models": p.models.iter().take(40).collect::<Vec<_>>(),
                    "modelCount": p.models.len(),
                })
            })
            .collect();
        let native_fork: Vec<String> = self
            .all_runtimes()
            .iter()
            .filter(|r| r.capabilities().native_fork)
            .map(|r| r.id().to_string())
            .collect();
        Ok(json!({
            "inherited": {"provider": ctx.provider, "model": ctx.model, "threadId": ctx.thread_id.as_str()},
            "profiles": profiles,
            "providers": providers,
            "features": {
                "delegation": true, "asyncWait": true, "cancel": true, "fork": true,
                "nativeForkRuntimes": native_fork, "mergeBack": true, "durableQueue": true,
                "steer": true, "restart": true,
                "continueAfterRestart": settings.continue_after_restart,
                "resumeOnUsageLimit": settings.resume_on_usage_limit,
                "schedules": "IronClaw automations (routines)",
            },
            "limits": {"maxDepth": settings.max_depth, "maxRunningPerParent": settings.max_running_per_parent},
        }))
    }

    fn brief(&self, task: &TaskRecord, parent_title: &str, max_depth: u32) -> String {
        let delegate_more = if task.depth < max_depth {
            "You may delegate further with t3_delegate if it genuinely helps.\n"
        } else {
            ""
        };
        format!(
            "You are a delegated child agent (T3 orchestrator task {id}, role: {role}) working for a parent agent \
             in the thread \"{parent}\". You only know what is in this brief: the parent's conversation is not \
             available to you. Work autonomously. No one will answer questions during the task, so make reasonable \
             assumptions and state them.\n{hint}\nYour final message is returned to the parent verbatim, so end \
             with a self-contained report: what you did, the results or findings, files changed, commands/tests \
             run with outcomes, and open issues.\n{delegate_more}\nTask:\n{prompt}",
            id = task.id,
            role = task.role,
            parent = if parent_title.is_empty() {
                "untitled"
            } else {
                parent_title
            },
            hint = task.role.hint(),
            prompt = task.prompt,
        )
    }

    pub async fn delegate(&self, ctx: &CallContext, req: DelegateRequest) -> Result<Value> {
        let text = req.task.trim().to_string();
        if text.is_empty() {
            return Err(OrchestratorError::invalid("task is required"));
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
        let request_id = trimmed(&req.client_request_id);
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
        let target = req.target.clone().unwrap_or_default();
        let (runtime_id, provider, model) = self
            .resolve_target(&target, ctx, &parent.runtime_id)
            .await?;
        let profile_id = match trimmed(&target.profile) {
            Some(raw) => Some(self.delegation_profile(&raw, &parent.scope).await?),
            None => parent.profile_id.clone(),
        };
        let runtime = self.runtime(&runtime_id)?;
        let title = clip_title(
            req.title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(&text),
        );
        let mut task = TaskRecord {
            id: TaskId::generate(),
            parent_thread: parent.thread_id.clone(),
            child_thread: None,
            root_thread: parent_task
                .as_ref()
                .map_or_else(|| parent.thread_id.clone(), |t| t.root_thread.clone()),
            depth,
            profile_id,
            parent_profile_id: parent.profile_id.clone(),
            title: title.clone(),
            role,
            runtime_id: runtime_id.clone(),
            provider: provider.clone(),
            model: model.clone(),
            prompt: text,
            status: TaskStatus::Starting,
            result: None,
            error: None,
            progress: None,
            usage: None,
            created_at: now_ms(),
            started_at: None,
            finished_at: None,
            client_request_id: request_id,
            delivery: Delivery::None,
            delivered_at: None,
            cancel_requested: false,
            limit_reset_at: None,
        };
        self.store.insert_task(&task).await?;
        self.store.set_hold(&parent.thread_id, None).await?;
        if let Err(error) = self
            .start_child(&mut task, &parent, &runtime, &settings)
            .await
        {
            let message = error.to_string();
            self.store
                .move_task(
                    &task.id,
                    &[],
                    false,
                    TaskStatus::Failed,
                    TaskPatch {
                        error: Some(Some(clip(
                            &format!("could not start child: {message}"),
                            2000,
                        ))),
                        finished_at: Some(now_ms()),
                        delivery: Some(Delivery::Skipped),
                        ..TaskPatch::default()
                    },
                )
                .await?;
            return Err(OrchestratorError::request(
                "orchestration_error",
                format!("Could not start the child agent: {message}"),
            ));
        }
        let current = self.require_task(&task.id).await?;
        self.maybe_wait(current, wait, timeout).await
    }

    async fn start_child(
        &self,
        task: &mut TaskRecord,
        parent: &ThreadRecord,
        runtime: &std::sync::Arc<dyn TurnRuntime>,
        settings: &Settings,
    ) -> Result<()> {
        let created = runtime
            .create_thread(CreateThread {
                scope: parent.scope.clone(),
                title: Some(format!("↳ {}", task.title)),
                provider: task.provider.clone(),
                model: task.model.clone(),
                idempotency_key: format!("t3o:{}", task.id),
            })
            .await?;
        let mut child = ThreadRecord::new(
            created.thread_id.clone(),
            parent.scope.clone(),
            runtime.id(),
        );
        child.profile_id = task.profile_id.clone();
        child.provider = created.provider.clone().or_else(|| task.provider.clone());
        child.model = created.model.clone().or_else(|| task.model.clone());
        child.title = Some(format!("↳ {}", task.title));
        child.native_session_id = created.native_session_id.clone();
        self.store.ensure_thread(&child).await?;
        self.store
            .set_task_child(
                &task.id,
                &child.thread_id,
                child.provider.as_deref(),
                child.model.as_deref(),
            )
            .await?;
        task.child_thread = Some(child.thread_id.clone());
        self.store
            .insert_transfer(&TransferRecord {
                id: TransferId::generate(),
                kind: TransferKind::SubagentSpawn,
                source_thread: parent.thread_id.clone(),
                target_thread: Some(child.thread_id.clone()),
                source_point: None,
                base_point: None,
                strategy: TransferStrategy::PromptOnly,
                status: TransferStatus::Consumed,
                delivery: TransferDelivery::Now,
                handoff: None,
                created_by: Author::Agent,
                created_at: now_ms(),
                consumed_at: Some(now_ms()),
            })
            .await?;
        let brief = self.brief(
            task,
            parent.title.as_deref().unwrap_or(""),
            settings.max_depth,
        );
        {
            // Stop / cancel may have landed while the child was being created.
            let current = self.require_task(&task.id).await?;
            if current.cancel_requested || current.status.is_terminal() {
                let reason = current
                    .error
                    .clone()
                    .unwrap_or_else(|| "cancelled before start".into());
                self.finalize(&current, TaskStatus::Cancelled, None, Some(reason))
                    .await?;
                return Ok(());
            }
        }
        // The child is a fresh thread: no hold applies to it yet.
        if !self
            .deliver(&child.thread_id, brief, TurnOrigin::DelegatedBrief, false)
            .await?
        {
            return Err(OrchestratorError::request(
                "runtime_rejected",
                "the child thread did not accept its brief",
            ));
        }
        self.store
            .move_task(
                &task.id,
                &[TaskStatus::Starting],
                false,
                TaskStatus::Running,
                TaskPatch {
                    started_at: Some(now_ms()),
                    ..TaskPatch::default()
                },
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn require_task(&self, id: &TaskId) -> Result<TaskRecord> {
        self.store
            .task(id)
            .await?
            .ok_or_else(|| OrchestratorError::not_found("task_not_found", format!("No task {id}.")))
    }

    async fn maybe_wait(&self, task: TaskRecord, wait: bool, timeout_s: f64) -> Result<Value> {
        if !wait {
            return self.task_view(&task, None).await;
        }
        if task.status.is_terminal() {
            self.ack(&task.id).await?;
            let current = self.require_task(&task.id).await?;
            return self.task_view(&current, Some(false)).await;
        }
        let notify = self.register_waiter(&task.id)?;
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Re-check after registering: the child may have finished in between.
        let done = if self.require_task(&task.id).await?.status.is_terminal() {
            true
        } else {
            tokio::time::timeout(Duration::from_secs_f64(timeout_s), notified)
                .await
                .is_ok()
        };
        self.drop_waiter(&task.id);
        let mut current = self.require_task(&task.id).await?;
        if current.status.is_terminal() {
            self.ack(&task.id).await?;
            current = self.require_task(&task.id).await?;
        }
        self.task_view(&current, Some(!done)).await
    }

    /// Reading a terminal result acknowledges its automatic delivery.
    pub(crate) async fn ack(&self, id: &TaskId) -> Result<()> {
        self.store
            .set_delivery(id, Delivery::Pending, Delivery::Read, now_ms())
            .await?;
        Ok(())
    }

    pub async fn task_view(&self, t: &TaskRecord, wait_timed_out: Option<bool>) -> Result<Value> {
        let end = t.finished_at.unwrap_or_else(now_ms);
        let duration = t.started_at.map_or(0.0, |s| (end - s) as f64 / 1000.0);
        let pending_children = match &t.child_thread {
            Some(child) => self.store.tasks_for_parent(child, true).await?.len(),
            None => 0,
        };
        let work_state = if t.status.is_terminal() {
            "result_available"
        } else if t.status == TaskStatus::Waiting {
            "waiting_for_limit_reset"
        } else {
            "working"
        };
        let mut view = json!({
            "taskId": t.id.as_str(), "title": t.title, "role": t.role.as_str(), "status": t.status.as_str(),
            "workState": work_state, "runtime": t.runtime_id.as_str(), "provider": t.provider, "model": t.model,
            "parentThreadId": t.parent_thread.as_str(),
            "childThreadId": t.child_thread.as_ref().map(|c| c.as_str()),
            "profile": t.profile_id.as_ref().map(|p| p.as_str()),
            "depth": t.depth, "createdAt": t.created_at, "startedAt": t.started_at, "finishedAt": t.finished_at,
            "durationSeconds": (duration * 10.0).round() / 10.0,
            "progress": t.progress, "usage": t.usage,
            "summary": if t.status.is_terminal() { t.result.clone() } else { None },
            "error": t.error, "delivery": t.delivery.as_str(), "limitResetAt": t.limit_reset_at,
            "cancelRequested": t.cancel_requested, "pendingChildren": pending_children,
        });
        if let Some(timed_out) = wait_timed_out {
            view["waitTimedOut"] = json!(timed_out);
        }
        Ok(view)
    }

    /// Task visible from `thread`: its own child, or one in its subtree.
    async fn visible_task(&self, id: &TaskId, thread: &ThreadId) -> Result<TaskRecord> {
        match self.store.task(id).await? {
            Some(t) if &t.parent_thread == thread || &t.root_thread == thread => Ok(t),
            _ => Err(OrchestratorError::not_found(
                "task_not_found",
                format!("No task {id} for this thread."),
            )),
        }
    }

    pub async fn task_status(&self, ctx: &CallContext, id: &TaskId) -> Result<Value> {
        let task = self.visible_task(id, &ctx.thread_id).await?;
        if task.status.is_terminal() {
            self.ack(id).await?;
        }
        let current = self.require_task(id).await?;
        self.task_view(&current, None).await
    }

    pub async fn task_list(&self, thread: &ThreadId) -> Result<Value> {
        let mut views = Vec::new();
        for t in self.store.tasks_for_parent(thread, false).await? {
            views.push(self.task_view(&t, None).await?);
        }
        Ok(json!({"threadId": thread.as_str(), "tasks": views}))
    }

    pub async fn cancel_task(&self, ctx: &CallContext, id: &TaskId, reason: &str) -> Result<Value> {
        let task = self.visible_task(id, &ctx.thread_id).await?;
        {
            let _guard = self.dispatch.lock().await;
            self.mark_cancel(&task, reason).await?;
            if let Some(child) = &task.child_thread {
                self.mark_cancel_tree(child, reason).await?;
            }
        }
        self.enforce_cancel(&self.require_task(id).await?).await?;
        if let Some(child) = &task.child_thread {
            self.stop_descendants(child).await?;
        }
        let current = self.require_task(id).await?;
        self.task_view(&current, None).await.map(json_ok)
    }

    async fn mark_cancel(&self, task: &TaskRecord, reason: &str) -> Result<()> {
        self.store.mark_task_cancel(&task.id, reason).await?;
        if let Some(child) = &task.child_thread {
            self.store
                .set_hold(child, Some(HoldReason::Cancelled))
                .await?;
            self.store.cancel_armed_recoveries(child).await?;
        }
        Ok(())
    }

    /// Durable cancel intent across the whole tree under `thread`.
    pub(crate) async fn mark_cancel_tree(&self, thread: &ThreadId, reason: &str) -> Result<()> {
        let mut stack = vec![thread.clone()];
        let mut seen = HashSet::new();
        while let Some(key) = stack.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            for task in self.store.tasks_for_parent(&key, false).await? {
                if !task.status.is_terminal() {
                    self.mark_cancel(&task, reason).await?;
                }
                if let Some(child) = task.child_thread {
                    stack.push(child);
                }
            }
        }
        Ok(())
    }

    /// Enforce cancel intent on every live task beneath `thread`.
    pub(crate) async fn stop_descendants(&self, thread: &ThreadId) -> Result<usize> {
        let mut stack = vec![thread.clone()];
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        while let Some(key) = stack.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            for task in self.store.tasks_for_parent(&key, false).await? {
                if !task.status.is_terminal() && task.cancel_requested {
                    targets.push(task.clone());
                }
                if let Some(child) = task.child_thread {
                    stack.push(child);
                }
            }
        }
        for task in &targets {
            self.enforce_cancel(task).await?;
        }
        Ok(targets.len())
    }

    /// Interrupt a cancel-requested task's running child, or finalize it now
    /// when the child is idle. A running child finalizes on its terminal
    /// event; the tick finalizes it after a grace period otherwise.
    async fn enforce_cancel(&self, task: &TaskRecord) -> Result<()> {
        if task.status.is_terminal() || !task.cancel_requested {
            return Ok(());
        }
        let child = match &task.child_thread {
            Some(id) => self.store.thread(id).await?,
            None => None,
        };
        match child {
            Some(child) if child.is_running() => {
                let reason = task.error.clone().unwrap_or_else(|| "cancelled".into());
                if let Err(error) = self
                    .runtime(&child.runtime_id)?
                    .cancel(&child, &reason)
                    .await
                {
                    tracing::debug!(%error, "orchestrator child cancel failed; tick will finalize");
                }
            }
            _ => {
                let reason = task.error.clone().unwrap_or_else(|| "cancelled".into());
                self.finalize(task, TaskStatus::Cancelled, None, Some(reason))
                    .await?;
            }
        }
        Ok(())
    }
}
