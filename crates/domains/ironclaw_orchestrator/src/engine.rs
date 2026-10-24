//! The process-wide orchestration engine. It owns every orchestrated thread:
//! turn admission for automatic turns, the delegation tree, the durable
//! queue, forks and merge-back, Stop propagation, restart continuation and
//! usage-limit resume. Side effects run on a 2-second tick plus the turn
//! events runtimes report through [`TurnEventSink`].
//!
//! Admission rule for automatic turns (queue drains, result deliveries,
//! handoffs, continuations): the thread must be idle and not held, checked
//! under the dispatch lock that Stop also takes, so an automatic turn can
//! never slip in between a user's Stop and its hold.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};

use crate::error::OrchestratorError;
use crate::handoff;
use crate::model::*;
use crate::restart::{self, SkipReason};
use crate::runtime::*;
use crate::store::{Store, TaskPatch};

pub(crate) type Result<T> = std::result::Result<T, OrchestratorError>;

/// Grace before an unanswered cancel is finalized without its terminal event.
const CANCEL_GRACE_MS: i64 = 20_000;
const HISTORY_LIMIT: usize = 2_000;

/// Who is calling an engine operation: the thread the call comes from and
/// the authenticated scope it acts as.
#[derive(Debug, Clone, PartialEq)]
pub struct CallContext {
    pub thread_id: ThreadId,
    pub scope: ThreadScope,
    /// The caller thread's current route, inherited by delegated children.
    pub provider: Option<String>,
    pub model: Option<String>,
}

pub struct Engine {
    pub(crate) store: Store,
    runtimes: RwLock<HashMap<RuntimeId, Arc<dyn TurnRuntime>>>,
    pub(crate) default_runtime: RuntimeId,
    /// Serializes "check hold + idle → start turn" against Stop / cancel.
    pub(crate) dispatch: Mutex<()>,
    waiters: StdMutex<HashMap<TaskId, Arc<Notify>>>,
    enforced: StdMutex<HashMap<TaskId, Millis>>,
    pub(crate) instance_id: String,
    pub(crate) started_at: Millis,
    started: AtomicBool,
    booted: AtomicBool,
    pub(crate) shutting_down: AtomicBool,
    /// Schedule grammar for `t3_schedule`, plugged in by the host.
    pub(crate) schedule_planner: OnceLock<Arc<dyn crate::schedules::SchedulePlanner>>,
    /// Loopback base of the webhook route, known once the gateway is bound.
    pub(crate) webhook_base: OnceLock<String>,
}

impl Engine {
    /// Open the engine over `store`, creating tables and importing legacy
    /// threads on first run.
    /// Open the engine over the deployment's shared libSQL runtime.
    pub async fn open_libsql(
        db: Arc<ironclaw_libsql_runtime::LibSqlRuntime>,
        default_runtime: &str,
    ) -> Result<Arc<Self>> {
        Self::open(Store::new(db), RuntimeId::new(default_runtime)?).await
    }

    pub async fn open(store: Store, default_runtime: RuntimeId) -> Result<Arc<Self>> {
        let imported = store.migrate(&default_runtime).await?;
        if imported > 0 {
            tracing::debug!(imported, "orchestrator imported legacy threads");
        }
        let started_at = now_ms();
        Ok(Arc::new(Self {
            store,
            runtimes: RwLock::new(HashMap::new()),
            default_runtime,
            dispatch: Mutex::new(()),
            waiters: StdMutex::new(HashMap::new()),
            enforced: StdMutex::new(HashMap::new()),
            instance_id: format!("{}-{started_at}", std::process::id()),
            started_at,
            started: AtomicBool::new(false),
            booted: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            schedule_planner: OnceLock::new(),
            webhook_base: OnceLock::new(),
        }))
    }

    /// Enable `t3_schedule` with the host's schedule grammar (first call wins).
    pub fn set_schedule_planner(&self, planner: Arc<dyn crate::schedules::SchedulePlanner>) {
        let _ = self.schedule_planner.set(planner); // silent-ok: already set by an earlier wiring
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Engine identity, runtimes and settings (for status routes).
    pub async fn status(&self) -> Result<Value> {
        let runtimes: Vec<Value> = self
            .all_runtimes()
            .iter()
            .map(|r| json!({"id": r.id().as_str(), "capabilities": r.capabilities()}))
            .collect();
        Ok(json!({
            "instanceId": self.instance_id, "startedAt": self.started_at,
            "defaultRuntime": self.default_runtime.as_str(), "runtimes": runtimes,
            "settings": self.store.settings().await?,
        }))
    }

    /// Register (or replace) a runtime. The engine looks runtimes up by the
    /// `runtime_id` on each thread row.
    pub fn register_runtime(&self, runtime: Arc<dyn TurnRuntime>) {
        if let Ok(mut map) = self.runtimes.write() {
            map.insert(runtime.id(), runtime);
        }
    }

    pub(crate) fn runtime(&self, id: &RuntimeId) -> Result<Arc<dyn TurnRuntime>> {
        self.runtimes
            .read()
            .ok()
            .and_then(|map| map.get(id).cloned())
            .ok_or_else(|| {
                OrchestratorError::request(
                    "runtime_unavailable",
                    format!("runtime {id} is not running in this backend"),
                )
            })
    }

    pub(crate) fn all_runtimes(&self) -> Vec<Arc<dyn TurnRuntime>> {
        self.runtimes
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Start the tick loop once. Holds only a weak reference, so dropping
    /// the last engine handle ends the loop.
    pub fn start(self: &Arc<Self>, period: Duration) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("orchestrator not started: no tokio runtime");
            return;
        };
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        handle.spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(engine) = weak.upgrade() else { break };
                if engine.shutting_down.load(Ordering::SeqCst) {
                    break;
                }
                engine.tick().await;
            }
        });
    }

    /// One pass of periodic work. Each step is independent: one failing step
    /// does not starve the others.
    pub async fn tick(&self) {
        if !self.booted.swap(true, Ordering::SeqCst)
            && let Err(error) = self.boot().await
        {
            tracing::warn!(%error, "orchestrator boot recovery failed");
        }
        log_step("cancels", self.process_cancels().await);
        log_step("schedules", self.fire_schedules().await);
        log_step("restarts", self.fire_restarts().await);
        log_step("queues", self.drain_queues().await);
        log_step("results", self.deliver_results().await);
        log_step("transfers", self.deliver_transfers_now().await);
        log_step("usage limits", self.fire_usage_limits().await);
    }

    /// Clean quit: hold every thread this instance is running as
    /// `Interrupted` (its queue waits, nothing auto-continues on reopen),
    /// then stop the tick loop. Call before the process interrupts its
    /// turns. A crash skips this, so its cut turns still auto-continue.
    pub async fn prepare_for_shutdown(&self) -> Result<usize> {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return Ok(0);
        }
        let mine: Vec<_> = self
            .store
            .live_work()
            .await?
            .into_iter()
            .filter(|w| w.instance_id == self.instance_id)
            .collect();
        for work in &mine {
            if self.store.hold(&work.thread_id).await?.is_none() {
                self.store
                    .set_hold(&work.thread_id, Some(HoldReason::Interrupted))
                    .await?;
            }
        }
        Ok(mine.len())
    }

    // ── threads ─────────────────────────────────────────────────────────

    /// Register a thread the engine has not seen (no-op when known).
    pub async fn ensure_thread(
        &self,
        thread_id: &ThreadId,
        scope: &ThreadScope,
        runtime: Option<&RuntimeId>,
    ) -> Result<ThreadRecord> {
        if let Some(existing) = self.store.thread(thread_id).await? {
            return Ok(existing);
        }
        let record = ThreadRecord::new(
            thread_id.clone(),
            scope.clone(),
            runtime
                .cloned()
                .unwrap_or_else(|| self.default_runtime.clone()),
        );
        self.store.ensure_thread(&record).await?;
        self.store
            .thread(thread_id)
            .await?
            .ok_or_else(|| OrchestratorError::Store("thread row vanished after insert".into()))
    }

    /// The thread, if it exists and belongs to `scope`'s tenant and user.
    pub async fn owned_thread(
        &self,
        thread_id: &ThreadId,
        scope: &ThreadScope,
    ) -> Result<ThreadRecord> {
        match self.store.thread(thread_id).await? {
            Some(t) if t.scope.tenant_id == scope.tenant_id && t.scope.user_id == scope.user_id => {
                Ok(t)
            }
            _ => Err(OrchestratorError::not_found(
                "thread_not_found",
                format!("No thread {thread_id} for this user."),
            )),
        }
    }

    pub(crate) async fn history(&self, thread: &ThreadRecord) -> Result<Vec<HistoryMessage>> {
        Ok(self
            .runtime(&thread.runtime_id)?
            .history(thread, HISTORY_LIMIT)
            .await?)
    }

    /// A user typed into `thread` (through the product surface): lift the
    /// Stop hold and stamp user activity, which supersedes pending
    /// automatic resumes.
    pub async fn on_user_prompt(&self, thread_id: &ThreadId) -> Result<()> {
        self.store.set_hold(thread_id, None).await?;
        self.store.set_last_user(thread_id, now_ms()).await
    }

    /// Pending next-message context for `thread` (consumed once). Runtimes
    /// without hidden context injection prepend it to the user's message.
    pub async fn take_next_message_context(&self, thread_id: &ThreadId) -> Result<Option<String>> {
        let mut texts = Vec::new();
        for x in self.store.pending_next_message(thread_id).await? {
            if self.store.consume_transfer(&x.id).await?
                && let Some(text) = x.handoff.filter(|t| !t.is_empty())
            {
                texts.push(text);
            }
        }
        Ok((!texts.is_empty()).then(|| texts.join("\n\n")))
    }

    /// The user pressed Stop on `thread`: hold its queue and automatic turns
    /// until the next user message, drop armed resumes, and stop every
    /// delegated task beneath it, finished intermediate children included.
    pub async fn on_user_stop(&self, thread_id: &ThreadId) -> Result<usize> {
        {
            let _guard = self.dispatch.lock().await;
            self.store
                .set_hold(thread_id, Some(HoldReason::Stopped))
                .await?;
            self.store.cancel_armed_recoveries(thread_id).await?;
            self.mark_cancel_tree(thread_id, "parent stopped").await?;
        }
        self.stop_descendants(thread_id).await
    }

    // ── automatic turn admission ────────────────────────────────────────

    /// Start an automatic turn on an idle, unheld thread. `false` means not
    /// delivered now (busy or held); the caller retries on a later tick.
    pub(crate) async fn deliver(
        &self,
        thread_id: &ThreadId,
        text: String,
        origin: TurnOrigin,
        respect_hold: bool,
    ) -> Result<bool> {
        let _guard = self.dispatch.lock().await;
        if respect_hold && self.store.hold(thread_id).await?.is_some() {
            return Ok(false);
        }
        let Some(thread) = self.store.thread(thread_id).await? else {
            return Ok(false);
        };
        if thread.is_running() {
            return Ok(false);
        }
        let runtime = self.runtime(&thread.runtime_id)?;
        let goal = self
            .store
            .goal(thread_id)
            .await?
            .filter(|g| g.status == ironclaw_product_contracts::inbox::GoalStatus::Active)
            .map(|g| g.objective);
        let admission = runtime
            .start_turn(
                &thread,
                TurnInput {
                    text,
                    origin,
                    model: thread.model.clone(),
                    goal,
                    effort: self.store.thread_effort(thread_id).await?,
                },
            )
            .await?;
        match admission {
            TurnAdmission::Started { run_id } => {
                self.mark_running(&thread, run_id.as_deref(), now_ms())
                    .await?;
                Ok(true)
            }
            TurnAdmission::Steered { .. } => Ok(true),
            TurnAdmission::Busy => Ok(false),
        }
    }

    async fn mark_running(
        &self,
        thread: &ThreadRecord,
        run_id: Option<&str>,
        at: Millis,
    ) -> Result<()> {
        self.store
            .set_turn_running(&thread.thread_id, run_id, at)
            .await?;
        self.store
            .put_live_work(
                &thread.thread_id,
                &self.instance_id,
                &json!({"run_id": run_id, "runtime": thread.runtime_id.as_str(), "turn_started_at": at}),
            )
            .await
    }

    // ── tick steps ──────────────────────────────────────────────────────

    async fn boot(&self) -> Result<()> {
        self.store.requeue_sending().await?;
        // Live work another engine instance left behind was cut by its exit.
        for work in self.store.live_work().await? {
            if work.instance_id == self.instance_id {
                continue;
            }
            let self_reporting = work.detail["runtime"]
                .as_str()
                .and_then(|id| RuntimeId::new(id).ok())
                .and_then(|id| self.runtime(&id).ok())
                .is_some_and(|rt| rt.capabilities().reports_restart_cuts);
            self.store.delete_live_work(&work.thread_id).await?;
            if self_reporting {
                // Its own terminal event (`cut_by_restart`) arms the continuation.
                continue;
            }
            let mut detail = work.detail.clone();
            if detail.get("cause").is_none() {
                detail["cause"] = json!("crash");
            }
            self.record_restart_intent(&work.thread_id, detail, work.updated_at)
                .await?;
            // The thread is not running any more, whatever its row says.
            self.store
                .set_turn_ended(&work.thread_id, None, TurnState::Idle, None, now_ms())
                .await?;
        }
        Ok(())
    }

    async fn drain_queues(&self) -> Result<()> {
        for thread_id in self.store.queued_threads().await? {
            if self.store.hold(&thread_id).await?.is_some() {
                continue;
            }
            match self.store.thread(&thread_id).await? {
                Some(t) if !t.is_running() => {}
                _ => continue,
            }
            let Some(item) = self.store.claim_queue_head(&thread_id).await? else {
                continue;
            };
            let delivered = self
                .deliver(&thread_id, item.text.clone(), TurnOrigin::QueueDrain, true)
                .await;
            let next = if matches!(delivered, Ok(true)) {
                self.store.set_last_user(&thread_id, now_ms()).await?;
                QueueStatus::Sent
            } else {
                QueueStatus::Pending
            };
            self.store
                .set_queue_status(&item.id, QueueStatus::Sending, next)
                .await?;
            delivered?;
        }
        Ok(())
    }

    async fn deliver_results(&self) -> Result<()> {
        let mut by_parent: Vec<(ThreadId, Vec<TaskRecord>)> = Vec::new();
        for task in self.store.pending_deliveries().await? {
            match by_parent.iter_mut().find(|(p, _)| *p == task.parent_thread) {
                Some((_, list)) => list.push(task),
                None => by_parent.push((task.parent_thread.clone(), vec![task])),
            }
        }
        for (parent, tasks) in by_parent {
            // Stop holds wakes; the user's queued messages go first.
            if self.store.hold(&parent).await?.is_some()
                || !self.store.queue(&parent, false).await?.is_empty()
            {
                continue;
            }
            let text = result_turn_text(&tasks);
            if self
                .deliver(&parent, text, TurnOrigin::ResultDelivery, true)
                .await?
            {
                let now = now_ms();
                for task in &tasks {
                    self.store
                        .set_delivery(&task.id, Delivery::Pending, Delivery::Delivered, now)
                        .await?;
                    if let Some(child) = &task.child_thread {
                        self.store.consume_result_transfers(child).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn deliver_transfers_now(&self) -> Result<()> {
        for x in self.store.pending_transfers_now().await? {
            let Some(target) = &x.target_thread else {
                continue;
            };
            let text = format!(
                "{}\n\nAcknowledge this context in one or two sentences; do not start new work.",
                x.handoff.clone().unwrap_or_default()
            );
            if self
                .deliver(target, text, TurnOrigin::ContextHandoff, true)
                .await?
            {
                self.store.consume_transfer(&x.id).await?;
            }
        }
        Ok(())
    }

    /// Enforce cancel intent whose terminal event never arrived.
    async fn process_cancels(&self) -> Result<()> {
        let now = now_ms();
        for task in self.store.active_tasks().await? {
            if !task.cancel_requested {
                continue;
            }
            let first = {
                let mut map = self.enforced.lock().map_err(lock_poisoned)?;
                *map.entry(task.id.clone()).or_insert(now)
            };
            let child_running = match &task.child_thread {
                Some(child) => self
                    .store
                    .thread(child)
                    .await?
                    .is_some_and(|t| t.is_running()),
                None => false,
            };
            if !child_running || now - first > CANCEL_GRACE_MS {
                let reason = task.error.clone().unwrap_or_else(|| "cancelled".into());
                self.finalize(&task, TaskStatus::Cancelled, None, Some(reason))
                    .await?;
            }
        }
        Ok(())
    }

    // ── task finalization ───────────────────────────────────────────────

    /// Move a task to its terminal state once (the first writer wins) and
    /// queue its result for the parent.
    pub(crate) async fn finalize(
        &self,
        task: &TaskRecord,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) -> Result<bool> {
        let waited = self
            .waiters
            .lock()
            .map_err(lock_poisoned)?
            .contains_key(&task.id);
        // A waited task is answered inline; its card would be a duplicate.
        let delivery = if waited {
            Delivery::Read
        } else if matches!(task.delivery, Delivery::None | Delivery::Pending) {
            Delivery::Pending
        } else {
            task.delivery
        };
        let patch = TaskPatch {
            result: Some(result.map(|r| clip(&r, 60_000))),
            error: Some(error.map(|e| clip(&e, 4_000))),
            finished_at: Some(now_ms()),
            delivery: Some(delivery),
            ..TaskPatch::default()
        };
        if !self
            .store
            .move_task(&task.id, &[], false, status, patch)
            .await?
        {
            return Ok(false);
        }
        self.store
            .insert_transfer(&TransferRecord {
                id: TransferId::generate(),
                kind: TransferKind::SubagentResult,
                source_thread: task
                    .child_thread
                    .clone()
                    .unwrap_or_else(|| task.parent_thread.clone()),
                target_thread: Some(task.parent_thread.clone()),
                source_point: None,
                base_point: None,
                strategy: TransferStrategy::ResultCard,
                status: if delivery == Delivery::Pending {
                    TransferStatus::Pending
                } else {
                    TransferStatus::Consumed
                },
                delivery: TransferDelivery::Now,
                handoff: None,
                created_by: Author::System,
                created_at: now_ms(),
                consumed_at: None,
            })
            .await?;
        if let Ok(mut map) = self.enforced.lock() {
            map.remove(&task.id);
        }
        if let Some(notify) = self.waiters.lock().map_err(lock_poisoned)?.get(&task.id) {
            notify.notify_waiters();
        }
        Ok(true)
    }

    pub(crate) fn register_waiter(&self, id: &TaskId) -> Result<Arc<Notify>> {
        let mut map = self.waiters.lock().map_err(lock_poisoned)?;
        Ok(Arc::clone(
            map.entry(id.clone())
                .or_insert_with(|| Arc::new(Notify::new())),
        ))
    }

    pub(crate) fn drop_waiter(&self, id: &TaskId) {
        if let Ok(mut map) = self.waiters.lock() {
            map.remove(id);
        }
    }

    async fn final_text(&self, thread: &ThreadRecord, outcome: &TurnOutcome) -> String {
        if let Some(text) = outcome.final_text.as_ref().filter(|t| !t.trim().is_empty()) {
            return text.clone();
        }
        match self.history(thread).await {
            Ok(messages) => handoff::last_reply(&messages),
            Err(error) => {
                tracing::debug!(%error, "orchestrator could not read child history");
                String::new()
            }
        }
    }

    /// A child thread's turn ended: settle its task (T3 semantics).
    async fn settle_child_task(
        &self,
        thread: &ThreadRecord,
        outcome: &TurnOutcome,
        usage_armed: bool,
        restart_pending: bool,
    ) -> Result<()> {
        let Some(task) = self.store.task_for_child(&thread.thread_id).await? else {
            return Ok(());
        };
        if task.status.is_terminal() {
            return Ok(());
        }
        let mut usage = task.usage.unwrap_or_default();
        if let Some(u) = &outcome.usage {
            usage.add(u);
        }
        let usage_patch = outcome.usage.map(|_| usage);
        if task.cancel_requested {
            let text = self.final_text(thread, outcome).await;
            self.finalize(
                &task,
                TaskStatus::Cancelled,
                non_empty(text),
                task.error.clone(),
            )
            .await?;
            return Ok(());
        }
        if usage_armed {
            let moved = self
                .store
                .move_task(
                    &task.id,
                    &[],
                    true,
                    TaskStatus::Waiting,
                    TaskPatch {
                        error: Some(outcome.error.clone()),
                        limit_reset_at: Some(outcome.usage_limit_resets_at),
                        usage: usage_patch,
                        ..TaskPatch::default()
                    },
                )
                .await?;
            if moved {
                return Ok(());
            }
        }
        // Restart continuation, or the user's Continue after a quit, will run
        // the child again; the task stays live.
        if restart_pending
            || self.store.hold(&thread.thread_id).await? == Some(HoldReason::Interrupted)
        {
            return Ok(());
        }
        match outcome.status {
            TurnEndStatus::Completed => {
                let text = self.final_text(thread, outcome).await;
                // A finished turn with live nested work is not a finished task.
                if !self
                    .store
                    .tasks_for_parent(&thread.thread_id, true)
                    .await?
                    .is_empty()
                {
                    self.store
                        .move_task(
                            &task.id,
                            &[],
                            true,
                            TaskStatus::WaitingChildren,
                            TaskPatch {
                                result: Some(Some(clip(&text, 60_000))),
                                usage: usage_patch,
                                ..TaskPatch::default()
                            },
                        )
                        .await?;
                    return Ok(());
                }
                if let Some(u) = usage_patch {
                    self.store
                        .move_task(
                            &task.id,
                            &[],
                            true,
                            task.status,
                            TaskPatch {
                                usage: Some(u),
                                ..TaskPatch::default()
                            },
                        )
                        .await?;
                }
                self.finalize(&task, TaskStatus::Completed, Some(text), None)
                    .await?;
            }
            TurnEndStatus::Cancelled | TurnEndStatus::Interrupted => {
                let text = self.final_text(thread, outcome).await;
                self.finalize(
                    &task,
                    TaskStatus::Interrupted,
                    non_empty(text),
                    outcome.error.clone(),
                )
                .await?;
            }
            TurnEndStatus::Failed => {
                let error = outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "the child turn failed".into());
                self.finalize(
                    &task,
                    TaskStatus::Failed,
                    outcome.final_text.clone(),
                    Some(error),
                )
                .await?;
            }
        }
        Ok(())
    }
}

impl Engine {
    /// The thread's latest per-turn telemetry, if any runtime reported one.
    pub async fn thread_telemetry(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<crate::telemetry::ThreadTelemetryRecord>> {
        self.store.telemetry(thread).await
    }
}

#[async_trait]
impl TurnEventSink for Engine {
    async fn turn_telemetry(
        &self,
        thread_id: &ThreadId,
        telemetry: crate::telemetry::TurnTelemetry,
        at: Millis,
    ) {
        let result = self.store.put_telemetry(thread_id, &telemetry, at).await;
        log_step("turn telemetry", result);
    }

    async fn turn_started(&self, thread_id: &ThreadId, run_id: Option<String>, at: Millis) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let result: Result<()> = async {
            let Some(thread) = self.store.thread(thread_id).await? else {
                return Ok(());
            };
            self.mark_running(&thread, run_id.as_deref(), at).await?;
            if let Some(task) = self.store.task_for_child(thread_id).await? {
                self.store
                    .move_task(
                        &task.id,
                        &[
                            TaskStatus::Starting,
                            TaskStatus::Waiting,
                            TaskStatus::WaitingChildren,
                        ],
                        true,
                        TaskStatus::Running,
                        TaskPatch {
                            started_at: Some(at),
                            ..TaskPatch::default()
                        },
                    )
                    .await?;
            }
            Ok(())
        }
        .await;
        log_step("turn started", result);
    }

    async fn turn_finished(
        &self,
        thread_id: &ThreadId,
        run_id: Option<String>,
        outcome: TurnOutcome,
        at: Millis,
    ) {
        // Once shutdown starts, terminal frames are the restart, not outcomes.
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let result: Result<()> = async {
            let Some(thread) = self.store.thread(thread_id).await? else {
                return Ok(());
            };
            // Log first: `set_turn_running` refuses runs already logged, so a
            // late admission bookkeeping write cannot resurrect this turn.
            self.store
                .insert_turn_log(&TurnLogRecord {
                    id: TurnLogId::generate(),
                    thread_id: thread_id.clone(),
                    run_id: run_id.clone(),
                    status: outcome.status,
                    started_at: thread.turn_started_at,
                    ended_at: at,
                    usage: outcome.usage,
                    error: outcome.error.clone(),
                })
                .await?;
            let state = if outcome.status == TurnEndStatus::Failed {
                TurnState::Error
            } else {
                TurnState::Idle
            };
            self.store
                .set_turn_ended(thread_id, run_id.as_deref(), state, outcome.error.as_deref(), at)
                .await?;
            self.store.delete_live_work(thread_id).await?;
            let settings = self.store.settings().await?;
            let usage_armed = match outcome.usage_limit_resets_at {
                Some(reset) if settings.resume_on_usage_limit => {
                    self.arm_usage_limit(thread_id, reset, outcome.error.as_deref())
                        .await?
                }
                _ => false,
            };
            let mut restart_pending = false;
            if outcome.cut_by_restart && settings.continue_after_restart {
                self.record_restart_intent(
                    thread_id,
                    json!({"cause": "crash", "run_id": run_id, "turn_started_at": thread.turn_started_at}),
                    at,
                )
                .await?;
                restart_pending = true;
            }
            self.settle_child_task(&thread, &outcome, usage_armed, restart_pending)
                .await
        }
        .await;
        log_step("turn finished", result);
    }
}

fn non_empty(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

pub(crate) fn lock_poisoned<T>(_: std::sync::PoisonError<T>) -> OrchestratorError {
    OrchestratorError::Store("engine lock poisoned".into())
}

fn log_step(step: &str, result: Result<()>) {
    if let Err(error) = result {
        tracing::warn!(step, %error, "orchestrator step failed");
    }
}

/// The parent's wake-up turn carrying finished delegated results.
fn result_turn_text(tasks: &[TaskRecord]) -> String {
    let parts: Vec<String> = tasks
        .iter()
        .map(|t| {
            let secs = (t.finished_at.unwrap_or_else(now_ms)
                - t.started_at.unwrap_or(t.created_at))
                / 1000;
            let head = format!(
                "[T3 delegated task {}] \"{}\" — task {}, {} / {}, {}s, child thread {}",
                t.status,
                t.title,
                t.id,
                t.provider.as_deref().unwrap_or("?"),
                t.model.as_deref().unwrap_or("?"),
                secs,
                t.child_thread.as_ref().map_or("-", |c| c.as_str()),
            );
            let body = match (&t.status, &t.error) {
                (TaskStatus::Failed, Some(error)) => {
                    format!("Error: {error}\n\n{}", t.result.clone().unwrap_or_default())
                        .trim()
                        .to_string()
                }
                _ => t
                    .result
                    .clone()
                    .or_else(|| t.error.clone())
                    .unwrap_or_else(|| "(no output)".into()),
            };
            format!("{head}\n\n{}", clip(&body, 30_000))
        })
        .collect();
    format!(
        "Delegated work finished. Use these results to continue.\n\n{}",
        parts.join("\n\n---\n\n")
    )
}

pub(crate) fn clip_title(raw: &str) -> String {
    clip(raw.lines().next().unwrap_or("").trim(), 80)
}

pub(crate) fn json_ok(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        obj.entry("ok").or_insert(json!(true));
    }
    value
}

pub(crate) fn skip_note(reason: SkipReason) -> &'static str {
    reason.as_str()
}

pub(crate) fn continue_prompt() -> String {
    restart::CONTINUE_PROMPT.to_string()
}
