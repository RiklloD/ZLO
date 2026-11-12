//! Shared turn-coordinator and approval-interaction doubles.

use crate::*;

pub(crate) struct FakeTurnCoordinator {
    pub(crate) submissions: Mutex<Vec<SubmitTurnRequest>>,
    pub(crate) cancellations: Mutex<Vec<CancelRunRequest>>,
    pub(crate) resumptions: Mutex<Vec<ResumeTurnRequest>>,
    pub(crate) retries: Mutex<Vec<RetryTurnRequest>>,
    pub(crate) retry_attempts: Mutex<usize>,
    pub(crate) run_state_requests: Mutex<Vec<GetRunStateRequest>>,
    pub(crate) submit_error: Mutex<Option<TurnError>>,
    pub(crate) retry_error: Mutex<Option<TurnError>>,
    pub(crate) run_state_error: Mutex<Option<TurnError>>,
    pub(crate) run_state_actor: Mutex<Option<TurnActor>>,
    pub(crate) explicit_run_status: Mutex<Option<TurnStatus>>,
    pub(crate) parked_gate_ref: Mutex<Option<TurnGateRef>>,
    pub(crate) parked_auth_gate: Mutex<bool>,
    pub(crate) parked_approval_gate: Mutex<bool>,
    pub(crate) run_state_failure: Mutex<Option<SanitizedFailure>>,
    pub(crate) run_state_usage: Mutex<Option<LoopModelUsage>>,
    pub(crate) run_state_model_route: Mutex<Option<LoopModelRouteSnapshot>>,
}

impl Default for FakeTurnCoordinator {
    fn default() -> Self {
        Self {
            submissions: Mutex::default(),
            cancellations: Mutex::default(),
            resumptions: Mutex::default(),
            retries: Mutex::default(),
            retry_attempts: Mutex::default(),
            run_state_requests: Mutex::default(),
            submit_error: Mutex::default(),
            retry_error: Mutex::default(),
            run_state_error: Mutex::default(),
            run_state_actor: Mutex::new(Some(turn_actor_for_user("user-alpha"))),
            explicit_run_status: Mutex::default(),
            parked_gate_ref: Mutex::default(),
            parked_auth_gate: Mutex::default(),
            parked_approval_gate: Mutex::default(),
            run_state_failure: Mutex::default(),
            run_state_usage: Mutex::default(),
            run_state_model_route: Mutex::default(),
        }
    }
}

impl FakeTurnCoordinator {
    pub(crate) fn with_submit_error(error: TurnError) -> Self {
        Self {
            submit_error: Mutex::new(Some(error)),
            ..Self::default()
        }
    }

    pub(crate) fn with_run_state_error(error: TurnError) -> Self {
        Self {
            run_state_error: Mutex::new(Some(error)),
            ..Self::default()
        }
    }

    pub(crate) fn with_retry_error(error: TurnError) -> Self {
        Self {
            retry_error: Mutex::new(Some(error)),
            ..Self::default()
        }
    }

    /// Programs `get_run_state` to return this gate as the run's currently
    /// parked gate. Needed by tests that exercise `resolve_gate` denied/
    /// cancelled paths now that `RebornServices` verifies the run is parked
    /// on the supplied gate before issuing cancellation.
    pub(crate) fn set_parked_gate(&self, gate_ref: TurnGateRef) {
        *self.parked_gate_ref.lock().expect("lock") = Some(gate_ref);
        *self.parked_auth_gate.lock().expect("lock") = false;
        *self.parked_approval_gate.lock().expect("lock") = false;
    }

    pub(crate) fn set_parked_auth_gate(&self, gate_ref: TurnGateRef) {
        *self.parked_gate_ref.lock().expect("lock") = Some(gate_ref);
        *self.parked_auth_gate.lock().expect("lock") = true;
        *self.parked_approval_gate.lock().expect("lock") = false;
    }

    pub(crate) fn set_parked_approval_gate(&self, gate_ref: TurnGateRef) {
        *self.parked_gate_ref.lock().expect("lock") = Some(gate_ref);
        *self.parked_auth_gate.lock().expect("lock") = false;
        *self.parked_approval_gate.lock().expect("lock") = true;
    }

    pub(crate) fn set_run_state_actor(&self, actor: Option<TurnActor>) {
        *self.run_state_actor.lock().expect("lock") = actor;
    }

    pub(crate) fn set_run_state_status(&self, status: TurnStatus) {
        *self.explicit_run_status.lock().expect("lock") = Some(status);
    }

    pub(crate) fn set_run_state_failure(&self, failure: SanitizedFailure) {
        *self.run_state_failure.lock().expect("lock") = Some(failure);
    }

    pub(crate) fn set_run_state_usage(
        &self,
        usage: LoopModelUsage,
        model_route: LoopModelRouteSnapshot,
    ) {
        *self.run_state_usage.lock().expect("lock") = Some(usage);
        *self.run_state_model_route.lock().expect("lock") = Some(model_route);
    }

    /// Report usage for a default-model run: token usage is captured but no
    /// `resolved_model_route` is set (the caller did not pick a model).
    pub(crate) fn set_run_state_usage_default_model(&self, usage: LoopModelUsage) {
        *self.run_state_usage.lock().expect("lock") = Some(usage);
        *self.run_state_model_route.lock().expect("lock") = None;
    }

    pub(crate) fn submission_count(&self) -> usize {
        self.submissions.lock().expect("lock").len()
    }

    pub(crate) fn cancellation_count(&self) -> usize {
        self.cancellations.lock().expect("lock").len()
    }

    pub(crate) fn resumption_count(&self) -> usize {
        self.resumptions.lock().expect("lock").len()
    }

    pub(crate) fn retry_count(&self) -> usize {
        self.retries.lock().expect("lock").len()
    }

    pub(crate) fn retry_attempt_count(&self) -> usize {
        *self.retry_attempts.lock().expect("lock")
    }

    pub(crate) fn run_state_request_count(&self) -> usize {
        self.run_state_requests.lock().expect("lock").len()
    }

    pub(crate) fn last_resumption_precondition(&self) -> Option<ResumeTurnPrecondition> {
        self.resumptions
            .lock()
            .expect("lock")
            .last()
            .map(|request| request.precondition)
    }

    pub(crate) fn last_retry(&self) -> Option<RetryTurnRequest> {
        self.retries.lock().expect("lock").last().cloned()
    }

    pub(crate) fn last_submission_scope(&self) -> Option<ironclaw_turns::TurnScope> {
        self.submissions
            .lock()
            .expect("lock")
            .last()
            .map(|request| request.scope.clone())
    }

    pub(crate) fn last_requested_model(&self) -> Option<String> {
        self.submissions
            .lock()
            .expect("lock")
            .last()
            .and_then(|request| request.requested_model.clone())
    }

    pub(crate) fn last_submission_origin_kind(&self) -> Option<TurnOriginKind> {
        self.submissions
            .lock()
            .expect("lock")
            .last()
            .and_then(|request| request.product_context.as_ref().map(|c| c.origin))
    }

    pub(crate) fn last_cancellation_scope(&self) -> Option<TurnScope> {
        self.cancellations
            .lock()
            .expect("lock")
            .last()
            .map(|request| request.scope.clone())
    }

    pub(crate) fn last_cancellation_actor(&self) -> Option<TurnActor> {
        self.cancellations
            .lock()
            .expect("lock")
            .last()
            .map(|request| request.actor.clone())
    }

    /// Returns the `TurnScope` from the most recent `get_run_state` call.
    ///
    /// Used by trigger-thread tests to assert that `resolve_gate`,
    /// `cancel_run`, and `get_run_state` receive the trigger-owned scope
    /// (with `owner_user_id = Some(creator_user_id)`) rather than the
    /// WebUI caller's session scope.  This distinction is what #4754 ("Part A")
    /// and the `check_automation_trigger_access` reconstruction guarantee.
    pub(crate) fn last_run_state_scope(&self) -> Option<TurnScope> {
        self.run_state_requests
            .lock()
            .expect("lock")
            .last()
            .map(|request| request.scope.clone())
    }
}

#[async_trait]
impl TurnCoordinator for FakeTurnCoordinator {
    async fn prepare_turn(&self, _scope: TurnScope) -> Result<TurnRunId, TurnError> {
        Ok(TurnRunId::new())
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        if let Some(error) = self.submit_error.lock().expect("lock").take() {
            return Err(error);
        }
        self.submissions.lock().expect("lock").push(request.clone());
        Ok(SubmitTurnResponse::Accepted {
            turn_id: TurnId::new(),
            run_id: TurnRunId::new(),
            status: TurnStatus::Queued,
            resolved_run_profile_id: RunProfileId::default_profile(),
            resolved_run_profile_version: RunProfileVersion::new(1),
            event_cursor: EventCursor(7),
            accepted_message_ref: request.accepted_message_ref,
        })
    }

    async fn resume_turn(
        &self,
        request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        self.resumptions.lock().expect("lock").push(request);
        Ok(ResumeTurnResponse {
            run_id: TurnRunId::new(),
            status: TurnStatus::Queued,
            event_cursor: EventCursor(11),
        })
    }

    async fn retry_turn(&self, request: RetryTurnRequest) -> Result<RetryTurnResponse, TurnError> {
        *self.retry_attempts.lock().expect("lock") += 1;
        if let Some(error) = self.retry_error.lock().expect("lock").take() {
            return Err(error);
        }
        self.retries.lock().expect("lock").push(request);
        Ok(RetryTurnResponse {
            run_id: TurnRunId::new(),
            status: TurnStatus::Queued,
            event_cursor: EventCursor(19),
        })
    }

    async fn cancel_run(&self, request: CancelRunRequest) -> Result<CancelRunResponse, TurnError> {
        let run_id = request.run_id;
        self.cancellations.lock().expect("lock").push(request);
        Ok(CancelRunResponse {
            run_id,
            status: TurnStatus::Cancelled,
            event_cursor: EventCursor(13),
            already_terminal: false,
            actor: None,
        })
    }

    async fn get_run_state(&self, request: GetRunStateRequest) -> Result<TurnRunState, TurnError> {
        if let Some(error) = self.run_state_error.lock().expect("lock").take() {
            return Err(error);
        }
        let actor = self.run_state_actor.lock().expect("lock").clone();
        let gate_ref = self.parked_gate_ref.lock().expect("lock").clone();
        let status = self
            .explicit_run_status
            .lock()
            .expect("lock")
            .unwrap_or_else(|| {
                if *self.parked_auth_gate.lock().expect("lock") {
                    TurnStatus::BlockedAuth
                } else if *self.parked_approval_gate.lock().expect("lock") {
                    TurnStatus::BlockedApproval
                } else {
                    TurnStatus::Queued
                }
            });
        let scope = request.scope.clone();
        let run_id = request.run_id;
        self.run_state_requests.lock().expect("lock").push(request);
        Ok(TurnRunState {
            scope,
            actor,
            turn_id: TurnId::new(),
            run_id,
            status,
            accepted_message_ref: AcceptedMessageRef::new("msg:replayed").expect("valid ref"),
            resolved_run_profile_id: RunProfileId::default_profile(),
            resolved_run_profile_version: RunProfileVersion::new(1),
            output_contract: ironclaw_host_api::output::OutputContract::AssistantMessage,
            allow_steering: true,
            resolved_model_route: self.run_state_model_route.lock().expect("lock").clone(),
            model_usage: *self.run_state_usage.lock().expect("lock"),
            execution_outcome: None,
            received_at: Utc::now(),
            checkpoint_id: None,
            gate_ref,
            blocked_activity_id: None,
            credential_requirements: Vec::new(),
            failure: self.run_state_failure.lock().expect("lock").clone(),
            event_cursor: EventCursor(17),
            product_context: None,
            resume_disposition: None,
        })
    }
}

#[derive(Default)]
pub(crate) struct RecordingApprovalInteractionService {
    pub(crate) resolutions: Mutex<Vec<ResolveApprovalInteractionRequest>>,
}

impl RecordingApprovalInteractionService {
    pub(crate) fn resolution_count(&self) -> usize {
        self.resolutions.lock().expect("lock").len()
    }

    pub(crate) fn last_resolution(&self) -> Option<ResolveApprovalInteractionRequest> {
        self.resolutions.lock().expect("lock").last().cloned()
    }
}

#[async_trait]
impl ApprovalInteractionService for RecordingApprovalInteractionService {
    async fn list_pending(
        &self,
        _request: ListPendingApprovalsRequest,
    ) -> Result<ListPendingApprovalsResponse, ironclaw_assistant::ProductSurfaceFailure> {
        Ok(ListPendingApprovalsResponse { approvals: vec![] })
    }

    async fn resolve(
        &self,
        request: ResolveApprovalInteractionRequest,
    ) -> Result<ResolveApprovalInteractionResponse, ironclaw_assistant::ProductSurfaceFailure> {
        let run_id = request.run_id_hint.expect("webui passes run_id");
        let decision = request.decision.clone();
        self.resolutions.lock().expect("lock").push(request);
        Ok(match decision {
            ApprovalInteractionDecision::ApproveOnce | ApprovalInteractionDecision::AlwaysAllow => {
                ResolveApprovalInteractionResponse::Approved(ResumeTurnResponse {
                    run_id,
                    status: TurnStatus::Queued,
                    event_cursor: EventCursor(19),
                })
            }
            ApprovalInteractionDecision::Deny { .. } => {
                ResolveApprovalInteractionResponse::Resumed(ResumeTurnResponse {
                    run_id,
                    status: TurnStatus::Queued,
                    event_cursor: EventCursor(23),
                })
            }
        })
    }
}
