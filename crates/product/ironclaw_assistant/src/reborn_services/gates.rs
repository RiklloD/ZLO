//! Approval and auth gate routing and the blocked-gate notices (`gates` charter
//! row).

use super::*;

pub const RESOLVE_GATE_COMMAND_ID: &str = "gate.resolve";

pub const RESOLVE_GATE_COMMAND: ProductSurfaceCommandDescriptor<
    ProductResolveGateRequest,
    RebornResolveGateResponse,
> = ProductSurfaceCommandDescriptor::new(RESOLVE_GATE_COMMAND_ID);

pub(super) const NOTICE_BLOCKED_APPROVAL: &str = "An approval gate is open on this thread — resolve it (approve or deny) before continuing, then resend your message.";

pub(super) const NOTICE_BLOCKED_AUTH: &str = "An authentication gate is open on this thread — complete authentication before continuing, then resend your message.";

#[derive(Clone, Copy)]
pub(super) enum GateResolutionRoute {
    Approval,
    Auth,
    Generic,
}

impl GateResolutionRoute {
    fn from_run_state(
        status: TurnStatus,
        parked_gate_ref: Option<&TurnGateRef>,
        requested_gate_ref: &TurnGateRef,
        resolution: &ProductGateResolution,
    ) -> Result<Self, ProductSurfaceError> {
        match status {
            TurnStatus::BlockedApproval => {
                validate_current_gate_ref(
                    parked_gate_ref,
                    requested_gate_ref,
                    ProductSurfaceErrorKind::BlockedApproval,
                )?;
                Ok(Self::Approval)
            }
            TurnStatus::BlockedAuth => {
                validate_current_gate_ref(
                    parked_gate_ref,
                    requested_gate_ref,
                    ProductSurfaceErrorKind::BlockedAuthentication,
                )?;
                Ok(Self::Auth)
            }
            status if status.is_terminal() => Err(ProductSurfaceError::from_status_kind(
                ProductSurfaceErrorCode::Conflict,
                ProductSurfaceErrorKind::Conflict,
                409,
                false,
            )),
            _ => Ok(Self::from_gate_shape(requested_gate_ref, resolution)),
        }
    }

    fn from_gate_shape(gate_ref: &TurnGateRef, resolution: &ProductGateResolution) -> Self {
        match (
            is_approval_gate_ref(gate_ref.as_str()),
            is_auth_gate_ref(gate_ref.as_str()),
            matches!(resolution, ProductGateResolution::CredentialProvided { .. }),
        ) {
            (true, _, _) => Self::Approval,
            (_, true, _) | (_, _, true) => Self::Auth,
            _ => Self::Generic,
        }
    }
}

pub(super) fn validate_current_gate_ref(
    parked_gate_ref: Option<&TurnGateRef>,
    requested_gate_ref: &TurnGateRef,
    kind: ProductSurfaceErrorKind,
) -> Result<(), ProductSurfaceError> {
    match parked_gate_ref {
        Some(parked) if parked == requested_gate_ref => Ok(()),
        _ => Err(ProductSurfaceError::from_status_kind(
            ProductSurfaceErrorCode::Conflict,
            kind,
            409,
            false,
        )),
    }
}

pub(super) fn participant_denied() -> ProductSurfaceError {
    ProductSurfaceError::from_status_kind(
        ProductSurfaceErrorCode::Forbidden,
        ProductSurfaceErrorKind::ParticipantDenied,
        403,
        false,
    )
}

/// Reject denied/cancelled generic gate resolutions whose `gate_ref` does not
/// match the gate the run is actually parked on. `cancel_run` is not gate-aware,
/// so without this guard a stale or attacker-supplied `gate_ref` would cancel
/// any non-terminal run sharing the same `run_id`.
pub(super) async fn assert_generic_run_parked_on_gate(
    turn_coordinator: &dyn TurnCoordinator,
    scope: &TurnScope,
    run_id: TurnRunId,
    expected_gate_ref: &TurnGateRef,
) -> Result<(), ProductSurfaceError> {
    let state = turn_coordinator
        .get_run_state(GetRunStateRequest {
            scope: scope.clone(),
            run_id,
        })
        .await
        .map_err(map_turn_error)?;
    if state.status == TurnStatus::BlockedAuth {
        return Err(blocked_authentication_unavailable());
    }
    if state.status == TurnStatus::BlockedApproval {
        return Err(blocked_approval_unavailable());
    }
    match state.gate_ref.as_ref() {
        Some(parked) if parked == expected_gate_ref => Ok(()),
        _ => Err(ProductSurfaceError::from_status_kind(
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::BlockedApproval,
            409,
            false,
        )),
    }
}

/// Generic WebUI gate handling is intentionally not allowed to resume or
/// cancel auth-blocked runs. Auth gates must pass through
/// AuthInteractionService so completed-flow/credential validation and
/// BlockedAuthGate preconditions are enforced.
pub(super) async fn reject_generic_auth_gate_resolution(
    turn_coordinator: &dyn TurnCoordinator,
    scope: &TurnScope,
    run_id: TurnRunId,
) -> Result<(), ProductSurfaceError> {
    let state = turn_coordinator
        .get_run_state(GetRunStateRequest {
            scope: scope.clone(),
            run_id,
        })
        .await
        .map_err(map_turn_error)?;
    if state.status == TurnStatus::BlockedAuth {
        return Err(blocked_authentication_unavailable());
    }
    if state.status == TurnStatus::BlockedApproval {
        return Err(blocked_approval_unavailable());
    }
    Ok(())
}

pub(super) fn persistent_approval_unavailable() -> ProductSurfaceError {
    ProductSurfaceError::from_status_kind(
        ProductSurfaceErrorCode::Unavailable,
        ProductSurfaceErrorKind::BlockedApproval,
        503,
        false,
    )
}

pub(super) fn blocked_approval_unavailable() -> ProductSurfaceError {
    persistent_approval_unavailable()
}

pub(super) fn blocked_authentication_unavailable() -> ProductSurfaceError {
    ProductSurfaceError::from_status_kind(
        ProductSurfaceErrorCode::Unavailable,
        ProductSurfaceErrorKind::BlockedAuthentication,
        503,
        false,
    )
}

pub(super) fn map_auth_interaction_error(error: ProductSurfaceFailure) -> ProductSurfaceError {
    match error {
        ProductSurfaceFailure::AuthInteractionRejected { kind } => {
            ProductSurfaceError::from_status_kind(
                code_for_status(kind.status_code()),
                ProductSurfaceErrorKind::BlockedAuthentication,
                kind.status_code(),
                kind.retryable(),
            )
        }
        error => map_adapter_error(error.into()),
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub async fn resolve_gate(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductResolveGateRequest,
    ) -> Result<RebornResolveGateResponse, ProductSurfaceError> {
        // A native `ask_user` gate takes answers, not an approval.
        let answering = request.resolution.as_deref() == Some("answered");
        let mut request = request;
        let answer = request.answer.take();
        if answering {
            // Parse (and validate) the rest as a dismissal would be.
            request.resolution = Some("declined".to_string());
        }
        let caller_for_fallback = caller.clone();
        let command = request.into_command(caller)?;
        let ProductInboundCommand::ResolveGate {
            scope,
            actor,
            run_id,
            gate_ref,
            client_action_id,
            resolution,
        } = command
        else {
            return Err(ProductSurfaceError::internal_invariant());
        };

        // Ownership probe with automation-trigger fallback. Trigger threads
        // return the trigger-owned scope and run actor; gate routing and resume
        // paths must use that run actor while authorization remains tied to the
        // WebUI caller's automation visibility.
        let access = self
            .resolve_thread_access_for_caller(caller_for_fallback, scope, &actor)
            .await?;
        if let Some(response) = self
            .resolve_native_question(
                &access.scope,
                &access.run_actor,
                run_id,
                &gate_ref,
                &client_action_id,
                answering.then_some(answer).flatten(),
            )
            .await?
        {
            return Ok(response);
        }
        if answering {
            // An answer for something that is not a waiting question.
            return Err(ProductSurfaceError::validation(
                "resolution",
                ProductSurfaceValidationCode::InvalidValue,
            ));
        }
        match self
            .gate_resolution_route(
                &access.scope,
                &access.run_actor,
                run_id,
                &gate_ref,
                &resolution,
            )
            .await?
        {
            GateResolutionRoute::Approval => {
                self.resolve_approval_gate(
                    access.scope,
                    access.run_actor,
                    run_id,
                    gate_ref,
                    client_action_id,
                    resolution,
                )
                .await
            }
            GateResolutionRoute::Auth => {
                self.resolve_auth_gate(
                    access.scope,
                    access.run_actor,
                    run_id,
                    gate_ref,
                    client_action_id,
                    resolution,
                )
                .await
            }
            GateResolutionRoute::Generic => {
                self.resolve_generic_gate(
                    access.scope,
                    access.run_actor,
                    run_id,
                    gate_ref,
                    client_action_id,
                    resolution,
                )
                .await
            }
        }
    }

    /// Ownership probe for `submit_turn` and `delete_thread` — these only
    /// operate on session-owned threads (not trigger threads), so the probe
    /// is user-scoped with no automation fallback.
    pub(super) async fn resolve_approval_gate(
        &self,
        scope: TurnScope,
        actor: TurnActor,
        run_id: TurnRunId,
        gate_ref: TurnGateRef,
        client_action_id: IdempotencyKey,
        resolution: ProductGateResolution,
    ) -> Result<RebornResolveGateResponse, ProductSurfaceError> {
        let Some(decision) = ApprovalInteractionDecision::from_gate_resolution(resolution) else {
            return Err(blocked_authentication_unavailable());
        };
        let response = self
            .approval_interactions
            .resolve(ResolveApprovalInteractionRequest {
                scope,
                actor,
                run_id_hint: Some(run_id),
                gate_ref,
                decision,
                idempotency_key: client_action_id,
            })
            .await
            .map_err(|error| map_adapter_error(error.into()))?;
        match response {
            ResolveApprovalInteractionResponse::Approved(response)
            | ResolveApprovalInteractionResponse::Resumed(response) => Ok(
                RebornResolveGateResponse::Resumed(types::reborn_resume_gate_response(response)),
            ),
        }
    }

    pub(super) async fn gate_resolution_route(
        &self,
        scope: &TurnScope,
        actor: &TurnActor,
        run_id: TurnRunId,
        gate_ref: &TurnGateRef,
        resolution: &ProductGateResolution,
    ) -> Result<GateResolutionRoute, ProductSurfaceError> {
        let state = match self
            .turn_coordinator
            .get_run_state(GetRunStateRequest {
                scope: scope.clone(),
                run_id,
            })
            .await
        {
            Ok(state) => state,
            Err(error) if error.category() == ironclaw_turns::TurnErrorCategory::ScopeNotFound => {
                return Ok(GateResolutionRoute::from_gate_shape(gate_ref, resolution));
            }
            Err(error) => return Err(map_turn_error(error)),
        };
        if state.actor.as_ref() != Some(actor) {
            return Err(participant_denied());
        }
        // This read only selects the WebUI route. The typed auth/approval
        // services intentionally re-read run-state through `blocked_gate_state`
        // before mutating auth/approval records or resuming/cancelling a run,
        // so stale service classification cannot authorize a side effect.
        GateResolutionRoute::from_run_state(
            state.status,
            state.gate_ref.as_ref(),
            gate_ref,
            resolution,
        )
    }

    pub(super) async fn resolve_auth_gate(
        &self,
        scope: TurnScope,
        actor: TurnActor,
        run_id: TurnRunId,
        gate_ref: TurnGateRef,
        client_action_id: IdempotencyKey,
        resolution: ProductGateResolution,
    ) -> Result<RebornResolveGateResponse, ProductSurfaceError> {
        let decision = match resolution {
            ProductGateResolution::CredentialProvided { credential_ref } => {
                AuthInteractionDecision::CredentialProvided {
                    credential_ref: parse_credential_account_id(&credential_ref)
                        .map_err(map_auth_interaction_error)?,
                }
            }
            // Auth gates have no call for feedback to explain; the decline
            // stands either way (the UI only offers feedback on approvals).
            ProductGateResolution::Declined { .. } => AuthInteractionDecision::Deny,
            ProductGateResolution::Approved { .. } => {
                return Err(blocked_authentication_unavailable());
            }
        };
        let response = self
            .auth_interactions
            .resolve(ResolveAuthInteractionRequest {
                scope,
                actor,
                run_id_hint: Some(run_id),
                gate_ref,
                decision,
                idempotency_key: client_action_id,
            })
            .await
            .map_err(map_auth_interaction_error)?;
        match response {
            ResolveAuthInteractionResponse::Resumed(response) => Ok(
                RebornResolveGateResponse::Resumed(types::reborn_resume_gate_response(response)),
            ),
            ResolveAuthInteractionResponse::Canceled(response) => Ok(
                RebornResolveGateResponse::Cancelled(types::reborn_cancel_run_response(response)),
            ),
        }
    }

    /// Answer (`Some`) or dismiss (`None`) the native `ask_user` question
    /// `run_id` is parked on at `gate_ref`: submit it as the call's output and
    /// resume the run. `None` when the run is not parked on a question there
    /// (the caller routes the resolution as a gate). An answer that does not
    /// fit the form is refused and the run stays parked.
    pub(super) async fn resolve_native_question(
        &self,
        scope: &TurnScope,
        actor: &TurnActor,
        run_id: TurnRunId,
        gate_ref: &TurnGateRef,
        client_action_id: &IdempotencyKey,
        answer: Option<ironclaw_host_api::agent_plan::QuestionAnswer>,
    ) -> Result<Option<RebornResolveGateResponse>, ProductSurfaceError> {
        let Some(catalog) = &self.external_tool_catalog else {
            return Ok(None);
        };
        // A run this lookup cannot see is not a waiting question: leave the
        // gate to the normal routing (approval, auth, generic), which answers
        // with its own error.
        let state = match self
            .turn_coordinator
            .get_run_state(GetRunStateRequest {
                scope: scope.clone(),
                run_id,
            })
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::debug!(error = %error, "native question lookup: run not readable");
                return Ok(None);
            }
        };
        if state.status != TurnStatus::BlockedExternalTool
            || state.gate_ref.as_ref() != Some(gate_ref)
        {
            return Ok(None);
        }
        let pending = crate::native_questions::pending_question(catalog.as_ref(), run_id)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        let Some((call_id, form)) = pending else {
            return Ok(None);
        };
        if let Some(answer) = &answer {
            form.check_answer(answer).map_err(|error| {
                tracing::debug!(%error, "native question answer refused");
                ProductSurfaceError::validation(
                    "answer",
                    ProductSurfaceValidationCode::InvalidValue,
                )
            })?;
        }
        let output = crate::native_questions::tool_output(&form, answer.as_ref());
        catalog
            .submit_output_for_pending_call(run_id, call_id, output)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        let response = self
            .turn_coordinator
            .resume_turn(ResumeTurnRequest {
                scope: scope.clone(),
                actor: actor.clone(),
                run_id,
                gate_resolution_ref: gate_ref.clone(),
                precondition: ResumeTurnPrecondition::BlockedExternalToolGate,
                idempotency_key: client_action_id.clone(),
                resume_disposition: None,
            })
            .await
            .map_err(map_turn_error)?;
        Ok(Some(RebornResolveGateResponse::Resumed(
            types::reborn_resume_gate_response(response),
        )))
    }

    pub(super) async fn resolve_generic_gate(
        &self,
        scope: TurnScope,
        actor: TurnActor,
        run_id: TurnRunId,
        gate_ref: TurnGateRef,
        client_action_id: IdempotencyKey,
        resolution: ProductGateResolution,
    ) -> Result<RebornResolveGateResponse, ProductSurfaceError> {
        match resolution {
            ProductGateResolution::Approved { always } => {
                reject_generic_auth_gate_resolution(self.turn_coordinator.as_ref(), &scope, run_id)
                    .await?;
                // Generic fallback has only one-shot `resume_turn`; persistent
                // approval must go through the typed approval interaction path.
                if always {
                    return Err(persistent_approval_unavailable());
                }
                let response = self
                    .turn_coordinator
                    .resume_turn(ResumeTurnRequest {
                        scope,
                        actor,
                        run_id,
                        gate_resolution_ref: gate_ref,
                        precondition: ResumeTurnPrecondition::AnyBlockedGate,
                        idempotency_key: client_action_id,
                        resume_disposition: None,
                    })
                    .await
                    .map_err(map_turn_error)?;
                Ok(RebornResolveGateResponse::Resumed(
                    types::reborn_resume_gate_response(response),
                ))
            }
            ProductGateResolution::CredentialProvided { .. } => {
                Err(blocked_authentication_unavailable())
            }
            ProductGateResolution::Declined { .. } => {
                assert_generic_run_parked_on_gate(
                    self.turn_coordinator.as_ref(),
                    &scope,
                    run_id,
                    &gate_ref,
                )
                .await?;
                // `cancel_run` is not gate-aware, so without this check a
                // denied/cancelled resolution for a stale or attacker-supplied
                // gate_ref would terminate any non-terminal run sharing run_id.
                let response = self
                    .turn_coordinator
                    .cancel_run(ironclaw_turns::CancelRunRequest {
                        scope,
                        actor,
                        run_id,
                        reason: SanitizedCancelReason::UserRequested,
                        idempotency_key: client_action_id,
                    })
                    .await
                    .map_err(map_turn_error)?;
                Ok(RebornResolveGateResponse::Cancelled(
                    types::reborn_cancel_run_response(response),
                ))
            }
        }
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker,
    V: RebornViewProvider,
{
    pub(super) async fn pending_approvals_for_thread_scope(
        &self,
        scope: &TurnScope,
        actor: &TurnActor,
    ) -> Result<Vec<PendingApprovalInteractionView>, ProductSurfaceError> {
        let pending = self
            .approval_interactions
            .list_pending(ListPendingApprovalsRequest {
                scope: scope.clone(),
                actor: actor.clone(),
            })
            .await
            .map_err(|error| map_adapter_error(error.into()))?;
        Ok(pending.approvals)
    }
}
