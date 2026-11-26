//! The one admission path of a thread's input (t3code's per-thread
//! dispatch lock): user sends, agent sends (`t3_thread send`), queue
//! promotes, deferred-steer delivery, automatic steers and queue drains all
//! run here, one at a time per thread.
//!
//! - Every message is one durable delivery row in `orch_queue`, inserted
//!   (or claimed, `pending → sending` by compare-and-set) before any
//!   runtime call and settled after the runtime answers. Its id is the
//!   delivery id across every attempt and fallback (steer → too late →
//!   start, not ready → deferred → steer), so no path sends it twice.
//! - The per-thread lock may be held across a runtime call; runtime
//!   callbacks (`TurnEventSink`) never take it: they only record durable
//!   facts (a run's end, a read, readiness) guarded by ledger state, and
//!   readiness wakes this path from a spawned task.
//! - Stop is recorded against its run (`crate::steers`) and checked on
//!   every attempt and fallback; a send started after the Stop is what
//!   lifted the hold.
//! - Ticks use `try_lock`: a busy thread is simply visited next tick.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::engine::{Engine, Result, json_ok, lock_poisoned};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::profile_runtime::UserTurn;
use crate::runtime::*;
use crate::steers::{Settle, SteerRecord};

pub(crate) type AdmissionLocks = StdMutex<HashMap<ThreadId, Arc<Mutex<()>>>>;

/// How long a send waits for a turn its runtime says has ended to be
/// recorded as ended, before it falls back to the queue head.
const TURN_END_WAIT: Duration = Duration::from_secs(3);
const TURN_END_POLL: Duration = Duration::from_millis(25);

/// How an agent send wants to reach a busy thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendIntent {
    /// Start, steer, or queue, whichever the thread can take.
    Auto,
    /// Steer the running turn; a refusal is reported, not queued.
    Steer,
    /// Restart the running turn with the text.
    Restart,
}

/// What an admission did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admitted {
    Turn(UserTurn),
    /// An explicit steer the runtime refused; nothing was delivered.
    Refused,
}

/// What a steer attempt did, and the ledger id it used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteerAttempt {
    pub(crate) outcome: SteerOutcome,
    pub(crate) id: SteerId,
}

fn new_claim() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl Engine {
    fn admission_lock(&self, thread: &ThreadId) -> Result<Arc<Mutex<()>>> {
        let mut locks = self.admission.lock().map_err(lock_poisoned)?;
        Ok(Arc::clone(
            locks
                .entry(thread.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ))
    }

    async fn admit(&self, thread: &ThreadId) -> Result<OwnedMutexGuard<()>> {
        Ok(self.admission_lock(thread)?.lock_owned().await)
    }

    /// The lock when no admission holds it (ticks skip busy threads).
    fn try_admit(&self, thread: &ThreadId) -> Result<Option<OwnedMutexGuard<()>>> {
        Ok(self.admission_lock(thread)?.try_lock_owned().ok())
    }

    async fn active_run(&self, thread_id: &ThreadId) -> Result<Option<String>> {
        Ok(self
            .store
            .thread(thread_id)
            .await?
            .and_then(|t| t.active_run_id))
    }

    /// Unread steers of ended runs back in the queue before anything is
    /// admitted. A failure stays durably retryable (the tick retries).
    pub(crate) async fn recover_first(&self, thread: &ThreadId) {
        if let Err(error) = self.recover_stranded(thread).await {
            tracing::debug!(%error, "steer recovery failed; the tick retries it");
        }
    }

    /// Older words waiting to run before a new message: recovered steers,
    /// and deferred steers whose run is gone.
    async fn carried_pending(&self, thread: &ThreadId) -> Result<bool> {
        Ok(self.store.queue(thread, false).await?.iter().any(|q| {
            q.status == QueueStatus::Pending
                && matches!(q.kind, QueueKind::RecoveredSteer | QueueKind::DeferredSteer)
        }))
    }

    // ── user and agent sends ────────────────────────────────────────────

    /// A message the user typed into an external-runtime thread: start a
    /// turn, steer it into the running one, or queue it behind that turn
    /// when the runtime cannot steer. A steer never restarts the turn. A
    /// steer the turn cannot take yet waits for it; one that is too late
    /// starts the next turn right away. Steers an ended turn never read run
    /// before this message.
    pub async fn user_turn(&self, thread: &ThreadRecord, text: &str) -> Result<UserTurn> {
        self.user_turn_once(thread, text, None).await
    }

    /// [`Self::user_turn`], once per `client_action_id`: a retried submit
    /// answers what the first one did and never reaches the runtime again.
    pub async fn user_turn_once(
        &self,
        thread: &ThreadRecord,
        text: &str,
        client_action_id: Option<&str>,
    ) -> Result<UserTurn> {
        let thread_id = &thread.thread_id;
        let _admission = self.admit(thread_id).await?;
        if let Some(key) = client_action_id
            && let Some(raw) = self.store.submission(thread_id, key).await?
        {
            return Ok(serde_json::from_str(&raw)?);
        }
        // A send started after a Stop releases its hold.
        self.on_user_prompt(thread_id).await?;
        let turn = match self
            .admit_locked(thread_id, text, Author::User, SendIntent::Auto)
            .await?
        {
            Admitted::Turn(turn) => turn,
            Admitted::Refused => {
                return Err(OrchestratorError::request(
                    "steer_rejected",
                    "The running turn refused the message.",
                ));
            }
        };
        if let Some(key) = client_action_id {
            self.store
                .put_submission(thread_id, key, &serde_json::to_string(&turn)?)
                .await?;
        }
        Ok(turn)
    }

    /// An agent's `t3_thread send` into another thread.
    pub(crate) async fn agent_send(
        &self,
        thread: &ThreadId,
        text: &str,
        intent: SendIntent,
    ) -> Result<Admitted> {
        let _admission = self.admit(thread).await?;
        self.admit_locked(thread, text, Author::Agent, intent).await
    }

    /// One send, under the thread's admission lock.
    async fn admit_locked(
        &self,
        thread_id: &ThreadId,
        text: &str,
        author: Author,
        intent: SendIntent,
    ) -> Result<Admitted> {
        self.recover_first(thread_id).await;
        let thread = self.store.thread(thread_id).await?.ok_or_else(|| {
            OrchestratorError::not_found("thread_not_found", format!("No thread {thread_id}."))
        })?;
        if !thread.is_running() {
            if intent != SendIntent::Auto {
                return Err(OrchestratorError::request(
                    "no_active_run",
                    "This mode needs an active turn in the target thread.",
                ));
            }
            if self.carried_pending(thread_id).await? {
                let queued_id = self
                    .store
                    .queue_insert(thread_id, text, author, QueueKind::Message, None, None)
                    .await?;
                self.drain_locked(thread_id).await?;
                return Ok(Admitted::Turn(UserTurn::Queued {
                    active_run_id: self.active_run(thread_id).await?,
                    steerable: true,
                    queued_id,
                }));
            }
        }
        let claim = new_claim();
        let delivery = self
            .store
            .queue_insert(
                thread_id,
                text,
                author,
                QueueKind::Message,
                None,
                Some(&claim),
            )
            .await?;
        if !thread.is_running() {
            // An agent's explicit send is not an automatic wake: no hold
            // check. The user's hold was lifted by this send, so a hold
            // now is a Stop that came after it.
            let respect_hold = author == Author::User;
            if let Some(turn) = self
                .start_claimed(thread_id, &delivery, &claim, text, author, respect_hold)
                .await?
            {
                return Ok(Admitted::Turn(turn));
            }
        }
        let Some(current) = self
            .store
            .thread(thread_id)
            .await?
            .filter(ThreadRecord::is_running)
        else {
            return self
                .settle_queued(thread_id, &delivery, &claim, true, false)
                .await;
        };
        let steerable = self
            .runtime(&current.runtime_id)?
            .capabilities()
            .steer_in_turn;
        // A compaction turn takes no steer: the message waits behind it.
        if self.compacting(thread_id).await? {
            if intent != SendIntent::Auto {
                self.store
                    .queue_settle(&delivery, &claim, &Settle::to(QueueStatus::Cancelled))
                    .await?;
                return Ok(Admitted::Refused);
            }
            return self
                .settle_queued(thread_id, &delivery, &claim, steerable, false)
                .await;
        }
        let how = if intent == SendIntent::Restart {
            SteerHow::Restart
        } else {
            SteerHow::Steer
        };
        let attempt = self
            .attempt_steer(&current, &delivery, &claim, text, author, how)
            .await;
        match attempt.as_ref().map(|a| a.outcome) {
            Ok(SteerOutcome::Steered) => Ok(Admitted::Turn(UserTurn::Steered {
                active_run_id: current.active_run_id.clone(),
                steer_id: attempt.ok().map(|a| a.id),
                waiting_for_turn: false,
                queued_id: None,
            })),
            Ok(SteerOutcome::Restarted) => {
                let run_id = self.active_run(thread_id).await?;
                let mut settle = Settle::to(QueueStatus::Sent);
                settle.claim_run = run_id.clone();
                self.store.queue_settle(&delivery, &claim, &settle).await?;
                Ok(Admitted::Turn(UserTurn::Restarted { run_id }))
            }
            Ok(SteerOutcome::NotReady) => {
                let settle = Settle {
                    kind: QueueKind::DeferredSteer,
                    steer_run: current.active_run_id.clone(),
                    ..Settle::to(QueueStatus::Pending)
                };
                self.store.queue_settle(&delivery, &claim, &settle).await?;
                Ok(Admitted::Turn(UserTurn::Steered {
                    active_run_id: current.active_run_id.clone(),
                    steer_id: None,
                    waiting_for_turn: true,
                    queued_id: Some(delivery),
                }))
            }
            Ok(SteerOutcome::TurnEnded) => {
                // Too late to steer: the next turn, right away. The lock
                // keeps queue drains from starting older items meanwhile.
                self.wait_turn_end(thread_id).await?;
                self.after_turn_end(thread_id, &delivery, &claim, text, author, steerable)
                    .await
            }
            other => {
                if let Err(error) = other {
                    tracing::debug!(%error, "steer failed; the message is kept");
                }
                if intent != SendIntent::Auto {
                    self.store
                        .queue_settle(&delivery, &claim, &Settle::to(QueueStatus::Cancelled))
                        .await?;
                    return Ok(Admitted::Refused);
                }
                // The turn may have ended while the steer was refused.
                if !self
                    .store
                    .thread(thread_id)
                    .await?
                    .is_some_and(|t| t.is_running())
                {
                    return self
                        .after_turn_end(thread_id, &delivery, &claim, text, author, steerable)
                        .await;
                }
                self.settle_queued(thread_id, &delivery, &claim, steerable, false)
                    .await
            }
        }
    }

    /// The turn a send meant to steer is gone: the send runs next, behind
    /// the steers that turn never read, unless a Stop came after it.
    async fn after_turn_end(
        &self,
        thread_id: &ThreadId,
        delivery: &QueueItemId,
        claim: &str,
        text: &str,
        author: Author,
        steerable: bool,
    ) -> Result<Admitted> {
        self.recover_first(thread_id).await;
        if self.carried_pending(thread_id).await? {
            self.store
                .queue_settle(delivery, claim, &Settle::to(QueueStatus::Pending))
                .await?;
            self.drain_locked(thread_id).await?;
            return Ok(Admitted::Turn(UserTurn::Queued {
                active_run_id: self.active_run(thread_id).await?,
                steerable,
                queued_id: delivery.clone(),
            }));
        }
        if let Some(turn) = self
            .start_claimed(thread_id, delivery, claim, text, author, true)
            .await?
        {
            return Ok(Admitted::Turn(turn));
        }
        self.settle_queued(thread_id, delivery, claim, steerable, true)
            .await
    }

    async fn settle_queued(
        &self,
        thread_id: &ThreadId,
        delivery: &QueueItemId,
        claim: &str,
        steerable: bool,
        front: bool,
    ) -> Result<Admitted> {
        let settle = Settle {
            front,
            ..Settle::to(QueueStatus::Pending)
        };
        self.store.queue_settle(delivery, claim, &settle).await?;
        Ok(Admitted::Turn(UserTurn::Queued {
            active_run_id: self.active_run(thread_id).await?,
            steerable,
            queued_id: delivery.clone(),
        }))
    }

    /// Start the claimed delivery as a new turn; `None` (claim kept) when
    /// the thread could not take one now.
    async fn start_claimed(
        &self,
        thread_id: &ThreadId,
        delivery: &QueueItemId,
        claim: &str,
        text: &str,
        author: Author,
        respect_hold: bool,
    ) -> Result<Option<UserTurn>> {
        let origin = if author == Author::User {
            TurnOrigin::UserPrompt
        } else {
            TurnOrigin::AgentSend
        };
        if !self
            .deliver(thread_id, text.to_string(), origin, respect_hold)
            .await?
        {
            return Ok(None);
        }
        let run_id = self.active_run(thread_id).await?;
        let mut settle = Settle::to(QueueStatus::Sent);
        settle.claim_run = run_id.clone();
        self.store.queue_settle(delivery, claim, &settle).await?;
        Ok(Some(UserTurn::Started { run_id }))
    }

    /// Wait (bounded) for a turn its runtime no longer has to be recorded
    /// as ended. `true` once the thread is idle.
    pub(crate) async fn wait_turn_end(&self, thread: &ThreadId) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + TURN_END_WAIT;
        loop {
            match self.store.thread(thread).await? {
                Some(t) if t.is_running() => {}
                _ => return Ok(true),
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(TURN_END_POLL).await;
        }
    }

    // ── one steer attempt ───────────────────────────────────────────────

    /// Hand a claimed delivery to the running turn as one steer, recorded
    /// in the ledger first (the runtime may report it read before it
    /// answers). A steered delivery is settled here; every other outcome
    /// leaves the claim to the caller. A steer never kills the turn: on a
    /// runtime without in-turn steering, or into a run a Stop reached, it
    /// is `Rejected` without a call.
    async fn attempt_steer(
        &self,
        thread: &ThreadRecord,
        delivery: &QueueItemId,
        claim: &str,
        text: &str,
        author: Author,
        how: SteerHow,
    ) -> Result<SteerAttempt> {
        let runtime = self.runtime(&thread.runtime_id)?;
        let id = SteerId::generate();
        let refused = (how == SteerHow::Steer && !runtime.capabilities().steer_in_turn)
            || self
                .store
                .run_stopped(&thread.thread_id, thread.active_run_id.as_deref())
                .await?;
        if refused {
            return Ok(SteerAttempt {
                outcome: SteerOutcome::Rejected,
                id,
            });
        }
        let now = now_ms();
        let steer = SteerRecord {
            id: id.clone(),
            thread_id: thread.thread_id.clone(),
            run_id: thread.active_run_id.clone(),
            text: text.to_string(),
            created_by: author,
            state: SteerState::Sending,
            queue_id: Some(delivery.clone()),
            sent_at: now,
            updated_at: now,
        };
        self.store.insert_steer(&steer).await?;
        let request = SteerRequest {
            id: id.clone(),
            text: text.to_string(),
            how,
        };
        let result = runtime.steer_message(thread, &request).await;
        match &result {
            Ok(SteerOutcome::Steered) => {
                let state = if runtime.reports_steer_reads() {
                    SteerState::Pending
                } else {
                    SteerState::Read
                };
                // Delivered: a failed write must not turn into a resend (boot
                // settles the claim as unconfirmed).
                if let Err(error) = self.store.settle_steered(&steer, state, claim).await {
                    tracing::warn!(%error, "steered delivery not settled; boot reconciles it");
                }
                // The run ended (or was stopped) while the call was out:
                // nobody else recovers a steer settled after that end.
                let live = self
                    .store
                    .thread(&thread.thread_id)
                    .await?
                    .is_some_and(|t| t.is_running() && t.active_run_id == thread.active_run_id);
                if !live {
                    self.recover_first(&thread.thread_id).await;
                }
            }
            // The restarted turn's prompt is this text.
            Ok(SteerOutcome::Restarted) => {
                self.store
                    .set_steer_state(
                        &thread.thread_id,
                        &id,
                        &[SteerState::Sending],
                        SteerState::Read,
                    )
                    .await?;
            }
            _ => {
                self.store
                    .set_steer_state(
                        &thread.thread_id,
                        &id,
                        &[SteerState::Sending],
                        SteerState::NotSent,
                    )
                    .await?;
            }
        }
        Ok(SteerAttempt {
            outcome: result?,
            id,
        })
    }

    // ── queue: promote, deferred steers, drain ──────────────────────────

    /// Deliver a queued item into the running turn (steer) or redirect it.
    /// A turn that cannot take input yet gets it as soon as it can (the
    /// item waits as a deferred steer).
    pub async fn queue_promote(
        &self,
        thread: &ThreadRecord,
        id: &QueueItemId,
        how: SteerHow,
    ) -> Result<Value> {
        let thread_id = &thread.thread_id;
        let _admission = self.admit(thread_id).await?;
        self.promote_locked(thread_id, id, how).await
    }

    /// Send now (t3code `queued-message.promote-to-steer` /
    /// `queued-run.start`), atomic under the thread's admission lock: a
    /// running turn takes the queued item as a steer (or as soon as it can
    /// take input); an idle thread starts a turn with just this item. A
    /// held queue stays held for every other item. The item is claimed by
    /// compare-and-set, so a drain that took it first wins and this answers
    /// `not_queued`, never a second delivery.
    pub async fn queue_send_now(&self, thread: &ThreadRecord, id: &QueueItemId) -> Result<Value> {
        let thread_id = &thread.thread_id;
        let _admission = self.admit(thread_id).await?;
        if self
            .store
            .thread(thread_id)
            .await?
            .is_some_and(|t| t.is_running())
        {
            match self.promote_locked(thread_id, id, SteerHow::Steer).await {
                // Too late to steer: it starts the next turn below.
                Err(OrchestratorError::Request {
                    code: "turn_ended" | "no_active_run",
                    ..
                }) => {
                    self.wait_turn_end(thread_id).await?;
                }
                other => return other,
            }
        }
        self.recover_first(thread_id).await;
        let item = self.pending_item(thread_id, id).await?;
        let claim = new_claim();
        if !self.store.queue_claim(id, &claim).await? {
            return Err(OrchestratorError::request(
                "not_queued",
                format!("Queued message {id} was sent meanwhile."),
            ));
        }
        let origin = if item.created_by == Author::User {
            TurnOrigin::UserPrompt
        } else {
            TurnOrigin::AgentSend
        };
        // `respect_hold: false` starts this one item; the hold itself (and
        // so every other queued item) is left as it is.
        let delivered = self
            .deliver(thread_id, item.text.clone(), origin, false)
            .await;
        let started = matches!(delivered, Ok(true));
        let mut settle = Settle::to(if started {
            QueueStatus::Sent
        } else {
            QueueStatus::Pending
        });
        if started {
            self.store.set_last_user(thread_id, now_ms()).await?;
            settle.claim_run = self.active_run(thread_id).await?;
        } else {
            settle.kind = item.kind;
            settle.steer_run = item.steer_run.clone();
        }
        self.store.queue_settle(id, &claim, &settle).await?;
        delivered?;
        if !started {
            return Err(OrchestratorError::request(
                "thread_busy",
                "The thread can't start a turn right now; the message stays queued.",
            ));
        }
        let mut view = self.queue_list(thread_id).await?;
        view["delivery"] = json!("started");
        view["runId"] = json!(settle.claim_run);
        Ok(json_ok(view))
    }

    /// [`Self::queue_promote`] under the admission lock.
    async fn promote_locked(
        &self,
        thread_id: &ThreadId,
        id: &QueueItemId,
        how: SteerHow,
    ) -> Result<Value> {
        self.recover_first(thread_id).await;
        let item = self.pending_item(thread_id, id).await?;
        let Some(current) = self
            .store
            .thread(thread_id)
            .await?
            .filter(ThreadRecord::is_running)
        else {
            return Err(OrchestratorError::request(
                "no_active_run",
                "The thread has no active turn to steer; it will run from the queue.",
            ));
        };
        if self.compacting(thread_id).await? {
            return Err(OrchestratorError::request(
                "compacting",
                "The thread is compacting its context; the message stays queued and runs after it.",
            ));
        }
        let claim = new_claim();
        if !self.store.queue_claim(id, &claim).await? {
            return Err(OrchestratorError::request(
                "not_queued",
                format!("Queued message {id} was sent meanwhile."),
            ));
        }
        let attempt = self
            .attempt_steer(&current, id, &claim, &item.text, item.created_by, how)
            .await;
        let outcome = match &attempt {
            Ok(a) => a.outcome,
            Err(_) => SteerOutcome::Rejected,
        };
        match outcome {
            SteerOutcome::Steered => {}
            SteerOutcome::Restarted => {
                let mut settle = Settle::to(QueueStatus::Sent);
                settle.claim_run = self.active_run(thread_id).await?;
                self.store.queue_settle(id, &claim, &settle).await?;
            }
            SteerOutcome::NotReady => {
                let settle = Settle {
                    kind: QueueKind::DeferredSteer,
                    steer_run: current.active_run_id.clone(),
                    ..Settle::to(QueueStatus::Pending)
                };
                self.store.queue_settle(id, &claim, &settle).await?;
            }
            SteerOutcome::TurnEnded | SteerOutcome::Rejected => {
                let settle = Settle {
                    kind: item.kind,
                    steer_run: item.steer_run.clone(),
                    ..Settle::to(QueueStatus::Pending)
                };
                self.store.queue_settle(id, &claim, &settle).await?;
                attempt?;
                return Err(if outcome == SteerOutcome::TurnEnded {
                    OrchestratorError::request(
                        "turn_ended",
                        "The turn ended before it could take the message; it runs next from the queue.",
                    )
                } else {
                    OrchestratorError::request(
                        "steer_rejected",
                        "The active turn can't take it mid-turn; it stays queued and runs when the turn ends.",
                    )
                });
            }
        }
        let mut view = self.queue_list(thread_id).await?;
        view["steerStatus"] = json!(steer_label(outcome));
        view["delivery"] = json!(match outcome {
            SteerOutcome::Restarted => "restarted",
            SteerOutcome::NotReady => "waiting_for_turn",
            _ => "steered",
        });
        if outcome == SteerOutcome::Steered
            && let Ok(a) = &attempt
        {
            view["steerId"] = json!(a.id.as_str());
        }
        Ok(json_ok(view))
    }

    /// Deliver the thread's deferred steers into its running turn, in queue
    /// order. One whose run ended, or that a Stop reached, becomes an
    /// ordinary queued message, keeping its place. Off the tick, a run is
    /// tried only once its runtime reported it ready.
    pub(crate) async fn deliver_deferred_steers(
        &self,
        thread_id: &ThreadId,
        from_tick: bool,
    ) -> Result<()> {
        let _admission = if from_tick {
            match self.try_admit(thread_id)? {
                Some(guard) => guard,
                None => return Ok(()),
            }
        } else {
            self.admit(thread_id).await?
        };
        self.recover_first(thread_id).await;
        for item in self.store.queue(thread_id, false).await? {
            if item.kind != QueueKind::DeferredSteer || item.status != QueueStatus::Pending {
                continue;
            }
            let thread = self.store.thread(thread_id).await?;
            let live = thread.as_ref().filter(|t| {
                t.is_running() && (item.steer_run.is_none() || t.active_run_id == item.steer_run)
            });
            let stopped = self
                .store
                .run_stopped(thread_id, item.steer_run.as_deref())
                .await?;
            let Some(thread) = live.filter(|_| !stopped) else {
                self.store
                    .set_pending_kind(&item.id, QueueKind::Message, None)
                    .await?;
                continue;
            };
            if !from_tick
                && !self
                    .store
                    .run_ready(thread_id, thread.active_run_id.as_deref())
                    .await?
            {
                return Ok(());
            }
            let claim = new_claim();
            if !self.store.queue_claim(&item.id, &claim).await? {
                continue;
            }
            let attempt = self
                .attempt_steer(
                    thread,
                    &item.id,
                    &claim,
                    &item.text,
                    item.created_by,
                    SteerHow::Steer,
                )
                .await;
            match attempt.as_ref().map(|a| a.outcome) {
                Ok(SteerOutcome::Steered) => {}
                Ok(SteerOutcome::Restarted) => {
                    self.store
                        .queue_settle(&item.id, &claim, &Settle::to(QueueStatus::Sent))
                        .await?;
                }
                Ok(SteerOutcome::NotReady) => {
                    let settle = Settle {
                        kind: QueueKind::DeferredSteer,
                        steer_run: item.steer_run.clone(),
                        ..Settle::to(QueueStatus::Pending)
                    };
                    self.store.queue_settle(&item.id, &claim, &settle).await?;
                    // Later ones would overtake it.
                    return Ok(());
                }
                other => {
                    if let Err(error) = other {
                        tracing::debug!(%error, "deferred steer failed; it runs from the queue");
                    }
                    self.store
                        .queue_settle(&item.id, &claim, &Settle::to(QueueStatus::Pending))
                        .await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Tick step: retry deferred steers (runtimes that never report ready).
    pub(crate) async fn deliver_all_deferred_steers(&self) -> Result<()> {
        for thread in self.store.deferred_steer_threads().await? {
            self.deliver_deferred_steers(&thread, true).await?;
        }
        Ok(())
    }

    /// The runtime reported run `run` ready for steers: recorded, then this
    /// path wakes from its own task, never inside the runtime's callback.
    pub(crate) async fn note_turn_ready(&self, thread: &ThreadId, run: Option<&str>) -> Result<()> {
        let run = match run {
            Some(run) => Some(run.to_string()),
            None => self.active_run(thread).await?,
        };
        self.store.record_run_ready(thread, run.as_deref()).await?;
        let (Some(engine), Ok(handle)) = (self.me.upgrade(), tokio::runtime::Handle::try_current())
        else {
            return Ok(()); // silent-ok: the tick delivers deferred steers too
        };
        let thread = thread.clone();
        handle.spawn(async move {
            if let Err(error) = engine.deliver_deferred_steers(&thread, false).await {
                tracing::debug!(%error, "deferred steers not delivered; the tick retries");
            }
        });
        Ok(())
    }

    /// Start the head of an idle, unheld thread's queue as a turn, unless
    /// an admission holds the thread (the next tick tries again). `true`
    /// when one started.
    pub(crate) async fn drain_thread(&self, thread_id: &ThreadId) -> Result<bool> {
        let Some(_admission) = self.try_admit(thread_id)? else {
            return Ok(false);
        };
        self.drain_locked(thread_id).await
    }

    async fn drain_locked(&self, thread_id: &ThreadId) -> Result<bool> {
        self.recover_first(thread_id).await;
        if self.store.hold(thread_id).await?.is_some() {
            return Ok(false);
        }
        match self.store.thread(thread_id).await? {
            Some(t) if !t.is_running() => {}
            _ => return Ok(false),
        }
        let queue = self.store.queue(thread_id, false).await?;
        // One delivery in flight per thread (a crash left claim is settled
        // at boot).
        if queue.iter().any(|q| q.status == QueueStatus::Sending) {
            return Ok(false);
        }
        let Some(head) = queue.into_iter().find(|q| q.status == QueueStatus::Pending) else {
            return Ok(false);
        };
        let claim = new_claim();
        if !self.store.queue_claim(&head.id, &claim).await? {
            return Ok(false);
        }
        let delivered = self
            .deliver_queued(
                thread_id,
                head.text.clone(),
                head.created_by == Author::User,
            )
            .await;
        let started = matches!(delivered, Ok(true));
        let mut settle = Settle::to(if started {
            QueueStatus::Sent
        } else {
            QueueStatus::Pending
        });
        if started {
            self.store.set_last_user(thread_id, now_ms()).await?;
            settle.claim_run = self.active_run(thread_id).await?;
        } else {
            settle.kind = head.kind;
            settle.steer_run = head.steer_run.clone();
        }
        self.store.queue_settle(&head.id, &claim, &settle).await?;
        delivered?;
        Ok(started)
    }

    /// Automatic input (a delegated result, a context handoff) for a busy
    /// `thread`: steered into its running turn only when the runtime steers
    /// without cancelling tool calls (t3code's rule for wakes, which never
    /// restart). `false`: not delivered, the caller waits for the turn end.
    pub(crate) async fn steer_automatic(&self, thread_id: &ThreadId, text: &str) -> Result<bool> {
        let Some(_admission) = self.try_admit(thread_id)? else {
            return Ok(false);
        };
        let Some(thread) = self.store.thread(thread_id).await? else {
            return Ok(false);
        };
        if !thread.is_running() || self.store.hold(thread_id).await?.is_some() {
            return Ok(false);
        }
        let runtime = self.runtime(&thread.runtime_id)?;
        if !runtime.capabilities().steer_in_turn || runtime.steer_interrupts_tools() {
            return Ok(false);
        }
        let claim = new_claim();
        let delivery = self
            .store
            .queue_insert(
                thread_id,
                text,
                Author::System,
                QueueKind::Message,
                None,
                Some(&claim),
            )
            .await?;
        let attempt = self
            .attempt_steer(
                &thread,
                &delivery,
                &claim,
                text,
                Author::System,
                SteerHow::Steer,
            )
            .await;
        if matches!(
            attempt.as_ref().map(|a| a.outcome),
            Ok(SteerOutcome::Steered)
        ) {
            return Ok(true);
        }
        if let Err(error) = &attempt {
            tracing::debug!(%error, "automatic steer failed; it waits for the turn end");
        }
        // The input itself stays pending where it came from.
        self.store
            .queue_settle(&delivery, &claim, &Settle::to(QueueStatus::Cancelled))
            .await?;
        Ok(false)
    }
}

pub(crate) fn steer_label(outcome: SteerOutcome) -> &'static str {
    match outcome {
        SteerOutcome::Steered => "steered",
        SteerOutcome::Restarted => "restarted",
        SteerOutcome::Rejected => "rejected",
        SteerOutcome::NotReady => "waiting_for_turn",
        SteerOutcome::TurnEnded => "turn_ended",
    }
}
