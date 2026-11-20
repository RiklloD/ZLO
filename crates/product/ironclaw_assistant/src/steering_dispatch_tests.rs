//! Schedules forced against the thread's steer dispatch decision. Real
//! in-memory queue, thread store, and handoff store; a scripted coordinator
//! whose thread-busy state and run states the test drives.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use ironclaw_host_api::ids::{AgentId, TenantId, UserId};
use ironclaw_loop_contracts::LoopInputCursorToken;
use ironclaw_loop_host::{
    HostInputQueue, HostInputQueueReconcile, InMemoryHostInputQueue, InMemorySteerHandoffStore,
    RunQueueEnding,
};
use ironclaw_threads::{
    AcceptInboundMessageRequest, EnsureThreadRequest, InMemorySessionThreadService, MessageContent,
};
use ironclaw_turns::{
    CancelRunRequest, CancelRunResponse, ResumeTurnRequest, ResumeTurnResponse, RetryTurnRequest,
    RetryTurnResponse, ThreadBusy,
};

use super::*;
use crate::steering::{SteeringAdmission, SteeringAdmissionPorts, SteeringAdmissionRequest};

/// Scripted coordinator: one thread, at most one active run. `submit_turn`
/// starts the requested run unless another is active (`ThreadBusy`), and
/// replays a seen idempotency key. `get_run_state` answers each run's state,
/// optionally from a scripted sequence (to force a status change between two
/// reads of the same admission).
#[derive(Default)]
struct Coordinator {
    runs: Mutex<HashMap<TurnRunId, TurnRunState>>,
    scripted: Mutex<HashMap<TurnRunId, VecDeque<TurnStatus>>>,
    active: Mutex<Option<TurnRunId>>,
    keys: Mutex<HashMap<String, TurnRunId>>,
    submits: Mutex<Vec<SubmitTurnRequest>>,
}

impl Coordinator {
    fn put(&self, state: TurnRunState) {
        self.runs.lock().unwrap().insert(state.run_id, state);
    }

    fn set_active(&self, run: Option<TurnRunId>) {
        *self.active.lock().unwrap() = run;
    }

    fn finish(&self, run: TurnRunId, status: TurnStatus) {
        if let Some(state) = self.runs.lock().unwrap().get_mut(&run) {
            state.status = status;
        }
        let mut active = self.active.lock().unwrap();
        if *active == Some(run) {
            *active = None;
        }
    }

    fn script(&self, run: TurnRunId, statuses: &[TurnStatus]) {
        self.scripted
            .lock()
            .unwrap()
            .insert(run, statuses.iter().copied().collect());
    }

    fn submits(&self) -> Vec<SubmitTurnRequest> {
        self.submits.lock().unwrap().clone()
    }
}

#[async_trait]
impl TurnCoordinator for Coordinator {
    async fn prepare_turn(&self, _scope: TurnScope) -> Result<TurnRunId, TurnError> {
        panic!("prepare_turn is not used")
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        if let Some(run_id) = self
            .keys
            .lock()
            .unwrap()
            .get(request.idempotency_key.as_str())
            .copied()
        {
            let state = self.runs.lock().unwrap()[&run_id].clone();
            return Ok(accepted(&state));
        }
        if let Some(active) = *self.active.lock().unwrap() {
            let status = self.runs.lock().unwrap()[&active].status;
            return Err(TurnError::ThreadBusy(ThreadBusy {
                active_run_id: active,
                status,
                event_cursor: Default::default(),
            }));
        }
        self.submits.lock().unwrap().push(request.clone());
        let run_id = request.requested_run_id.unwrap_or_default();
        let mut state = run_state(request.scope.clone(), run_id);
        state.actor = Some(request.actor.clone());
        self.put(state.clone());
        self.keys
            .lock()
            .unwrap()
            .insert(request.idempotency_key.as_str().to_string(), run_id);
        self.set_active(Some(run_id));
        Ok(accepted(&state))
    }

    async fn resume_turn(
        &self,
        _request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        panic!("resume_turn is not used")
    }

    async fn retry_turn(&self, _request: RetryTurnRequest) -> Result<RetryTurnResponse, TurnError> {
        panic!("retry_turn is not used")
    }

    async fn cancel_run(&self, _request: CancelRunRequest) -> Result<CancelRunResponse, TurnError> {
        panic!("cancel_run is not used")
    }

    async fn get_run_state(&self, request: GetRunStateRequest) -> Result<TurnRunState, TurnError> {
        let mut state = self
            .runs
            .lock()
            .unwrap()
            .get(&request.run_id)
            .cloned()
            .ok_or(TurnError::ScopeNotFound)?;
        if state.scope != request.scope {
            return Err(TurnError::ScopeNotFound);
        }
        if let Some(next) = self
            .scripted
            .lock()
            .unwrap()
            .get_mut(&request.run_id)
            .and_then(VecDeque::pop_front)
        {
            state.status = next;
        }
        Ok(state)
    }
}

fn accepted(state: &TurnRunState) -> SubmitTurnResponse {
    SubmitTurnResponse::Accepted {
        turn_id: state.turn_id,
        run_id: state.run_id,
        status: TurnStatus::Queued,
        resolved_run_profile_id: state.resolved_run_profile_id.clone(),
        resolved_run_profile_version: state.resolved_run_profile_version,
        event_cursor: state.event_cursor,
        accepted_message_ref: state.accepted_message_ref.clone(),
    }
}

fn run_state(scope: TurnScope, run_id: TurnRunId) -> TurnRunState {
    use ironclaw_host_api::turn::{RunProfileVersion, TurnActor, TurnId};
    TurnRunState {
        scope,
        actor: Some(TurnActor::new(UserId::new("user-dispatch").unwrap())),
        turn_id: TurnId::new(),
        run_id,
        status: TurnStatus::Running,
        accepted_message_ref: AcceptedMessageRef::new("msg:start").unwrap(),
        resolved_run_profile_id: ironclaw_turns::RunProfileId::default_profile(),
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
        event_cursor: Default::default(),
        product_context: None,
        resume_disposition: None,
    }
}

struct Fixture {
    coordinator: Arc<Coordinator>,
    threads: Arc<InMemorySessionThreadService>,
    queue: Arc<InMemoryHostInputQueue>,
    store: Arc<InMemorySteerHandoffStore>,
    dispatch: Arc<NativeSteerDispatch>,
    thread_scope: ThreadScope,
    thread_id: ThreadId,
    turn_scope: TurnScope,
}

impl Fixture {
    async fn new() -> Self {
        let coordinator = Arc::new(Coordinator::default());
        let threads = Arc::new(InMemorySessionThreadService::default());
        let store = Arc::new(InMemorySteerHandoffStore::new());
        let queue = Arc::new(
            InMemoryHostInputQueue::new(Arc::clone(&threads) as Arc<dyn SessionThreadService>)
                .with_steer_handoff(Arc::clone(&store) as Arc<dyn SteerHandoffStore>),
        );
        let dispatch = NativeSteerDispatch::new(
            Arc::clone(&coordinator) as Arc<dyn TurnCoordinator>,
            Arc::clone(&threads) as Arc<dyn SessionThreadService>,
            Arc::clone(&queue) as Arc<dyn HostInputEnqueuePort>,
            Arc::clone(&store) as Arc<dyn SteerHandoffStore>,
            None,
            None,
        );
        let thread_scope = ThreadScope {
            tenant_id: TenantId::new("tenant-dispatch").unwrap(),
            agent_id: AgentId::new("agent-dispatch").unwrap(),
            project_id: None,
            owner_user_id: Some(UserId::new("user-dispatch").unwrap()),
            mission_id: None,
        };
        let thread = threads
            .ensure_thread(EnsureThreadRequest {
                scope: thread_scope.clone(),
                thread_id: None,
                created_by_actor_id: "actor-dispatch".into(),
                title: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let turn_scope = TurnScope::new_with_owner(
            thread_scope.tenant_id.clone(),
            Some(thread_scope.agent_id.clone()),
            None,
            thread.thread_id.clone(),
            thread_scope.owner_user_id.clone(),
        );
        Self {
            coordinator,
            threads,
            queue,
            store,
            dispatch,
            thread_scope,
            thread_id: thread.thread_id,
            turn_scope,
        }
    }

    /// A run that is active on the thread.
    fn start_run(&self) -> TurnRunId {
        let run = TurnRunId::new();
        self.coordinator
            .put(run_state(self.turn_scope.clone(), run));
        self.coordinator.set_active(Some(run));
        run
    }

    async fn accept(&self, text: &str) -> ThreadMessageId {
        self.threads
            .accept_inbound_message(AcceptInboundMessageRequest {
                scope: self.thread_scope.clone(),
                thread_id: self.thread_id.clone(),
                actor_id: "actor-dispatch".into(),
                source_binding_id: None,
                reply_target_binding_id: None,
                external_event_id: None,
                content: MessageContent::text(text),
            })
            .await
            .unwrap()
            .message_id
    }

    /// A user steer queued for `run`, as admission leaves it.
    async fn steer_into(&self, run: TurnRunId, text: &str) -> ThreadMessageId {
        let message_id = self.accept(text).await;
        self.threads
            .mark_message_queued(
                &self.thread_scope,
                &self.thread_id,
                message_id,
                run.to_string(),
            )
            .await
            .unwrap();
        self.queue
            .enqueue_user_steer(
                EnqueueQueuedMessageRequest {
                    run_id: run,
                    turn_id: ironclaw_host_api::turn::TurnId::new(),
                    scope: self.thread_scope.clone(),
                    thread_id: self.thread_id.clone(),
                    message_id,
                    input: LoopInput::Steering {
                        message_ref: LoopMessageRef::new(format!("msg:{message_id}")).unwrap(),
                    },
                    ack_effect: None,
                },
                self.turn_scope.clone(),
            )
            .await
            .unwrap();
        message_id
    }

    async fn row(&self, message_id: ThreadMessageId) -> ThreadMessageRecord {
        self.threads
            .read_thread_message(&self.thread_scope, &self.thread_id, message_id)
            .await
            .unwrap()
            .unwrap()
    }

    /// The loop of `run` drains and reads everything queued for it.
    async fn consume_all(&self, run: TurnRunId) {
        let batch = self
            .queue
            .next_after(run, LoopInputCursorToken::origin(), 8)
            .await
            .unwrap();
        let tokens: Vec<_> = batch
            .inputs
            .iter()
            .map(|input| input.ack_token.clone())
            .collect();
        self.queue.ack_consumed(run, tokens.clone()).await.unwrap();
        self.queue.confirm_read(run, tokens).await.unwrap();
    }

    async fn pass(&self) -> DispatchPass {
        self.dispatch
            .dispatch_thread(&self.thread_scope, &self.thread_id)
            .await
    }
}

/// Review P1-2: the oldest unread steer starts successor B, the second steers
/// B; B consumes it and completes BEFORE the old handoff is confirmed, and the
/// settlement is replayed. The replay must not start C with the second
/// message (on 7eb371917 it did: the ThreadBusy attempt reserved no submit
/// key, and the transcript was flipped only after the start). Each message is
/// delivered exactly once.
#[tokio::test]
async fn a_replayed_settlement_after_the_successor_ran_delivers_nothing_twice() {
    let f = Fixture::new().await;
    let ended = f.start_run();
    let first = f.steer_into(ended, "first unread").await;
    let second = f.steer_into(ended, "second unread").await;
    f.coordinator.finish(ended, TurnStatus::Completed);
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Ended)
        .await
        .unwrap();
    // Keep a copy of the handoff to replay it after it settled.
    let recorded = f.store.for_thread(&f.thread_id).await.unwrap();
    assert_eq!(recorded.len(), 1);

    assert_eq!(f.pass().await, DispatchPass::Idle);
    let submits = f.coordinator.submits();
    assert_eq!(submits.len(), 1, "one successor started");
    let successor = submits[0].requested_run_id.expect("successor id");
    assert_eq!(successor, successor_run_id(ended, first));
    assert_eq!(
        submits[0].accepted_message_ref.as_str(),
        format!("msg:{first}").as_str(),
        "the successor carries the oldest unread steer's own message"
    );
    assert_eq!(f.row(first).await.status, MessageStatus::Submitted);
    let second_row = f.row(second).await;
    assert_eq!(second_row.status, MessageStatus::Queued);
    assert_eq!(
        second_row.turn_run_id.as_deref(),
        Some(successor.to_string().as_str()),
        "the second steers the successor"
    );

    // The successor reads the second steer and completes.
    f.consume_all(successor).await;
    f.coordinator.finish(successor, TurnStatus::Completed);
    assert_eq!(f.row(second).await.status, MessageStatus::Submitted);

    // The old settlement is replayed (a repeated reconciliation, a crash
    // before confirmation): nothing is delivered again.
    for handoff in recorded {
        f.store.record(handoff).await.unwrap();
    }
    assert_eq!(f.pass().await, DispatchPass::Idle);
    assert_eq!(f.coordinator.submits().len(), 1, "no second successor");
    let second_row = f.row(second).await;
    assert_eq!(second_row.status, MessageStatus::Submitted);
    assert_eq!(
        second_row.turn_run_id.as_deref(),
        Some(successor.to_string().as_str())
    );
    assert!(f.store.for_thread(&f.thread_id).await.unwrap().is_empty());
}

/// Review P1-4: a Stop that lands after the run ended by itself (its
/// settlement already moved the steers into a handoff) holds them: no turn
/// starts after the Stop, and the rows settle not delivered.
#[tokio::test]
async fn a_stop_after_the_end_holds_the_unread_steers() {
    let f = Fixture::new().await;
    let ended = f.start_run();
    let unread = f.steer_into(ended, "unread before stop").await;
    f.coordinator.finish(ended, TurnStatus::Cancelled);
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Ended)
        .await
        .unwrap();
    // The cancel reconciliation of the same run, arriving second.
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Stopped)
        .await
        .unwrap();

    assert_eq!(f.pass().await, DispatchPass::Idle);
    assert!(
        f.coordinator.submits().is_empty(),
        "no turn starts after a Stop"
    );
    assert_eq!(f.row(unread).await.status, MessageStatus::RejectedBusy);
}

/// Review P1-3: the terminal settlement runs (and records its handoff)
/// before any dispatcher exists — a replayed terminal commit at startup, or
/// a dispatch whose retries ran out. The handoff waits durably; the sweep
/// delivers it. (On 7eb371917 a settlement without a bound handler settled
/// the steer `RejectedBusy`, and a failed handoff was left to a cursor that
/// had already advanced.)
#[tokio::test]
async fn a_handoff_recorded_before_the_dispatcher_runs_is_delivered_by_the_sweep() {
    let f = Fixture::new().await;
    let ended = f.start_run();
    let unread = f.steer_into(ended, "unread at startup").await;
    // Settled while the thread is still held, so nothing can start yet.
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Ended)
        .await
        .unwrap();
    assert_eq!(
        f.pass().await,
        DispatchPass::Waiting,
        "the ended run holds the thread"
    );
    assert_eq!(f.row(unread).await.status, MessageStatus::Queued);

    f.coordinator.finish(ended, TurnStatus::Completed);
    f.dispatch.sweep().await;
    assert_eq!(f.coordinator.submits().len(), 1);
    assert_eq!(f.row(unread).await.status, MessageStatus::Submitted);
}

/// Review P2-10: an entry queued by an older build carries no turn scope;
/// its handoff derives the thread owner's scope instead of rejecting it.
#[tokio::test]
async fn a_legacy_entry_without_a_turn_scope_is_still_delivered() {
    let f = Fixture::new().await;
    let ended = f.start_run();
    let message_id = f.accept("legacy steer").await;
    f.threads
        .mark_message_queued(&f.thread_scope, &f.thread_id, message_id, ended.to_string())
        .await
        .unwrap();
    f.queue
        .enqueue_queued_message(EnqueueQueuedMessageRequest {
            run_id: ended,
            turn_id: ironclaw_host_api::turn::TurnId::new(),
            scope: f.thread_scope.clone(),
            thread_id: f.thread_id.clone(),
            message_id,
            input: LoopInput::Steering {
                message_ref: LoopMessageRef::new(format!("msg:{message_id}")).unwrap(),
            },
            ack_effect: None,
        })
        .await
        .unwrap();
    f.coordinator.finish(ended, TurnStatus::Completed);
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Ended)
        .await
        .unwrap();
    assert_eq!(f.pass().await, DispatchPass::Idle);
    let submits = f.coordinator.submits();
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0].scope, f.turn_scope, "the derived owner scope");
    assert_eq!(f.row(message_id).await.status, MessageStatus::Submitted);
}

/// Review P1-1: admission sees the run serviceable, then the run finishes and
/// its settlement reclaims the queue before the enqueue lands — the enqueue
/// makes a fresh queue the run will never poll. The fence after the enqueue
/// sees the ended run, takes the steer back, and stages it for the next turn
/// (on 7eb371917 the caller got `DeferredBusy` for a run that never read
/// it).
#[tokio::test]
async fn a_late_admission_into_a_finished_run_is_staged_not_stranded() {
    let f = Fixture::new().await;
    let run = f.start_run();
    let message_id = f.accept("late steer").await;
    // Serviceable at the first read, completed (and reclaimed) by the fence.
    f.coordinator
        .script(run, &[TurnStatus::Running, TurnStatus::Completed]);
    let admission = crate::steering::admit_busy_steering(
        f.coordinator.as_ref(),
        SteeringAdmissionPorts {
            input_enqueue: f.queue.as_ref(),
            approvals: None,
            dispatch: Some(f.dispatch.as_ref()),
        },
        f.threads.as_ref(),
        SteeringAdmissionRequest {
            turn_scope: f.turn_scope.clone(),
            thread_scope: f.thread_scope.clone(),
            message_id,
            accepted_message_ref: AcceptedMessageRef::new(format!("msg:{message_id}")).unwrap(),
            active_run_id: run,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(admission, SteeringAdmission::Staged { .. }),
        "staged for the next turn"
    );
    assert!(
        f.queue
            .next_after(run, LoopInputCursorToken::origin(), 8)
            .await
            .unwrap()
            .inputs
            .is_empty(),
        "nothing is left in a queue the run never polls"
    );
    let handoffs = f.store.for_thread(&f.thread_id).await.unwrap();
    assert_eq!(handoffs.len(), 1);
    assert_eq!(handoffs[0].steers[0].kind, HandoffSteerKind::AfterEnd);

    // Once the run releases the thread, the message starts the next turn.
    f.coordinator.finish(run, TurnStatus::Completed);
    assert_eq!(f.pass().await, DispatchPass::Idle);
    assert_eq!(f.coordinator.submits().len(), 1);
    assert_eq!(f.row(message_id).await.status, MessageStatus::Submitted);
}

/// Review P1-4 (order): a newer message admitted while an older unread steer
/// waits must not overtake it — the inline dispatch pass lets the older one
/// claim the thread first, and the newer one then steers that run.
#[tokio::test]
async fn an_older_unread_steer_starts_before_a_newer_message() {
    let f = Fixture::new().await;
    let ended = f.start_run();
    let older = f.steer_into(ended, "older unread").await;
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Ended)
        .await
        .unwrap();
    f.coordinator.finish(ended, TurnStatus::Completed);

    // The inline pass every new send makes first.
    assert_eq!(f.pass().await, DispatchPass::Idle);
    let started = f.coordinator.submits();
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].accepted_message_ref.as_str(),
        format!("msg:{older}").as_str(),
        "the older steer took the thread"
    );
    // The newer message now finds the thread busy with that run.
    let newer = f.accept("newer message").await;
    let busy = f.coordinator.submit_turn(SubmitTurnRequest {
        idempotency_key: IdempotencyKey::new("newer").unwrap(),
        accepted_message_ref: AcceptedMessageRef::new(format!("msg:{newer}")).unwrap(),
        requested_run_id: None,
        ..started[0].clone()
    });
    assert!(matches!(busy.await, Err(TurnError::ThreadBusy(_))));
}

/// Review P1-7: the run parks on an approval DURING the enqueue; the decline
/// must use the state read after the enqueue (the snapshot before it showed
/// no approval).
#[tokio::test]
async fn an_approval_that_opens_during_the_enqueue_is_still_declined() {
    use crate::approval_interaction::{
        ApprovalInteractionDecision, ListPendingApprovalsRequest, ListPendingApprovalsResponse,
        ResolveApprovalInteractionRequest, ResolveApprovalInteractionResponse,
    };

    #[derive(Default)]
    struct Approvals {
        resolved: Mutex<Vec<ResolveApprovalInteractionRequest>>,
    }

    #[async_trait]
    impl ApprovalInteractionService for Approvals {
        async fn list_pending(
            &self,
            _request: ListPendingApprovalsRequest,
        ) -> Result<ListPendingApprovalsResponse, crate::ProductSurfaceFailure> {
            panic!("list_pending is not used")
        }

        async fn resolve(
            &self,
            request: ResolveApprovalInteractionRequest,
        ) -> Result<ResolveApprovalInteractionResponse, crate::ProductSurfaceFailure> {
            self.resolved.lock().unwrap().push(request.clone());
            Ok(ResolveApprovalInteractionResponse::Resumed(
                ResumeTurnResponse {
                    run_id: request.run_id_hint.unwrap_or_default(),
                    status: TurnStatus::Running,
                    event_cursor: Default::default(),
                },
            ))
        }
    }

    let f = Fixture::new().await;
    let run = f.start_run();
    {
        let mut runs = f.coordinator.runs.lock().unwrap();
        let state = runs.get_mut(&run).unwrap();
        state.gate_ref = Some(ironclaw_turns::TurnGateRef::new("gate:approval-late").unwrap());
    }
    f.coordinator
        .script(run, &[TurnStatus::Running, TurnStatus::BlockedApproval]);
    let approvals = Approvals::default();
    let message_id = f.accept("redirect after the call parked").await;
    let admission = crate::steering::admit_busy_steering(
        f.coordinator.as_ref(),
        SteeringAdmissionPorts {
            input_enqueue: f.queue.as_ref(),
            approvals: Some(&approvals),
            dispatch: Some(f.dispatch.as_ref()),
        },
        f.threads.as_ref(),
        SteeringAdmissionRequest {
            turn_scope: f.turn_scope.clone(),
            thread_scope: f.thread_scope.clone(),
            message_id,
            accepted_message_ref: AcceptedMessageRef::new(format!("msg:{message_id}")).unwrap(),
            active_run_id: run,
        },
    )
    .await
    .unwrap();
    assert!(matches!(admission, SteeringAdmission::Deferred { .. }));
    let resolved = approvals.resolved.lock().unwrap();
    assert_eq!(resolved.len(), 1, "the approval that opened is declined");
    assert!(matches!(
        resolved[0].decision,
        ApprovalInteractionDecision::Deny { .. }
    ));
}

/// Review P1-6: a handed-off channel steer must be answered where it
/// was sent — the successor inherits the ended run's channel reply targets —
/// and a steer a Stop held tells its conversation it was not delivered (on
/// 7eb371917 the successor had no reply target and the Stop changed only
/// the transcript row).
#[tokio::test]
async fn channel_steers_keep_their_reply_target_and_hear_about_a_stop() {
    #[derive(Default)]
    struct Targets {
        inherited: Mutex<Vec<(TurnRunId, TurnRunId)>>,
        not_delivered: Mutex<Vec<ThreadMessageId>>,
    }

    #[async_trait]
    impl SuccessorReplyTargets for Targets {
        async fn inherit(
            &self,
            _scope: &TurnScope,
            ended_run_id: TurnRunId,
            successor_run_id: TurnRunId,
        ) -> Result<(), String> {
            self.inherited
                .lock()
                .unwrap()
                .push((ended_run_id, successor_run_id));
            Ok(())
        }

        async fn notify_not_delivered(
            &self,
            _scope: &TurnScope,
            _ended_run_id: TurnRunId,
            message_id: ThreadMessageId,
        ) -> Result<(), String> {
            self.not_delivered.lock().unwrap().push(message_id);
            Ok(())
        }
    }

    let mut f = Fixture::new().await;
    let targets = Arc::new(Targets::default());
    f.dispatch = NativeSteerDispatch::new(
        Arc::clone(&f.coordinator) as Arc<dyn TurnCoordinator>,
        Arc::clone(&f.threads) as Arc<dyn SessionThreadService>,
        Arc::clone(&f.queue) as Arc<dyn HostInputEnqueuePort>,
        Arc::clone(&f.store) as Arc<dyn SteerHandoffStore>,
        Some(Arc::clone(&targets) as Arc<dyn SuccessorReplyTargets>),
        None,
    );

    // Ended by itself: the successor inherits the reply targets.
    let ended = f.start_run();
    let unread = f.steer_into(ended, "from a channel").await;
    f.coordinator.finish(ended, TurnStatus::Completed);
    f.queue
        .settle_unconsumed(ended, RunQueueEnding::Ended)
        .await
        .unwrap();
    assert_eq!(f.pass().await, DispatchPass::Idle);
    let successor = successor_run_id(ended, unread);
    assert_eq!(*targets.inherited.lock().unwrap(), vec![(ended, successor)]);

    // Stopped: the conversation hears that its steer was not delivered.
    f.coordinator.finish(successor, TurnStatus::Completed);
    let stopped = f.start_run();
    let held = f.steer_into(stopped, "sent before stop").await;
    f.coordinator.finish(stopped, TurnStatus::Cancelled);
    f.queue
        .settle_unconsumed(stopped, RunQueueEnding::Stopped)
        .await
        .unwrap();
    assert_eq!(f.pass().await, DispatchPass::Idle);
    assert_eq!(*targets.not_delivered.lock().unwrap(), vec![held]);
    assert_eq!(f.row(held).await.status, MessageStatus::RejectedBusy);
}
