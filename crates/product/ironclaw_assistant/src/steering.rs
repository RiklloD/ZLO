//! Busy-run steering admission — the single owner of the queued-message flow.
//!
//! Both the product inbound-turn path ([`crate::inbound_turn`]) and the WebUI
//! facade ([`crate::reborn_services`]) admit a user message that hits a busy
//! thread through this module: mark the row `Queued`, enqueue it as steering
//! input for the active run, and settle it as `RejectedBusy` when steering
//! cannot serve it. The DISPOSITION policy lives here — callers receive a
//! classified [`SteeringAdmission`] and only build their surface-specific
//! response; they never enumerate queue error variants, so the two surfaces
//! cannot drift on ordering, idempotency, settle semantics, or error fidelity.
//!
//! Two entry points cover the two lifecycle moments:
//! - [`admit_busy_steering`] — a FRESH busy submit: the row is `Accepted` and
//!   must be marked `Queued` (reconciling the races a failed mark can hide)
//!   before the input becomes drainable.
//! - [`readmit_queued_steering`] — a REPLAY of an already-`Queued` row
//!   (crash-orphan recovery): the busy path persists `Queued` before
//!   enqueueing, so a crash between the two must not leave the replay
//!   treating the row as proof of enqueue; re-enqueueing is idempotent (the
//!   queue dedups by the message's own ref).
//!
//! Steering follows t3code: a message sent while a run works steers it.
//! - A run that is still ending (terminal, cancelling, or its queue already
//!   closed) cannot take a steer; admission reports
//!   [`SteeringAdmission::RunEnding`] without settling the row, and the
//!   caller retries the submit so the message starts the next turn.
//! - A run parked on an approval: the message is queued as a steer, then the
//!   pending approval is declined so the run continues with it (the text
//!   goes once, as the person's message — never also as decline feedback).
//! - A steer the run never read when it ended by itself is redelivered as the
//!   next turn by [`SteeringRedelivery`] (bound on the input queue).

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_loop_contracts::LoopInput;
use ironclaw_loop_host::{
    EnqueueQueuedMessageRequest, HostInputEnqueuePort, HostInputQueueError,
    HostStrandedSteerRedelivery, StrandedSteer,
};
use ironclaw_threads::{
    MessageStatus, SessionThreadError, SessionThreadService, ThreadMessageId, ThreadScope,
};
use ironclaw_turns::{
    AcceptedMessageRef, GetRunStateRequest, IdempotencyKey, LoopMessageRef, SubmitTurnRequest,
    SubmitTurnResponse, TurnCoordinator, TurnError, TurnRunId, TurnRunState, TurnScope, TurnStatus,
};

use crate::approval_interaction::{
    ApprovalInteractionDecision, ApprovalInteractionService, ResolveApprovalInteractionRequest,
};

/// Feedback recorded on an approval declined because the person sent a
/// message instead. The message itself follows as a steer, so the feedback
/// only links the two — it never repeats the text.
const STEER_DECLINE_FEEDBACK: &str =
    "Declined: the person sent a new message instead of answering. Read it next.";

/// One busy-thread message targeting one active run.
pub(crate) struct SteeringAdmissionRequest {
    pub turn_scope: TurnScope,
    pub thread_scope: ThreadScope,
    pub message_id: ThreadMessageId,
    pub accepted_message_ref: AcceptedMessageRef,
    pub active_run_id: TurnRunId,
}

impl SteeringAdmissionRequest {
    /// The thread identity lives on the turn scope; re-passing it separately
    /// would be re-derived identity the compiler cannot keep in agreement.
    fn thread_id(&self) -> &ThreadId {
        &self.turn_scope.thread_id
    }
}

/// Classified admission outcome. Callers translate these into their surface's
/// response types; every settle decision has already been applied here.
pub(crate) enum SteeringAdmission {
    /// The row is `Queued` and the input is (idempotently) enqueued for the
    /// active run, whose state snapshot rides along so callers never need a
    /// second `get_run_state` to build their response.
    Deferred { run: Box<TurnRunState> },
    /// Steering cannot serve this message — the queue is disabled or at
    /// capacity, the run's profile disallows steering, or the run is not in
    /// the sender's scope — and the row has been settled `RejectedBusy`.
    Rejected,
    /// The run is ending (terminal, cancel requested, or its queue already
    /// closed) and will not read another steer. Nothing is settled: the
    /// caller submits the message again once the run has released the
    /// thread, so it starts the next turn instead of being lost.
    RunEnding { run: Box<TurnRunState> },
}

/// Genuinely fatal admission failures. Settle-cases are never errors: they
/// come back as [`SteeringAdmission::Rejected`], so this enum cannot express
/// "should have been a rejection" and callers need no unreachable arms.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SteeringAdmissionError {
    #[error("invalid steering message ref: {0}")]
    InvalidMessageRef(String),
    #[error("active run state read failed: {0}")]
    RunState(#[source] TurnError),
    #[error("failed to mark message queued: {0}")]
    MarkQueued(#[source] SessionThreadError),
    #[error("failed to settle message as rejected-busy: {0}")]
    SettleRejected(#[source] SessionThreadError),
    #[error("steering enqueue failed: {0}")]
    Enqueue(#[source] HostInputQueueError),
}

/// Parse a stored `turn_run_id` column back into a [`TurnRunId`]. The single
/// spelling of this reconstruction; callers map the error string into their
/// surface's error type.
pub(crate) fn parse_stored_run_id(raw: Option<&str>) -> Result<TurnRunId, String> {
    let raw = raw.ok_or_else(|| "queued replay missing turn_run_id".to_string())?;
    uuid::Uuid::parse_str(raw)
        .map(TurnRunId::from_uuid)
        .map_err(|error| format!("invalid queued turn_run_id: {error}"))
}

/// Fresh busy admission (the `ThreadBusy` submit branches): classify the run,
/// mark the row `Queued` BEFORE the input becomes drainable (so the loop's
/// consumer always observes a deterministic `Queued` → `Submitted`
/// transition), then enqueue. A failed enqueue rolls the row back to
/// `RejectedBusy` so it never sticks in `Queued` with no backing input.
pub(crate) async fn admit_busy_steering<C>(
    turn_coordinator: &C,
    input_enqueue: &dyn HostInputEnqueuePort,
    thread_service: &dyn SessionThreadService,
    approvals: Option<&dyn ApprovalInteractionService>,
    request: SteeringAdmissionRequest,
) -> Result<SteeringAdmission, SteeringAdmissionError>
where
    C: TurnCoordinator + ?Sized,
{
    let run = match serviceable_run(turn_coordinator, &request).await? {
        Serviceable::Run(run) => *run,
        Serviceable::Ending(run) => return Ok(SteeringAdmission::RunEnding { run }),
        Serviceable::No => {
            settle_rejected(thread_service, &request).await?;
            return Ok(SteeringAdmission::Rejected);
        }
    };
    match mark_queued_reconciling(thread_service, &request).await? {
        QueuedMark::Queued => {}
        // A concurrent duplicate lost the race (or a reconciler already
        // flipped the row): it is settled — do not queue a contradicting
        // input on top of it.
        QueuedMark::SettledRejected => return Ok(SteeringAdmission::Rejected),
    }
    match try_enqueue(input_enqueue, &request, &run).await {
        Ok(()) => {
            let run = decline_pending_approval(turn_coordinator, approvals, &request, run).await;
            Ok(SteeringAdmission::Deferred { run: Box::new(run) })
        }
        Err(EnqueueFailure::RunClosed) => Ok(SteeringAdmission::RunEnding { run: Box::new(run) }),
        Err(EnqueueFailure::Unserviceable) => {
            settle_rejected(thread_service, &request).await?;
            Ok(SteeringAdmission::Rejected)
        }
        Err(EnqueueFailure::Fatal(error)) => {
            // Best-effort rollback: the `Queued` row has no backing input.
            if let Err(rollback) = thread_service
                .mark_message_rejected_busy(
                    &request.thread_scope,
                    request.thread_id(),
                    request.message_id,
                )
                .await
            {
                tracing::debug!(
                    %rollback,
                    thread_id = %request.thread_id(),
                    message_id = %request.message_id,
                    active_run_id = %request.active_run_id,
                    "failed to roll back queued message to rejected-busy after steering enqueue failure"
                );
            }
            Err(error)
        }
    }
}

/// A message sent while the run waits on an approval answers it: the steer
/// is already queued, so decline the approval and the run continues with the
/// message at its next step (t3code: "a message sent while approvals are
/// pending redirects the agent"). Best-effort — when the decline fails (the
/// gate was answered meanwhile, or no approval service is wired) the steer
/// stays queued and is read once the run moves on. Returns the run's state
/// after the decline, so the caller reports the running run.
async fn decline_pending_approval<C>(
    turn_coordinator: &C,
    approvals: Option<&dyn ApprovalInteractionService>,
    request: &SteeringAdmissionRequest,
    run: TurnRunState,
) -> TurnRunState
where
    C: TurnCoordinator + ?Sized,
{
    let (Some(approvals), TurnStatus::BlockedApproval, Some(gate_ref), Some(actor)) = (
        approvals,
        run.status,
        run.gate_ref.clone(),
        run.actor.clone(),
    ) else {
        return run;
    };
    let feedback = match ironclaw_host_api::turn::DenialFeedback::new(STEER_DECLINE_FEEDBACK) {
        Ok(feedback) => Some(feedback),
        Err(error) => {
            tracing::debug!(%error, "steer decline feedback rejected; declining without it");
            None
        }
    };
    let idempotency_key = match IdempotencyKey::new(format!("steer-decline:{}", request.message_id))
    {
        Ok(key) => key,
        Err(error) => {
            tracing::debug!(%error, "steer decline idempotency key rejected");
            return run;
        }
    };
    if let Err(error) = approvals
        .resolve(ResolveApprovalInteractionRequest {
            scope: request.turn_scope.clone(),
            actor,
            run_id_hint: Some(request.active_run_id),
            gate_ref,
            decision: ApprovalInteractionDecision::Deny { feedback },
            idempotency_key,
        })
        .await
    {
        tracing::debug!(
            active_run_id = %request.active_run_id,
            ?error,
            "declining the pending approval for a steer failed; the steer stays queued"
        );
        return run;
    }
    match turn_coordinator
        .get_run_state(GetRunStateRequest {
            scope: request.turn_scope.clone(),
            run_id: request.active_run_id,
        })
        .await
    {
        Ok(after) => after,
        Err(error) => {
            tracing::debug!(%error, "run state read after a steer decline failed");
            run
        }
    }
}

/// Replay re-admission for an already-`Queued` row. No mark step (the row is
/// already `Queued`) and no rollback on a fatal enqueue failure — the row
/// stays `Queued` so the NEXT replay retries the re-enqueue.
pub(crate) async fn readmit_queued_steering<C>(
    turn_coordinator: &C,
    input_enqueue: &dyn HostInputEnqueuePort,
    thread_service: &dyn SessionThreadService,
    request: SteeringAdmissionRequest,
) -> Result<SteeringAdmission, SteeringAdmissionError>
where
    C: TurnCoordinator + ?Sized,
{
    // A replayed row is one whose enqueue may never have happened (a crash
    // between the `Queued` mark and the enqueue), so terminal reconciliation
    // may not know it: when its run is ending, settle it here rather than
    // leave it `Queued` with nothing left to settle it. (Crash-orphan corner
    // case; a fresh submit resubmits as the next turn instead.)
    let run = match serviceable_run(turn_coordinator, &request).await? {
        Serviceable::Run(run) => *run,
        Serviceable::Ending(_) | Serviceable::No => {
            settle_rejected(thread_service, &request).await?;
            return Ok(SteeringAdmission::Rejected);
        }
    };
    match try_enqueue(input_enqueue, &request, &run).await {
        Ok(()) => Ok(SteeringAdmission::Deferred { run: Box::new(run) }),
        Err(EnqueueFailure::RunClosed) => {
            settle_rejected(thread_service, &request).await?;
            Ok(SteeringAdmission::Rejected)
        }
        Err(EnqueueFailure::Unserviceable) => {
            settle_rejected(thread_service, &request).await?;
            Ok(SteeringAdmission::Rejected)
        }
        Err(EnqueueFailure::Fatal(error)) => Err(error),
    }
}

enum Serviceable {
    Run(Box<TurnRunState>),
    /// Terminal or cancel requested: it will not read another steer, but it
    /// is about to release the thread.
    Ending(Box<TurnRunState>),
    No,
}

/// Whether the active run can still consume steering input. Terminal and
/// `CancelRequested` runs are [`Serviceable::Ending`]: a cancelling run's
/// next drain (if any) exists only to wind down, so input queued behind it
/// would race the cancel reconciler — the caller resubmits once the thread
/// is released instead. A run outside the sender's scope or whose profile
/// disallows steering is [`Serviceable::No`].
async fn serviceable_run<C>(
    turn_coordinator: &C,
    request: &SteeringAdmissionRequest,
) -> Result<Serviceable, SteeringAdmissionError>
where
    C: TurnCoordinator + ?Sized,
{
    let run = match turn_coordinator
        .get_run_state(GetRunStateRequest {
            scope: request.turn_scope.clone(),
            run_id: request.active_run_id,
        })
        .await
    {
        Ok(run) => run,
        Err(TurnError::ScopeNotFound) => {
            tracing::debug!(
                active_run_id = %request.active_run_id,
                "steering refused: active run not found in the sender's scope"
            );
            return Ok(Serviceable::No);
        }
        Err(error) => return Err(SteeringAdmissionError::RunState(error)),
    };
    if run.status.is_terminal() || matches!(run.status, TurnStatus::CancelRequested) {
        tracing::debug!(
            active_run_id = %request.active_run_id,
            status = ?run.status,
            "steering deferred: the active run is ending"
        );
        return Ok(Serviceable::Ending(Box::new(run)));
    }
    if !run.allow_steering {
        tracing::debug!(
            active_run_id = %request.active_run_id,
            "steering refused: the run's profile disallows steering"
        );
        return Ok(Serviceable::No);
    }
    Ok(Serviceable::Run(Box::new(run)))
}

enum QueuedMark {
    Queued,
    SettledRejected,
}

/// Mark the row `Queued`, reconciling a failed transition against what it
/// actually raced with via a point read. Lenient ONLY for the confirmed
/// races: the row is already `Queued`/`Submitted` for this run (a duplicate
/// submit or a fast loop won) or already settled `RejectedBusy`. Every other
/// failure propagates — swallowing it would enqueue a drainable input whose
/// row silently stays `Accepted` (fail-loud, `.claude/rules/error-handling.md`).
async fn mark_queued_reconciling(
    thread_service: &dyn SessionThreadService,
    request: &SteeringAdmissionRequest,
) -> Result<QueuedMark, SteeringAdmissionError> {
    let run_id = request.active_run_id.to_string();
    let original_error = match thread_service
        .mark_message_queued(
            &request.thread_scope,
            request.thread_id(),
            request.message_id,
            run_id.clone(),
        )
        .await
    {
        Ok(_) => return Ok(QueuedMark::Queued),
        Err(error) => error,
    };
    let row = match thread_service
        .read_thread_message(
            &request.thread_scope,
            request.thread_id(),
            request.message_id,
        )
        .await
    {
        Ok(row) => row,
        Err(read_error) => {
            tracing::debug!(
                thread_id = %request.thread_id(),
                message_id = %request.message_id,
                active_run_id = %request.active_run_id,
                %original_error,
                %read_error,
                "queued-status reconciliation read failed after mark_message_queued error"
            );
            return Err(SteeringAdmissionError::MarkQueued(original_error));
        }
    };
    match row {
        Some(row)
            if matches!(row.status, MessageStatus::Queued | MessageStatus::Submitted)
                && row.turn_run_id.as_deref() == Some(run_id.as_str()) =>
        {
            Ok(QueuedMark::Queued)
        }
        Some(row) if row.status == MessageStatus::RejectedBusy => Ok(QueuedMark::SettledRejected),
        _ => {
            tracing::debug!(
                thread_id = %request.thread_id(),
                message_id = %request.message_id,
                active_run_id = %request.active_run_id,
                %original_error,
                "queued-status transition failed and the row was not consumed by the run"
            );
            Err(SteeringAdmissionError::MarkQueued(original_error))
        }
    }
}

pub(crate) async fn settle_rejected(
    thread_service: &dyn SessionThreadService,
    request: &SteeringAdmissionRequest,
) -> Result<(), SteeringAdmissionError> {
    thread_service
        .mark_message_rejected_busy(
            &request.thread_scope,
            request.thread_id(),
            request.message_id,
        )
        .await
        .map(|_| ())
        .map_err(SteeringAdmissionError::SettleRejected)
}

enum EnqueueFailure {
    /// The run's queue was closed by terminal reconciliation: the run is
    /// ending and the caller resubmits the message as the next turn.
    RunClosed,
    /// The queue refused the message in a way that settles it as
    /// rejected-busy: steering is deliberately not wired for this runtime,
    /// or the run's queue is at capacity. Never an operational failure —
    /// those stay `Fatal` so they surface as retryable errors.
    Unserviceable,
    Fatal(SteeringAdmissionError),
}

async fn try_enqueue(
    input_enqueue: &dyn HostInputEnqueuePort,
    request: &SteeringAdmissionRequest,
    run: &TurnRunState,
) -> Result<(), EnqueueFailure> {
    let message_ref = LoopMessageRef::new(request.accepted_message_ref.as_str().to_string())
        .map_err(|error| {
            EnqueueFailure::Fatal(SteeringAdmissionError::InvalidMessageRef(error.to_string()))
        })?;
    match input_enqueue
        .enqueue_user_steer(
            EnqueueQueuedMessageRequest {
                run_id: request.active_run_id,
                turn_id: run.turn_id,
                scope: request.thread_scope.clone(),
                thread_id: request.thread_id().clone(),
                message_id: request.message_id,
                input: LoopInput::Steering { message_ref },
                ack_effect: None,
            },
            request.turn_scope.clone(),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(HostInputQueueError::RunClosed) => {
            tracing::debug!(
                active_run_id = %request.active_run_id,
                "steering deferred: the run's input queue is already closed"
            );
            Err(EnqueueFailure::RunClosed)
        }
        Err(error @ (HostInputQueueError::Disabled | HostInputQueueError::CapacityExhausted)) => {
            tracing::debug!(
                active_run_id = %request.active_run_id,
                %error,
                "steering refused: the run's input queue did not take the message"
            );
            Err(EnqueueFailure::Unserviceable)
        }
        Err(error) => Err(EnqueueFailure::Fatal(SteeringAdmissionError::Enqueue(
            error,
        ))),
    }
}

/// Starts the next turn with steers an ended run never read
/// ([`HostStrandedSteerRedelivery`], bound on the input queue by
/// composition). The first steer submits a turn carrying the same accepted
/// message (its row moves `Queued` → `Submitted` on the new run, no
/// duplicate row); each later one steers that new run through
/// [`admit_busy_steering`], so several keep their order. The submit is keyed
/// by (ended run, message), so a repeated reconciliation never starts it
/// twice.
pub struct SteeringRedelivery {
    turn_coordinator: Arc<dyn TurnCoordinator>,
    thread_service: Arc<dyn SessionThreadService>,
    input_enqueue: Arc<dyn HostInputEnqueuePort>,
}

impl SteeringRedelivery {
    pub fn new(
        turn_coordinator: Arc<dyn TurnCoordinator>,
        thread_service: Arc<dyn SessionThreadService>,
        input_enqueue: Arc<dyn HostInputEnqueuePort>,
    ) -> Self {
        Self {
            turn_coordinator,
            thread_service,
            input_enqueue,
        }
    }

    async fn redeliver_one(
        &self,
        ended_run_id: TurnRunId,
        steer: &StrandedSteer,
    ) -> Result<(), String> {
        let ended = self
            .turn_coordinator
            .get_run_state(GetRunStateRequest {
                scope: steer.turn_scope.clone(),
                run_id: ended_run_id,
            })
            .await
            .map_err(|error| format!("ended run state read failed: {error}"))?;
        let Some(actor) = ended.actor.clone() else {
            // No person to run the turn as (a system-activated run): the row
            // can only be settled as not delivered.
            return self.settle_not_delivered(steer).await;
        };
        let accepted_message_ref = AcceptedMessageRef::new(steer.message_ref.as_str())
            .map_err(|error| format!("stranded steer ref invalid: {error}"))?;
        let idempotency_key = IdempotencyKey::new(format!(
            "steer-redelivery:{ended_run_id}:{}",
            steer.message_id
        ))
        .map_err(|error| format!("redelivery idempotency key invalid: {error}"))?;
        // The ended run's model carries over; an unresolvable pick falls
        // back to the active model, as any submit's would.
        let requested_model = ended.resolved_model_route.as_ref().and_then(|route| {
            if route.is_advisory() {
                Some(route.model_id().to_string())
            } else {
                ironclaw_product_contracts::operator_llm::NativeModelRoute::turn_model(
                    Some(route.provider_id()),
                    Some(route.model_id()),
                    None,
                )
            }
        });
        let submitted = self
            .turn_coordinator
            .submit_turn(SubmitTurnRequest {
                scope: steer.turn_scope.clone(),
                actor,
                accepted_message_ref: accepted_message_ref.clone(),
                requested_run_profile: None,
                output_contract: None,
                requested_model,
                idempotency_key,
                received_at: chrono::Utc::now(),
                requested_run_id: None,
                parent_run_id: None,
                subagent_depth: 0,
                spawn_tree_root_run_id: None,
                product_context: ended.product_context.clone(),
                subagent_activation_provenance: None,
            })
            .await;
        match submitted {
            Ok(SubmitTurnResponse::Accepted {
                turn_id, run_id, ..
            }) => self
                .thread_service
                .mark_message_submitted(
                    &steer.thread_scope,
                    &steer.thread_id,
                    steer.message_id,
                    turn_id.to_string(),
                    run_id.to_string(),
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("redelivered row submit flip failed: {error}")),
            Err(TurnError::ThreadBusy(busy)) if busy.active_run_id == ended_run_id => {
                // The ended run has not released the thread yet; a later
                // reconciliation of it retries.
                Err("the ended run still holds the thread".to_string())
            }
            Err(TurnError::ThreadBusy(busy)) => {
                // Another run started first (an earlier redelivered steer, or
                // a new message): steer it.
                match admit_busy_steering(
                    self.turn_coordinator.as_ref(),
                    self.input_enqueue.as_ref(),
                    self.thread_service.as_ref(),
                    None,
                    SteeringAdmissionRequest {
                        turn_scope: steer.turn_scope.clone(),
                        thread_scope: steer.thread_scope.clone(),
                        message_id: steer.message_id,
                        accepted_message_ref,
                        active_run_id: busy.active_run_id,
                    },
                )
                .await
                {
                    Ok(SteeringAdmission::Deferred { .. } | SteeringAdmission::Rejected) => Ok(()),
                    Ok(SteeringAdmission::RunEnding { .. }) => {
                        Err("the run it would steer is ending".to_string())
                    }
                    Err(error) => Err(format!("steering the started run failed: {error}")),
                }
            }
            Err(error) => Err(format!("redelivery submit failed: {error}")),
        }
    }

    async fn settle_not_delivered(&self, steer: &StrandedSteer) -> Result<(), String> {
        self.thread_service
            .mark_message_rejected_busy(&steer.thread_scope, &steer.thread_id, steer.message_id)
            .await
            .map(|_| ())
            .map_err(|error| format!("not-delivered settle failed: {error}"))
    }
}

#[async_trait]
impl HostStrandedSteerRedelivery for SteeringRedelivery {
    async fn redeliver(
        &self,
        ended_run_id: TurnRunId,
        steers: Vec<StrandedSteer>,
    ) -> Vec<ThreadMessageId> {
        let mut settled = Vec::new();
        for steer in &steers {
            match self.redeliver_one(ended_run_id, steer).await {
                Ok(()) => settled.push(steer.message_id),
                Err(reason) => {
                    tracing::debug!(
                        %ended_run_id,
                        message_id = %steer.message_id,
                        %reason,
                        "unread steer not redelivered yet; retried by the next reconciliation"
                    );
                    // Later steers must not overtake this one.
                    break;
                }
            }
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ironclaw_host_api::ids::{AgentId, TenantId, UserId};
    use ironclaw_loop_host::HostInputEnvelope;
    use ironclaw_threads::{
        AcceptInboundMessageRequest, EnsureThreadRequest, InMemorySessionThreadService,
        MessageContent, ThreadHistoryRequest,
    };
    use ironclaw_turns::{
        CancelRunRequest, CancelRunResponse, GetRunStateRequest, ResumeTurnRequest,
        ResumeTurnResponse, RetryTurnRequest, RetryTurnResponse, SubmitTurnRequest,
        SubmitTurnResponse, TurnStatus,
    };

    use super::*;

    /// Coordinator double returning one scripted run state; the admission
    /// path only ever calls `get_run_state`.
    struct ScriptedRunStateCoordinator {
        state: TurnRunState,
    }

    #[async_trait]
    impl TurnCoordinator for ScriptedRunStateCoordinator {
        async fn prepare_turn(&self, _scope: TurnScope) -> Result<TurnRunId, TurnError> {
            panic!("prepare_turn is not used by steering admission")
        }

        async fn submit_turn(
            &self,
            _request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResponse, TurnError> {
            panic!("submit_turn is not used by steering admission")
        }

        async fn resume_turn(
            &self,
            _request: ResumeTurnRequest,
        ) -> Result<ResumeTurnResponse, TurnError> {
            panic!("resume_turn is not used by steering admission")
        }

        async fn retry_turn(
            &self,
            _request: RetryTurnRequest,
        ) -> Result<RetryTurnResponse, TurnError> {
            panic!("retry_turn is not used by steering admission")
        }

        async fn cancel_run(
            &self,
            _request: CancelRunRequest,
        ) -> Result<CancelRunResponse, TurnError> {
            panic!("cancel_run is not used by steering admission")
        }

        async fn get_run_state(
            &self,
            _request: GetRunStateRequest,
        ) -> Result<TurnRunState, TurnError> {
            Ok(self.state.clone())
        }
    }

    /// Enqueue double that must never be reached by an unserviceable run.
    #[derive(Default)]
    struct CountingEnqueue {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl HostInputEnqueuePort for CountingEnqueue {
        async fn enqueue_queued_message(
            &self,
            _request: EnqueueQueuedMessageRequest,
        ) -> Result<HostInputEnvelope, HostInputQueueError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(HostInputQueueError::Disabled)
        }
    }

    #[tokio::test]
    async fn cancel_requested_run_is_ending_and_leaves_the_message_for_the_next_turn() {
        // `CancelRequested` is nonterminal but the cancel reconciler will
        // close its queue: admitting steering behind it would strand the
        // message (or race the reconciler). Admission reports the run as
        // ending WITHOUT settling the row, so the caller resubmits it as the
        // next turn once the thread is released (never "please resend").
        let thread_service = InMemorySessionThreadService::default();
        let thread_scope = ThreadScope {
            tenant_id: TenantId::new("tenant-steer").expect("tenant"),
            agent_id: AgentId::new("agent-steer").expect("agent"),
            project_id: None,
            owner_user_id: None,
            mission_id: None,
        };
        let thread = thread_service
            .ensure_thread(EnsureThreadRequest {
                scope: thread_scope.clone(),
                thread_id: None,
                created_by_actor_id: "actor-steer".into(),
                title: None,
                metadata_json: None,
            })
            .await
            .expect("thread");
        let accepted = thread_service
            .accept_inbound_message(AcceptInboundMessageRequest {
                scope: thread_scope.clone(),
                thread_id: thread.thread_id.clone(),
                actor_id: "actor-steer".into(),
                source_binding_id: None,
                reply_target_binding_id: None,
                external_event_id: None,
                content: MessageContent::text("too late to steer"),
            })
            .await
            .expect("accept");

        let run_id = TurnRunId::new();
        let turn_scope = TurnScope::new(
            thread_scope.tenant_id.clone(),
            Some(thread_scope.agent_id.clone()),
            None,
            thread.thread_id.clone(),
        );
        let accepted_message_ref =
            AcceptedMessageRef::new(format!("msg:{}", accepted.message_id)).expect("message ref");
        let mut state = sample_run_state(turn_scope.clone(), run_id, &accepted_message_ref);
        state.status = TurnStatus::CancelRequested;
        let coordinator = ScriptedRunStateCoordinator { state };
        let enqueue = CountingEnqueue::default();

        let admission = admit_busy_steering(
            &coordinator,
            &enqueue,
            &thread_service,
            None,
            SteeringAdmissionRequest {
                turn_scope,
                thread_scope: thread_scope.clone(),
                message_id: accepted.message_id,
                accepted_message_ref,
                active_run_id: run_id,
            },
        )
        .await
        .expect("admission settles");

        assert!(matches!(admission, SteeringAdmission::RunEnding { .. }));
        assert_eq!(
            enqueue.calls.load(Ordering::SeqCst),
            0,
            "no steering input may be enqueued behind a cancel-requested run"
        );
        let history = thread_service
            .list_thread_history(ThreadHistoryRequest {
                scope: thread_scope,
                thread_id: thread.thread_id,
            })
            .await
            .expect("history");
        assert_eq!(
            history.messages[0].status,
            MessageStatus::Accepted,
            "the row is left for the resubmit, not settled as not delivered"
        );
    }

    /// Coordinator double for redelivery: the ended run's state answers
    /// `get_run_state` for it; the first submit starts `next_run`, later ones
    /// find the thread busy with it. Every submit request is recorded.
    struct RedeliveryCoordinator {
        ended: TurnRunState,
        next: TurnRunState,
        submits: std::sync::Mutex<Vec<SubmitTurnRequest>>,
    }

    #[async_trait]
    impl TurnCoordinator for RedeliveryCoordinator {
        async fn prepare_turn(&self, _scope: TurnScope) -> Result<TurnRunId, TurnError> {
            panic!("prepare_turn is not used by redelivery")
        }

        async fn submit_turn(
            &self,
            request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResponse, TurnError> {
            let mut submits = self.submits.lock().expect("submits");
            submits.push(request.clone());
            if submits.len() == 1 {
                Ok(SubmitTurnResponse::Accepted {
                    turn_id: self.next.turn_id,
                    run_id: self.next.run_id,
                    status: TurnStatus::Queued,
                    resolved_run_profile_id: self.next.resolved_run_profile_id.clone(),
                    resolved_run_profile_version: self.next.resolved_run_profile_version,
                    event_cursor: self.next.event_cursor,
                    accepted_message_ref: request.accepted_message_ref,
                })
            } else {
                Err(TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
                    active_run_id: self.next.run_id,
                    status: TurnStatus::Running,
                    event_cursor: self.next.event_cursor,
                }))
            }
        }

        async fn resume_turn(
            &self,
            _request: ResumeTurnRequest,
        ) -> Result<ResumeTurnResponse, TurnError> {
            panic!("resume_turn is not used by redelivery")
        }

        async fn retry_turn(
            &self,
            _request: RetryTurnRequest,
        ) -> Result<RetryTurnResponse, TurnError> {
            panic!("retry_turn is not used by redelivery")
        }

        async fn cancel_run(
            &self,
            _request: CancelRunRequest,
        ) -> Result<CancelRunResponse, TurnError> {
            panic!("cancel_run is not used by redelivery")
        }

        async fn get_run_state(
            &self,
            request: GetRunStateRequest,
        ) -> Result<TurnRunState, TurnError> {
            if request.run_id == self.ended.run_id {
                Ok(self.ended.clone())
            } else if request.run_id == self.next.run_id {
                Ok(self.next.clone())
            } else {
                Err(TurnError::ScopeNotFound)
            }
        }
    }

    /// Steers a run never read when it ended by itself start the next turn:
    /// the first submits a turn carrying the SAME accepted message (as the
    /// ended run's actor, with its product context, keyed so a repeated
    /// reconciliation cannot start it twice) and its row moves to the new
    /// run; the second steers that new run, keeping their order. Nothing is
    /// settled `RejectedBusy` and nobody is asked to resend.
    #[tokio::test]
    async fn unread_steers_of_an_ended_run_start_the_next_turn_in_order() {
        use ironclaw_loop_contracts::LoopInputCursorToken;
        use ironclaw_loop_host::{
            HostInputQueue, HostInputQueueReconcile, InMemoryHostInputQueue, RunQueueEnding,
        };

        let thread_service = Arc::new(InMemorySessionThreadService::default());
        let thread_scope = ThreadScope {
            tenant_id: TenantId::new("tenant-redeliver").expect("tenant"),
            agent_id: AgentId::new("agent-redeliver").expect("agent"),
            project_id: None,
            owner_user_id: None,
            mission_id: None,
        };
        let thread = thread_service
            .ensure_thread(EnsureThreadRequest {
                scope: thread_scope.clone(),
                thread_id: None,
                created_by_actor_id: "actor-redeliver".into(),
                title: None,
                metadata_json: None,
            })
            .await
            .expect("thread");
        let turn_scope = TurnScope::new(
            thread_scope.tenant_id.clone(),
            Some(thread_scope.agent_id.clone()),
            None,
            thread.thread_id.clone(),
        );
        let ended_run = TurnRunId::new();
        let next_run = TurnRunId::new();
        let first_ref = AcceptedMessageRef::new("msg:placeholder").expect("ref");
        let mut ended = sample_run_state(turn_scope.clone(), ended_run, &first_ref);
        ended.status = TurnStatus::Completed;
        ended.product_context = Some(ironclaw_turns::product_context::resolve_web_ui(
            turn_scope.product_owner(ended.actor.as_ref().expect("actor")),
        ));
        let next = sample_run_state(turn_scope.clone(), next_run, &first_ref);
        let coordinator = Arc::new(RedeliveryCoordinator {
            ended: ended.clone(),
            next,
            submits: std::sync::Mutex::new(Vec::new()),
        });
        let queue = Arc::new(InMemoryHostInputQueue::new(
            Arc::clone(&thread_service) as Arc<dyn SessionThreadService>
        ));
        queue
            .bind_steer_redelivery(Arc::new(SteeringRedelivery::new(
                Arc::clone(&coordinator) as Arc<dyn TurnCoordinator>,
                Arc::clone(&thread_service) as Arc<dyn SessionThreadService>,
                Arc::clone(&queue) as Arc<dyn HostInputEnqueuePort>,
            )))
            .expect("bind");

        let mut ids = Vec::new();
        for text in ["first unread steer", "second unread steer"] {
            let accepted = thread_service
                .accept_inbound_message(AcceptInboundMessageRequest {
                    scope: thread_scope.clone(),
                    thread_id: thread.thread_id.clone(),
                    actor_id: "actor-redeliver".into(),
                    source_binding_id: None,
                    reply_target_binding_id: None,
                    external_event_id: None,
                    content: MessageContent::text(text),
                })
                .await
                .expect("accept");
            thread_service
                .mark_message_queued(
                    &thread_scope,
                    &thread.thread_id,
                    accepted.message_id,
                    ended_run.to_string(),
                )
                .await
                .expect("queued");
            queue
                .enqueue_user_steer(
                    EnqueueQueuedMessageRequest {
                        run_id: ended_run,
                        turn_id: ended.turn_id,
                        scope: thread_scope.clone(),
                        thread_id: thread.thread_id.clone(),
                        message_id: accepted.message_id,
                        input: LoopInput::Steering {
                            message_ref: LoopMessageRef::new(format!(
                                "msg:{}",
                                accepted.message_id
                            ))
                            .expect("ref"),
                        },
                        ack_effect: None,
                    },
                    turn_scope.clone(),
                )
                .await
                .expect("enqueue");
            ids.push(accepted.message_id);
        }

        let rejected = queue
            .settle_unconsumed(ended_run, RunQueueEnding::Ended)
            .await
            .expect("settle");
        assert!(rejected.is_empty(), "nothing is settled as not delivered");

        let submits = coordinator.submits.lock().expect("submits").clone();
        assert_eq!(submits.len(), 2, "one start, one steer attempt");
        let start = &submits[0];
        assert_eq!(
            start.accepted_message_ref.as_str(),
            format!("msg:{}", ids[0]).as_str(),
            "the next turn carries the oldest unread steer's own message"
        );
        assert_eq!(Some(&start.actor), ended.actor.as_ref());
        assert_eq!(start.scope, turn_scope);
        assert_eq!(start.product_context, ended.product_context);
        assert_eq!(
            start.idempotency_key.as_str(),
            format!("steer-redelivery:{ended_run}:{}", ids[0]).as_str()
        );

        let row = |message_id| {
            let thread_service = Arc::clone(&thread_service);
            let thread_scope = thread_scope.clone();
            let thread_id = thread.thread_id.clone();
            async move {
                thread_service
                    .read_thread_message(&thread_scope, &thread_id, message_id)
                    .await
                    .expect("read")
                    .expect("row")
            }
        };
        let first = row(ids[0]).await;
        assert_eq!(first.status, MessageStatus::Submitted);
        assert_eq!(
            first.turn_run_id.as_deref(),
            Some(next_run.to_string().as_str())
        );
        let second = row(ids[1]).await;
        assert_eq!(second.status, MessageStatus::Queued);
        assert_eq!(
            second.turn_run_id.as_deref(),
            Some(next_run.to_string().as_str()),
            "the later steer steers the started run"
        );
        let drained = queue
            .next_after(next_run, LoopInputCursorToken::origin(), 8)
            .await
            .expect("poll next run");
        assert_eq!(
            drained.inputs.len(),
            1,
            "the started run reads the second steer"
        );

        // A repeated reconciliation of the ended run does nothing more.
        queue
            .settle_unconsumed(ended_run, RunQueueEnding::Ended)
            .await
            .expect("idempotent");
        assert_eq!(coordinator.submits.lock().expect("submits").len(), 2);
    }

    fn sample_run_state(
        scope: TurnScope,
        run_id: TurnRunId,
        accepted_message_ref: &AcceptedMessageRef,
    ) -> TurnRunState {
        use ironclaw_host_api::turn::{EventCursor, RunProfileVersion, TurnActor, TurnId};
        use ironclaw_turns::RunProfileId;
        TurnRunState {
            scope,
            actor: Some(TurnActor::new(UserId::new("user-steer").expect("user"))),
            turn_id: TurnId::new(),
            run_id,
            status: TurnStatus::Running,
            accepted_message_ref: accepted_message_ref.clone(),
            resolved_run_profile_id: RunProfileId::default_profile(),
            resolved_run_profile_version: RunProfileVersion::new(1),
            output_contract: Default::default(),
            allow_steering: true,
            resolved_model_route: None,
            model_usage: None,
            execution_outcome: None,
            received_at: chrono::Utc::now(),
            checkpoint_id: None,
            gate_ref: None,
            blocked_activity_id: None,
            credential_requirements: Vec::new(),
            failure: None,
            event_cursor: EventCursor::default(),
            product_context: None,
            resume_disposition: None,
        }
    }
}
