//! The thread's ONE dispatch decision for steers a run never read.
//!
//! Every path that hands a native steer to a run other than the one it was
//! sent to goes through [`NativeSteerDispatch`]: a run's terminal settlement
//! (unread and read-but-unconfirmed steers), a message that arrived while
//! its run was ending, a Stop that holds them, the startup and periodic
//! sweep, and — before any new message is admitted to the thread — the
//! inline pass that lets older unread steers claim the thread first.
//!
//! Durable state:
//! - the work: [`SteerHandoff`] documents in the [`SteerHandoffStore`],
//!   apart from the run's queue (independently retried, never reclaimed by
//!   the queue's cleanup);
//! - the decision: the transcript row, moved only through the guarded
//!   delivery transition (`SessionThreadService::change_message_delivery`).
//!   A message is first CLAIMED for its successor run — a run id derived
//!   from (ended run, message), so every replay names the same one — and
//!   only then made runnable (the successor submitted under that id with a
//!   keyed submit), or moved, still guarded, into a newer run that took the
//!   thread first. A replay that finds the row anywhere else knows another
//!   writer already decided and settles without acting, so no schedule
//!   delivers a message twice.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_loop_contracts::LoopInput;
use ironclaw_loop_host::{
    EnqueueQueuedMessageRequest, HandoffSteer, HandoffSteerKind, HostInputEnqueuePort,
    HostInputQueueError, SteerHandoff, SteerHandoffDispatch, SteerHandoffStore, SteerWithdrawal,
};
use ironclaw_threads::{
    GuardedDelivery, MessageDelivery, MessageDeliveryChange, MessageStatus, SessionThreadService,
    ThreadMessageId, ThreadMessageRecord, ThreadScope,
};
use ironclaw_turns::{
    AcceptedMessageRef, GetRunStateRequest, IdempotencyKey, LoopMessageRef, SubmitTurnRequest,
    SubmitTurnResponse, TurnCoordinator, TurnError, TurnRunId, TurnRunState, TurnScope, TurnStatus,
};

use crate::approval_interaction::ApprovalInteractionService;

/// The channel side of a handoff, implemented by the delivery coordinator
/// from the ended run's durable publication targets (the conversations a
/// channel message came from).
#[async_trait]
pub trait SuccessorReplyTargets: Send + Sync {
    /// The successor answers where the ended run answered, so a steer
    /// delivered as the next turn is answered where it was sent.
    async fn inherit(
        &self,
        scope: &TurnScope,
        ended_run_id: TurnRunId,
        successor_run_id: TurnRunId,
    ) -> Result<(), String>;

    /// A Stop left `message_id` unread: tell the conversation it came from
    /// that it was not delivered (keyed by the message: once).
    async fn notify_not_delivered(
        &self,
        scope: &TurnScope,
        ended_run_id: TurnRunId,
        message_id: ThreadMessageId,
    ) -> Result<(), String>;
}

/// Backoff for a handoff whose thread is still held (the ended run not yet
/// released, or a newer run ending): ~1 minute in all, then the sweep.
const RETRY_FIRST_PAUSE: Duration = Duration::from_millis(25);
const RETRY_MAX_PAUSE: Duration = Duration::from_secs(2);
const RETRY_ATTEMPTS: u32 = 40;

/// What one dispatch pass over a thread left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPass {
    /// Nothing left to hand off on the thread.
    Idle,
    /// A message must wait (its thread is held); later ones wait behind it.
    Waiting,
}

enum Step {
    Settled,
    Wait(String),
}

/// See the module docs.
pub struct NativeSteerDispatch {
    turn_coordinator: Arc<dyn TurnCoordinator>,
    thread_service: Arc<dyn SessionThreadService>,
    input_enqueue: Arc<dyn HostInputEnqueuePort>,
    store: Arc<dyn SteerHandoffStore>,
    reply_targets: Option<Arc<dyn SuccessorReplyTargets>>,
    approvals: Option<Arc<dyn ApprovalInteractionService>>,
    this: Weak<NativeSteerDispatch>,
}

impl NativeSteerDispatch {
    /// `reply_targets`: the delivery coordinator(s) a successor's answer is
    /// published through (`None` in a deployment with no channel delivery).
    /// `approvals`: declines an approval a handed-off steer lands on.
    pub fn new(
        turn_coordinator: Arc<dyn TurnCoordinator>,
        thread_service: Arc<dyn SessionThreadService>,
        input_enqueue: Arc<dyn HostInputEnqueuePort>,
        store: Arc<dyn SteerHandoffStore>,
        reply_targets: Option<Arc<dyn SuccessorReplyTargets>>,
        approvals: Option<Arc<dyn ApprovalInteractionService>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            turn_coordinator,
            thread_service,
            input_enqueue,
            store,
            reply_targets,
            approvals,
            this: this.clone(),
        })
    }

    /// The port the input queue tells about each recorded handoff.
    pub fn as_queue_dispatch(self: &Arc<Self>) -> Arc<dyn SteerHandoffDispatch> {
        Arc::new(QueueDispatch(Arc::clone(self)))
    }

    /// One pass over every open handoff (startup, then periodically): the
    /// durable retry of a handoff whose notification or retries ran out.
    pub async fn sweep(&self) {
        let pending = match self.store.pending().await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::debug!(%error, "steer handoff sweep could not list handoffs");
                return;
            }
        };
        let mut threads: Vec<(ThreadScope, ThreadId)> = Vec::new();
        for handoff in pending {
            if !threads.iter().any(|(_, id)| id == &handoff.thread_id) {
                threads.push((handoff.thread_scope, handoff.thread_id));
            }
        }
        for (scope, thread_id) in threads {
            self.dispatch_thread(&scope, &thread_id).await;
        }
    }

    /// Run [`Self::sweep`] now and then every `every`, for the life of the
    /// returned task.
    pub fn spawn_sweeper(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                this.sweep().await;
                tokio::time::sleep(every).await;
            }
        })
    }

    /// One pass over the thread's handoffs, oldest first. Called inline
    /// before a new message is admitted, so older unread steers claim the
    /// thread ahead of it.
    pub async fn dispatch_thread(
        &self,
        thread_scope: &ThreadScope,
        thread_id: &ThreadId,
    ) -> DispatchPass {
        let handoffs = match self.store.for_thread(thread_id).await {
            Ok(handoffs) => handoffs,
            Err(error) => {
                tracing::debug!(%error, %thread_id, "steer handoffs could not be read");
                return DispatchPass::Waiting;
            }
        };
        for handoff in handoffs
            .into_iter()
            .filter(|handoff| &handoff.thread_scope == thread_scope)
        {
            let mut settled = Vec::new();
            let mut waiting = None;
            for steer in &handoff.steers {
                match self.deliver(&handoff, steer).await {
                    Step::Settled => settled.push(steer.message_id),
                    Step::Wait(reason) => {
                        waiting = Some(reason);
                        break;
                    }
                }
            }
            if let Err(error) = self.store.settle(handoff.ended_run_id, &settled).await {
                tracing::debug!(%error, "settled steers stay in the handoff for the next pass");
                return DispatchPass::Waiting;
            }
            if let Some(reason) = waiting {
                tracing::debug!(
                    ended_run_id = %handoff.ended_run_id,
                    %reason,
                    "steer handoff waits; later messages wait behind it"
                );
                return DispatchPass::Waiting;
            }
        }
        DispatchPass::Idle
    }

    /// Stage a message that arrived while `ending_run` was ending: it goes
    /// to the next turn, behind that run's unread steers. The row is already
    /// `Queued(ending_run)`.
    pub(crate) async fn stage_after_end(
        &self,
        thread_scope: &ThreadScope,
        thread_id: &ThreadId,
        turn_scope: &TurnScope,
        ending_run: TurnRunId,
        message_id: ThreadMessageId,
        message_ref: LoopMessageRef,
    ) -> Result<(), HostInputQueueError> {
        self.store
            .record(SteerHandoff {
                ended_run_id: ending_run,
                thread_scope: thread_scope.clone(),
                thread_id: thread_id.clone(),
                turn_scope: Some(turn_scope.clone()),
                stopped: false,
                recorded_at_ms: chrono::Utc::now().timestamp_millis(),
                steers: vec![HandoffSteer {
                    message_id,
                    message_ref,
                    kind: HandoffSteerKind::AfterEnd,
                }],
            })
            .await?;
        self.notify(thread_scope.clone(), thread_id.clone());
        Ok(())
    }

    /// Dispatch the thread in the background, retrying (bounded) while it
    /// is held; the sweep takes over after that.
    fn notify(&self, thread_scope: ThreadScope, thread_id: ThreadId) {
        let Some(this) = self.this.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let mut pause = RETRY_FIRST_PAUSE;
            for _ in 0..RETRY_ATTEMPTS {
                if this.dispatch_thread(&thread_scope, &thread_id).await == DispatchPass::Idle {
                    return;
                }
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(RETRY_MAX_PAUSE);
            }
            tracing::debug!(%thread_id, "steer handoff still waiting; the sweep retries it");
        });
    }

    async fn row(
        &self,
        handoff: &SteerHandoff,
        message_id: ThreadMessageId,
    ) -> Result<Option<ThreadMessageRecord>, String> {
        self.thread_service
            .read_thread_message(&handoff.thread_scope, &handoff.thread_id, message_id)
            .await
            .map_err(|error| format!("row read failed: {error}"))
    }

    async fn change(
        &self,
        handoff: &SteerHandoff,
        message_id: ThreadMessageId,
        expected: MessageDelivery,
        change: MessageDeliveryChange,
    ) -> Result<GuardedDelivery, String> {
        self.thread_service
            .change_message_delivery(
                &handoff.thread_scope,
                &handoff.thread_id,
                message_id,
                &expected,
                change,
            )
            .await
            .map_err(|error| format!("guarded delivery change failed: {error}"))
    }

    async fn deliver(&self, handoff: &SteerHandoff, steer: &HandoffSteer) -> Step {
        match self.deliver_inner(handoff, steer).await {
            Ok(step) => step,
            Err(reason) => Step::Wait(reason),
        }
    }

    async fn deliver_inner(
        &self,
        handoff: &SteerHandoff,
        steer: &HandoffSteer,
    ) -> Result<Step, String> {
        let ended = handoff.ended_run_id;
        let ended_owner = ended.to_string();
        let successor = successor_run_id(ended, steer.message_id);
        let successor_owner = successor.to_string();
        let Some(row) = self.row(handoff, steer.message_id).await? else {
            return Ok(Step::Settled);
        };

        // Where the row is decides what is left to do; anything else means
        // another writer already decided this message.
        let at_ended = MessageDelivery::Queued {
            run_id: ended_owner.clone(),
        }
        .matches(&row)
            || (steer.kind == HandoffSteerKind::ReadUnconfirmed
                && MessageDelivery::Submitted {
                    run_id: ended_owner.clone(),
                }
                .matches(&row));
        let claimed = MessageDelivery::Queued {
            run_id: successor_owner.clone(),
        }
        .matches(&row);
        if !at_ended && !claimed {
            return Ok(Step::Settled);
        }

        // A Stop holds what was meant for the stopped run: not delivered,
        // the person decides. A message sent after the end still goes.
        if handoff.stopped && steer.kind != HandoffSteerKind::AfterEnd && at_ended {
            let expected = current_delivery(&row);
            if let GuardedDelivery::Applied(_) = self
                .change(
                    handoff,
                    steer.message_id,
                    expected,
                    MessageDeliveryChange::NotDelivered,
                )
                .await?
                && let Some(targets) = &self.reply_targets
            {
                let scope = handoff
                    .turn_scope
                    .clone()
                    .unwrap_or_else(|| derived_turn_scope(handoff));
                if let Err(error) = targets
                    .notify_not_delivered(&scope, ended, steer.message_id)
                    .await
                {
                    tracing::debug!(%error, "not-delivered channel notice failed (best-effort)");
                }
            }
            return Ok(Step::Settled);
        }

        if at_ended {
            // Claim it for the successor BEFORE anything can run it.
            match self
                .change(
                    handoff,
                    steer.message_id,
                    current_delivery(&row),
                    MessageDeliveryChange::Queue {
                        run_id: successor_owner.clone(),
                    },
                )
                .await?
            {
                GuardedDelivery::Applied(_) => {}
                GuardedDelivery::Moved(now)
                    if MessageDelivery::Queued {
                        run_id: successor_owner.clone(),
                    }
                    .matches(&now) => {}
                GuardedDelivery::Moved(_) => return Ok(Step::Settled),
            }
        }

        let turn_scope = handoff
            .turn_scope
            .clone()
            .unwrap_or_else(|| derived_turn_scope(handoff));
        let ended_state = match self
            .turn_coordinator
            .get_run_state(GetRunStateRequest {
                scope: turn_scope.clone(),
                run_id: ended,
            })
            .await
        {
            Ok(state) => state,
            Err(TurnError::ScopeNotFound) => {
                // A legacy entry whose scope cannot be derived: it can only
                // be settled as not delivered.
                return self
                    .not_delivered_from(handoff, steer, &successor_owner)
                    .await;
            }
            Err(error) => return Err(format!("ended run state read failed: {error}")),
        };
        let Some(actor) = ended_state.actor.clone() else {
            return self
                .not_delivered_from(handoff, steer, &successor_owner)
                .await;
        };
        let accepted_message_ref = AcceptedMessageRef::new(steer.message_ref.as_str())
            .map_err(|error| format!("handoff message ref invalid: {error}"))?;
        let idempotency_key =
            IdempotencyKey::new(format!("steer-handoff:{ended}:{}", steer.message_id))
                .map_err(|error| format!("handoff idempotency key invalid: {error}"))?;
        let submitted = self
            .turn_coordinator
            .submit_turn(SubmitTurnRequest {
                scope: turn_scope.clone(),
                actor,
                accepted_message_ref,
                requested_run_profile: None,
                output_contract: None,
                requested_model: requested_model(&ended_state),
                idempotency_key,
                received_at: chrono::Utc::now(),
                requested_run_id: Some(successor),
                parent_run_id: None,
                subagent_depth: 0,
                spawn_tree_root_run_id: None,
                product_context: ended_state.product_context.clone(),
                subagent_activation_provenance: None,
            })
            .await;
        match submitted {
            Ok(SubmitTurnResponse::Accepted {
                turn_id, run_id, ..
            }) => {
                self.change(
                    handoff,
                    steer.message_id,
                    MessageDelivery::Queued {
                        run_id: successor_owner,
                    },
                    MessageDeliveryChange::Submit {
                        turn_id: turn_id.to_string(),
                        run_id: run_id.to_string(),
                    },
                )
                .await?;
                if let Some(targets) = &self.reply_targets
                    && let Err(error) = targets.inherit(&turn_scope, ended, run_id).await
                {
                    tracing::debug!(
                        %error,
                        %run_id,
                        "successor run could not inherit its ended run's reply targets"
                    );
                }
                Ok(Step::Settled)
            }
            Err(TurnError::ThreadBusy(busy)) if busy.active_run_id == ended => Ok(Step::Wait(
                "the ended run still holds the thread".to_string(),
            )),
            Err(TurnError::ThreadBusy(busy)) if busy.active_run_id == successor => {
                // An earlier attempt started it; the submit flip is all that
                // is left.
                let state = self
                    .turn_coordinator
                    .get_run_state(GetRunStateRequest {
                        scope: turn_scope,
                        run_id: successor,
                    })
                    .await
                    .map_err(|error| format!("successor state read failed: {error}"))?;
                self.change(
                    handoff,
                    steer.message_id,
                    MessageDelivery::Queued {
                        run_id: successor_owner,
                    },
                    MessageDeliveryChange::Submit {
                        turn_id: state.turn_id.to_string(),
                        run_id: successor.to_string(),
                    },
                )
                .await?;
                Ok(Step::Settled)
            }
            Err(TurnError::ThreadBusy(busy)) => {
                self.steer_into(
                    handoff,
                    steer,
                    &turn_scope,
                    &successor_owner,
                    busy.active_run_id,
                )
                .await
            }
            Err(error) => Err(format!("successor submit failed: {error}")),
        }
    }

    /// Another run took the thread first: steer the message into it, the
    /// row moved (guarded) before the input is enqueued, and fenced after:
    /// a run that turned out to be ending gives it back and the message
    /// waits for the thread.
    async fn steer_into(
        &self,
        handoff: &SteerHandoff,
        steer: &HandoffSteer,
        turn_scope: &TurnScope,
        successor_owner: &str,
        active: TurnRunId,
    ) -> Result<Step, String> {
        let state = match self
            .turn_coordinator
            .get_run_state(GetRunStateRequest {
                scope: turn_scope.clone(),
                run_id: active,
            })
            .await
        {
            Ok(state) => state,
            Err(TurnError::ScopeNotFound) => {
                // Another person's run in a shared thread: wait for it.
                return Ok(Step::Wait(
                    "another person's run holds the thread".to_string(),
                ));
            }
            Err(error) => return Err(format!("active run state read failed: {error}")),
        };
        if run_is_ending(&state) || !state.allow_steering {
            return Ok(Step::Wait(
                "the run holding the thread cannot take it".to_string(),
            ));
        }
        let active_owner = active.to_string();
        match self
            .change(
                handoff,
                steer.message_id,
                MessageDelivery::Queued {
                    run_id: successor_owner.to_string(),
                },
                MessageDeliveryChange::Queue {
                    run_id: active_owner.clone(),
                },
            )
            .await?
        {
            GuardedDelivery::Applied(_) => {}
            GuardedDelivery::Moved(_) => return Ok(Step::Settled),
        }
        let enqueued = self
            .input_enqueue
            .enqueue_user_steer(
                EnqueueQueuedMessageRequest {
                    run_id: active,
                    turn_id: state.turn_id,
                    scope: handoff.thread_scope.clone(),
                    thread_id: handoff.thread_id.clone(),
                    message_id: steer.message_id,
                    input: LoopInput::Steering {
                        message_ref: steer.message_ref.clone(),
                    },
                    ack_effect: None,
                },
                turn_scope.clone(),
            )
            .await;
        let give_back = match enqueued {
            Ok(_) => {
                let after = self
                    .turn_coordinator
                    .get_run_state(GetRunStateRequest {
                        scope: turn_scope.clone(),
                        run_id: active,
                    })
                    .await
                    .map_err(|error| format!("active run state read failed: {error}"))?;
                if run_is_ending(&after) {
                    matches!(
                        self.input_enqueue
                            .withdraw_user_steer(active, steer.message_id)
                            .await
                            .map_err(|error| format!("steer withdraw failed: {error}"))?,
                        SteerWithdrawal::Withdrawn
                    )
                } else {
                    if after.status == TurnStatus::BlockedApproval {
                        self.decline_for(handoff, steer, turn_scope, after).await;
                    }
                    false
                }
            }
            Err(HostInputQueueError::RunClosed) => true,
            Err(error) => return Err(format!("steer enqueue failed: {error}")),
        };
        if give_back {
            // Back to the successor's claim; it waits for the thread.
            self.change(
                handoff,
                steer.message_id,
                MessageDelivery::Queued {
                    run_id: active_owner,
                },
                MessageDeliveryChange::Queue {
                    run_id: successor_owner.to_string(),
                },
            )
            .await?;
            return Ok(Step::Wait("the run it would steer is ending".to_string()));
        }
        Ok(Step::Settled)
    }

    async fn decline_for(
        &self,
        handoff: &SteerHandoff,
        steer: &HandoffSteer,
        turn_scope: &TurnScope,
        state: TurnRunState,
    ) {
        let Ok(accepted_message_ref) = AcceptedMessageRef::new(steer.message_ref.as_str()) else {
            return;
        };
        let request = crate::steering::SteeringAdmissionRequest {
            turn_scope: turn_scope.clone(),
            thread_scope: handoff.thread_scope.clone(),
            message_id: steer.message_id,
            accepted_message_ref,
            active_run_id: state.run_id,
        };
        crate::steering::decline_pending_approval(
            self.turn_coordinator.as_ref(),
            self.approvals.as_deref(),
            &request,
            state,
        )
        .await;
    }

    async fn not_delivered_from(
        &self,
        handoff: &SteerHandoff,
        steer: &HandoffSteer,
        owner: &str,
    ) -> Result<Step, String> {
        self.change(
            handoff,
            steer.message_id,
            MessageDelivery::Queued {
                run_id: owner.to_string(),
            },
            MessageDeliveryChange::NotDelivered,
        )
        .await?;
        Ok(Step::Settled)
    }
}

struct QueueDispatch(Arc<NativeSteerDispatch>);

#[async_trait]
impl SteerHandoffDispatch for QueueDispatch {
    async fn handoff_recorded(&self, thread_scope: &ThreadScope, thread_id: &ThreadId) {
        self.0.notify(thread_scope.clone(), thread_id.clone());
    }
}

/// The successor run a handed-off message starts: the same id on every
/// replay, so retries and concurrent dispatchers converge on one run.
pub(crate) fn successor_run_id(ended: TurnRunId, message_id: ThreadMessageId) -> TurnRunId {
    TurnRunId::from_uuid(uuid::Uuid::new_v5(
        &ended.as_uuid(),
        message_id.to_string().as_bytes(),
    ))
}

fn current_delivery(row: &ThreadMessageRecord) -> MessageDelivery {
    let run_id = row.turn_run_id.clone().unwrap_or_default();
    if row.status == MessageStatus::Submitted {
        MessageDelivery::Submitted { run_id }
    } else {
        MessageDelivery::Queued { run_id }
    }
}

pub(crate) fn run_is_ending(state: &TurnRunState) -> bool {
    state.status.is_terminal() || state.status == TurnStatus::CancelRequested
}

/// The turn scope of a handoff recorded without one (an entry queued by an
/// older build): the thread's own scope, owned by its owner.
fn derived_turn_scope(handoff: &SteerHandoff) -> TurnScope {
    TurnScope::new_with_owner(
        handoff.thread_scope.tenant_id.clone(),
        Some(handoff.thread_scope.agent_id.clone()),
        handoff.thread_scope.project_id.clone(),
        handoff.thread_id.clone(),
        handoff.thread_scope.owner_user_id.clone(),
    )
}

/// The ended run's model carries over; an unresolvable pick falls back to
/// the active model, as any submit's would.
fn requested_model(ended: &TurnRunState) -> Option<String> {
    ended.resolved_model_route.as_ref().and_then(|route| {
        if route.is_advisory() {
            Some(route.model_id().to_string())
        } else {
            ironclaw_product_contracts::operator_llm::NativeModelRoute::turn_model(
                Some(route.provider_id()),
                Some(route.model_id()),
                None,
            )
        }
    })
}

#[cfg(test)]
#[path = "steering_dispatch_tests.rs"]
mod tests;
