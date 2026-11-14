//! Turn submission, cancel, retry, run state, and steering admission
//! (`runs` charter row).

use super::*;

pub const SUBMIT_TURN_COMMAND_ID: &str = "turn.submit";

pub const SUBMIT_TURN_COMMAND: ProductSurfaceCommandDescriptor<
    ProductSubmitTurnRequest,
    RebornSubmitTurnResponse,
> = ProductSurfaceCommandDescriptor::new(SUBMIT_TURN_COMMAND_ID);

pub const CANCEL_RUN_COMMAND_ID: &str = "run.cancel";

pub const CANCEL_RUN_COMMAND: ProductSurfaceCommandDescriptor<
    ProductCancelRunRequest,
    RebornCancelRunResponse,
> = ProductSurfaceCommandDescriptor::new(CANCEL_RUN_COMMAND_ID);

pub const RETRY_RUN_COMMAND_ID: &str = "run.retry";

pub const RETRY_RUN_COMMAND: ProductSurfaceCommandDescriptor<
    ProductRetryRunRequest,
    RebornRetryRunResponse,
> = ProductSurfaceCommandDescriptor::new(RETRY_RUN_COMMAND_ID);

pub(super) const NOTICE_BUSY_GENERIC: &str = "Ironclaw is still working on a previous message — resend yours once the current task finishes.";

pub(super) fn describe_turn_status(status: TurnStatus) -> (&'static str, Option<&'static str>) {
    match status {
        TurnStatus::Queued => ("queued", None),
        TurnStatus::Running => ("working", None),
        TurnStatus::BlockedApproval => (
            "waiting for approval",
            Some("Reply `approve` or `deny` to continue."),
        ),
        TurnStatus::BlockedAuth => (
            "waiting for authentication",
            Some("Complete the pending connection to continue."),
        ),
        TurnStatus::CancelRequested => ("cancelling", None),
        TurnStatus::Completed => ("idle", Some("The last task completed.")),
        TurnStatus::Failed => ("idle", Some("The last task failed.")),
        TurnStatus::Cancelled => ("idle", Some("The last task was cancelled.")),
        _ => ("working", None),
    }
}

pub(super) fn rejected_busy_notice(status: TurnStatus) -> String {
    match status {
        TurnStatus::BlockedApproval => NOTICE_BLOCKED_APPROVAL.to_string(),
        TurnStatus::BlockedAuth => NOTICE_BLOCKED_AUTH.to_string(),
        _ => NOTICE_BUSY_GENERIC.to_string(),
    }
}

/// Session-only projection of tenant model selection onto the existing
/// before-inbound policy seam. The workflow invokes this after both replay
/// checks and before attachment landing or message acceptance.
pub(super) struct SessionModelSelectionPolicy {
    llm_config: Option<Arc<dyn LlmConfigService>>,
}

#[async_trait]
impl BeforeInboundPolicy for SessionModelSelectionPolicy {
    async fn check_user_message(
        &self,
        request: BeforeInboundPolicyRequest,
    ) -> Result<BeforeInboundPolicyOutcome, ProductSurfaceFailure> {
        let Some(llm_config) = self.llm_config.as_ref() else {
            return Ok(BeforeInboundPolicyOutcome::Allow);
        };
        let Some(caller) = request.session_caller else {
            return Err(ProductSurfaceFailure::BeforeInboundPolicyFailed {
                reason: "session model policy received a webhook message".to_string(),
                permanent: true,
            });
        };
        let requested_model = request.user_message.requested_model.clone();
        match llm_config
            .resolve_user_model(caller, requested_model.clone())
            .await
        {
            Ok(resolved_model) if resolved_model == requested_model => {
                Ok(BeforeInboundPolicyOutcome::Allow)
            }
            Ok(resolved_model) => {
                let mut user_message = request.user_message;
                user_message.requested_model = resolved_model;
                Ok(BeforeInboundPolicyOutcome::RewriteUserMessage(user_message))
            }
            Err(LlmConfigServiceError::InvalidRequest { reason, .. }) => {
                Ok(BeforeInboundPolicyOutcome::Reject(
                    ProductRejection::permanent(ProductRejectionKind::InvalidRequest, reason),
                ))
            }
            Err(LlmConfigServiceError::NotFound) => Ok(BeforeInboundPolicyOutcome::Reject(
                ProductRejection::permanent(
                    ProductRejectionKind::InvalidRequest,
                    "requested model is unavailable",
                ),
            )),
            Err(LlmConfigServiceError::Unavailable) => {
                Err(ProductSurfaceFailure::BeforeInboundPolicyFailed {
                    reason: "model selection policy is unavailable".to_string(),
                    permanent: false,
                })
            }
            // `NotApplied` is a save-path outcome resolution never yields.
            Err(LlmConfigServiceError::Internal | LlmConfigServiceError::NotApplied { .. }) => {
                // A backend fault is not a policy verdict: `permanent: true`
                // would settle Rejected(PolicyDenied) in the durable session
                // idempotency ledger, poisoning this client_action_id even
                // after the backend recovers. Fail transient so the caller
                // can retry the same action.
                Err(ProductSurfaceFailure::BeforeInboundPolicyFailed {
                    reason: "model selection policy failed".to_string(),
                    permanent: false,
                })
            }
        }
    }
}

pub(super) fn parse_run_id_field(
    field: &'static str,
    value: String,
) -> Result<TurnRunId, ProductSurfaceError> {
    Uuid::parse_str(&value)
        .map(TurnRunId::from_uuid)
        .map_err(|_| {
            ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidId)
        })
}

pub(super) fn parse_persisted_turn_run_id(value: &str) -> Result<TurnRunId, ProductSurfaceError> {
    TurnRunId::parse(value).map_err(ProductSurfaceError::internal_from)
}

/// Transport identity stamped on session-lane submissions that did not name
/// a channel extension (the OpenAI-compatible API transports). Not a channel
/// name: `webui` is the product transport itself, the same constant the turn
/// kernel uses for the WebUi source channel — and not route-addressable, the
/// parameterized session route resolves only manifest-declared channels.
pub(super) const SESSION_SURFACE_ADAPTER_ID: &str =
    ironclaw_product_contracts::session_ingress::BUILTIN_SESSION_SURFACE_ID;

/// External-actor ref kind for session callers in admission fingerprints.
pub(super) const SESSION_ACTOR_KIND: &str = "session_user";

/// Build the neutral inbound-surface request for one session submission.
pub(super) fn session_inbound_request(
    session_surface: &str,
    caller: ProductSurfaceCaller,
    thread_id: &ThreadId,
    client_action_id: &IdempotencyKey,
    content: String,
    requested_model: Option<String>,
    attachments: Vec<ironclaw_host_api::attachment::InboundAttachment>,
) -> Result<ChannelInboundSurfaceRequest, ProductSurfaceError> {
    let adapter_id =
        ProductAdapterId::new(session_surface).map_err(ProductSurfaceError::internal_from)?;
    let source_channel =
        ironclaw_product_contracts::inbound::ProductSourceChannel::new(session_surface)
            .map_err(ProductSurfaceError::internal_from)?;
    let installation_id = AdapterInstallationId::new(caller.tenant_id.as_str())
        .map_err(ProductSurfaceError::internal_from)?;
    // Session and webhook ingress converge on the same complete attachment
    // type. Provider descriptors and download handles never cross this edge.
    let message = ironclaw_extension_contracts::channel_adapter::NormalizedInboundMessage {
        actor: ironclaw_extension_contracts::external::ExternalActorRef::new(
            SESSION_ACTOR_KIND,
            caller.user_id.as_str(),
            Option::<String>::None,
        )
        .map_err(ProductSurfaceError::internal_from)?,
        conversation: ironclaw_extension_contracts::external::ExternalConversationRef::new(
            None,
            thread_id.as_str(),
            None,
            None,
        )
        .map_err(ProductSurfaceError::internal_from)?,
        event_id: ironclaw_extension_contracts::external::ExternalEventId::new(
            client_action_id.as_str(),
        )
        .map_err(ProductSurfaceError::internal_from)?,
        text: content,
        trigger: ironclaw_extension_contracts::channel_adapter::ProductTriggerReason::DirectChat,
        attachments,
        conversation_context: None,
        reply_context: None,
    };
    Ok(ChannelInboundSurfaceRequest {
        context: ironclaw_product_contracts::inbound::TrustedInboundContext::from_session_caller(
            adapter_id,
            source_channel,
            installation_id,
            Utc::now(),
            caller,
            thread_id.clone(),
        ),
        message,
        classification: None,
        requested_model,
    })
}

/// Render a settled workflow rejection replayed for a session submission.
pub(super) fn session_rejection_error(
    rejection: &ironclaw_product_contracts::inbound::ProductRejection,
) -> ProductSurfaceError {
    use ironclaw_product_contracts::inbound::ProductRejectionKind;
    let retryable = rejection.disposition()
        == ironclaw_product_contracts::inbound::ProductRejectionDisposition::Retryable;
    match rejection.kind {
        ProductRejectionKind::InvalidRequest => ProductSurfaceError::from_status(
            ProductSurfaceErrorCode::InvalidRequest,
            400,
            retryable,
        ),
        ProductRejectionKind::AccessDenied
        | ProductRejectionKind::UnknownInstallation
        | ProductRejectionKind::PolicyDenied => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Forbidden, 403, retryable)
        }
        ProductRejectionKind::BindingRequired => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, retryable)
        }
        ProductRejectionKind::AmbiguousResolution | ProductRejectionKind::StaleGate => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Conflict, 409, retryable)
        }
    }
}

pub(super) fn map_turn_error(error: TurnError) -> ProductSurfaceError {
    if matches!(error, TurnError::RunNotRetryable { .. }) {
        return ProductSurfaceError::from_status_kind(
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::Conflict,
            409,
            false,
        );
    }
    let (code, kind, status_code, retryable) = match error.category() {
        ironclaw_turns::TurnErrorCategory::ThreadBusy => (
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::Busy,
            409,
            false,
        ),
        ironclaw_turns::TurnErrorCategory::Conflict => (
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::Conflict,
            409,
            false,
        ),
        ironclaw_turns::TurnErrorCategory::AdmissionRejected => (
            ProductSurfaceErrorCode::RateLimited,
            ProductSurfaceErrorKind::Busy,
            429,
            true,
        ),
        ironclaw_turns::TurnErrorCategory::CapacityExceeded => (
            ProductSurfaceErrorCode::RateLimited,
            ProductSurfaceErrorKind::Busy,
            429,
            false,
        ),
        ironclaw_turns::TurnErrorCategory::ScopeNotFound => (
            ProductSurfaceErrorCode::NotFound,
            ProductSurfaceErrorKind::NotFound,
            404,
            false,
        ),
        ironclaw_turns::TurnErrorCategory::Unauthorized => (
            ProductSurfaceErrorCode::Forbidden,
            ProductSurfaceErrorKind::ParticipantDenied,
            403,
            false,
        ),
        ironclaw_turns::TurnErrorCategory::InvalidRequest => (
            ProductSurfaceErrorCode::InvalidRequest,
            ProductSurfaceErrorKind::Validation,
            400,
            false,
        ),
        ironclaw_turns::TurnErrorCategory::Unavailable => (
            ProductSurfaceErrorCode::Unavailable,
            ProductSurfaceErrorKind::ServiceUnavailable,
            503,
            true,
        ),
    };
    ProductSurfaceError::from_status_kind(code, kind, status_code, retryable)
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    /// Submit one session user message through the unified inbound core.
    ///
    /// This is the authenticated-session lane of the same admission pipeline
    /// webhook channels ride: durable idempotency ledger → owned-thread
    /// binding → `TurnCoordinator::submit_turn`. The caller owns the thread
    /// (never created implicitly), `client_action_id` replay is preserved —
    /// including messages accepted under the legacy binding-id schemes — and
    /// the response wire shape is unchanged.
    pub async fn submit_turn(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSubmitTurnRequest,
    ) -> Result<RebornSubmitTurnResponse, ProductSurfaceError> {
        // Decode + budget inline attachment bytes before the request is
        // consumed into the (bytes-free, serializable) command.
        let attachments = request.decode_attachments()?;
        let command = request.into_command(caller.clone())?;
        let ProductInboundCommand::SendMessage {
            scope,
            actor,
            client_action_id,
            content,
            requested_model,
            extension_id,
        } = command
        else {
            return Err(ProductSurfaceError::internal_invariant());
        };
        // A channel-parameterized submission must name a deployment channel
        // whose declared entrypoint is the authenticated session — fail
        // closed (404, indistinguishable from an absent route) otherwise. An
        // unparameterized submission is the legacy API lane
        // (`ProductSubmitTurnRequest::extension_id`): OpenAI-compatible
        // clients cannot learn a channel id, so they submit under the
        // built-in surface identity. That id stays out of the parameterized
        // route — naming it there still 404s below unless a manifest channel
        // claims it.
        let session_surface = match &extension_id {
            Some(extension_id) => {
                let Some(directory) = &self.session_channels else {
                    return Err(ProductSurfaceError::service_unavailable(false));
                };
                if !directory.is_session_channel(extension_id) {
                    return Err(ProductSurfaceError::not_found());
                }
                extension_id.as_str()
            }
            None => SESSION_SURFACE_ADAPTER_ID,
        };
        let thread_id = scope.thread_id.clone();
        // Serialize with thread deletion (delete_thread holds the same
        // per-thread lock across its active-run probe + delete).
        let _thread_operation_guard = self.lock_thread_operation(&scope).await;

        let session_caller = ProductSurfaceCaller::new(
            scope.tenant_id.clone(),
            actor.user_id.clone(),
            scope.agent_id.clone(),
            scope.project_id.clone(),
        );
        let neutral = session_inbound_request(
            session_surface,
            session_caller,
            &thread_id,
            &client_action_id,
            content,
            requested_model,
            attachments,
        )?;
        let core = self.session_inbound_core();
        let outcome = core.admit_channel_inbound(neutral).await;
        self.session_submit_response(&scope, &thread_id, outcome)
            .await
    }

    /// The session-lane inbound core: the same `DefaultProductSurface`
    /// implementation webhook channels run, constructed over this service's
    /// own ports plus the durable session idempotency ledger. The surface is
    /// memoized once after all builder-wired ports have been attached.
    pub(super) fn session_inbound_core(&self) -> Arc<DefaultProductSurface> {
        Arc::clone(
            self.session_inbound_surface
                .get_or_init(|| Arc::new(self.build_session_inbound_core())),
        )
    }

    /// Compose the session lane's surface. Called once per service instance
    /// through [`Self::session_inbound_core`]'s memoization.
    pub(super) fn build_session_inbound_core(&self) -> DefaultProductSurface {
        let mut inbound = DefaultInboundTurnService::new(
            SessionLaneRejectingBindingResolver,
            Arc::clone(&self.thread_service),
            Arc::clone(&self.turn_coordinator),
            Arc::clone(&self.input_enqueue),
        );
        if let Some(lander) = &self.inbound_attachments {
            inbound = inbound.with_inbound_attachments(Arc::clone(lander));
        }
        if let Some(recorder) = &self.skill_activation_recorder {
            let clearer: Arc<SkillActivationClearer> = match &self.skill_activation_clearer {
                Some(clearer) => Arc::clone(clearer),
                None => Arc::new(|_: &TurnScope, _: &AcceptedMessageRef| Ok(())),
            };
            inbound = inbound.with_session_skill_activation(SessionSkillActivationPorts {
                recorder: Arc::clone(recorder),
                clearer,
            });
        }
        DefaultProductSurface::new(
            Arc::new(inbound),
            Arc::clone(&self.session_inbound_ledger),
            Arc::new(SessionLaneRejectingBindingResolver),
        )
        .with_before_inbound_policy(Arc::new(SessionModelSelectionPolicy {
            llm_config: self.llm_config.clone(),
        }))
    }

    pub(super) async fn session_submit_response(
        &self,
        scope: &TurnScope,
        thread_id: &ThreadId,
        outcome: ChannelInboundSurfaceOutcome,
    ) -> Result<RebornSubmitTurnResponse, ProductSurfaceError> {
        let ack = match outcome {
            ChannelInboundSurfaceOutcome::Admitted(admission) => admission.ack,
            ChannelInboundSurfaceOutcome::Invalid(error) => return Err(map_adapter_error(error)),
            ChannelInboundSurfaceOutcome::Rejected(rejected) => {
                return Err(map_adapter_error(rejected.error));
            }
        };
        // A ledger replay wraps the settled outcome; unwrap to the prior ack
        // and render it AS a replay — never as a fresh submission, whatever
        // metadata the stored ack carries.
        let mut ack = ack;
        let mut replayed = false;
        while let ProductInboundAck::Duplicate { prior } = ack {
            ack = *prior;
            replayed = true;
        }
        match ack {
            ProductInboundAck::Accepted {
                accepted_message_ref,
                submitted_run_id,
                submission: Some(submission),
            } if !replayed => Ok(RebornSubmitTurnResponse::Submitted {
                thread_id: thread_id.clone(),
                accepted_message_ref,
                turn_id: submission.turn_id,
                run_id: submitted_run_id,
                status: submission.status,
                resolved_run_profile_id: submission.resolved_run_profile_id,
                resolved_run_profile_version: submission.resolved_run_profile_version,
                event_cursor: submission.event_cursor,
            }),
            ProductInboundAck::Accepted {
                accepted_message_ref,
                submitted_run_id,
                ..
            } => {
                // A replayed submission reports the run's CURRENT state, the
                // same read the dedicated browser path always performed.
                let state = self
                    .turn_coordinator
                    .get_run_state(GetRunStateRequest {
                        scope: scope.clone(),
                        run_id: submitted_run_id,
                    })
                    .await
                    .map_err(map_turn_error)?;
                Ok(RebornSubmitTurnResponse::AlreadySubmitted {
                    thread_id: thread_id.clone(),
                    accepted_message_ref,
                    run_id: submitted_run_id,
                    status: state.status,
                    event_cursor: state.event_cursor,
                })
            }
            ProductInboundAck::RejectedBusy {
                accepted_message_ref,
                active_run_id,
                busy,
            } => {
                // A ledger-replayed busy rejection reports no run metadata:
                // the original blocking run may already be gone, and handing
                // the client a stale reference invites dead lookups. Fresh
                // rejections keep the decision-time snapshot.
                let (active_run_id, busy) = if replayed {
                    (None, None)
                } else {
                    (active_run_id, busy)
                };
                let notice = busy
                    .as_deref()
                    .map(|snapshot| rejected_busy_notice(snapshot.status))
                    .unwrap_or_else(|| NOTICE_BUSY_GENERIC.to_string());
                Ok(RebornSubmitTurnResponse::RejectedBusy {
                    thread_id: thread_id.clone(),
                    accepted_message_ref,
                    active_run_id,
                    status: busy.as_deref().map(|snapshot| snapshot.status),
                    event_cursor: busy.as_deref().map(|snapshot| snapshot.event_cursor),
                    notice,
                })
            }
            ProductInboundAck::DeferredBusy {
                accepted_message_ref,
                active_run_id,
                busy,
            } => {
                let Some(busy) = busy else {
                    // A deferred decision always carries the queued run's
                    // snapshot; its absence is a core invariant violation.
                    return Err(ProductSurfaceError::internal_invariant());
                };
                // Deferred means steered: the message is queued as steering
                // input for the active run and drains at its next step, so
                // the notice must not ask for a resend.
                let notice = match busy.status {
                    TurnStatus::BlockedApproval => {
                        "Sent to the running turn; it continues once the open approval is answered."
                    }
                    TurnStatus::BlockedAuth => {
                        "Sent to the running turn; it continues once authentication completes."
                    }
                    _ => "Sent to the running turn; the agent reads it at its next step.",
                };
                Ok(RebornSubmitTurnResponse::DeferredBusy {
                    thread_id: thread_id.clone(),
                    accepted_message_ref,
                    active_run_id,
                    status: busy.status,
                    event_cursor: busy.event_cursor,
                    notice: notice.to_string(),
                })
            }
            ProductInboundAck::Rejected(rejection) => Err(session_rejection_error(&rejection)),
            ProductInboundAck::CommandResult { .. }
            | ProductInboundAck::NoOp
            | ProductInboundAck::Duplicate { .. } => Err(ProductSurfaceError::internal_invariant()),
        }
    }

    pub async fn cancel_run(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductCancelRunRequest,
    ) -> Result<RebornCancelRunResponse, ProductSurfaceError> {
        let caller_for_fallback = caller.clone();
        let command = request.into_command(caller)?;
        let ProductInboundCommand::CancelRun { mut request } = command else {
            return Err(ProductSurfaceError::internal_invariant());
        };
        // Ownership probe with automation-trigger fallback. If the thread is a
        // trigger-fired thread belonging to the caller's automation, the probe
        // succeeds and returns the trigger-owned scope/actor so the cancel
        // arrives at the actual run, not the browser caller's session scope.
        let access = self
            .resolve_thread_access_for_caller(
                caller_for_fallback,
                request.scope.clone(),
                &request.actor,
            )
            .await?;
        request.scope = access.scope;
        request.actor = access.run_actor;
        let response = self
            .turn_coordinator
            .cancel_run(request)
            .await
            .map_err(map_turn_error)?;
        Ok(types::reborn_cancel_run_response(response))
    }

    pub async fn retry_run(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductRetryRunRequest,
    ) -> Result<RebornRetryRunResponse, ProductSurfaceError> {
        let caller_for_fallback = caller.clone();
        let command = request.into_command(caller)?;
        let ProductInboundCommand::RetryRun {
            scope,
            actor,
            run_id,
            client_action_id,
        } = command
        else {
            return Err(ProductSurfaceError::internal_invariant());
        };

        let access = self
            .resolve_thread_access_for_caller(caller_for_fallback, scope, &actor)
            .await?;
        // Serialize retry admission with thread deletion. `delete_thread` holds
        // this same per-thread lock across its active-run probe + delete; taking
        // it here closes the window where a concurrent delete passes its probe
        // (the failed run is terminal) and then deletes the thread while
        // `retry_turn` enqueues a replacement run against it.
        let _thread_operation_guard = self.lock_thread_operation(&access.scope).await;
        let response = self
            .turn_coordinator
            .retry_turn(RetryTurnRequest {
                scope: access.scope,
                actor: access.run_actor,
                run_id,
                idempotency_key: client_action_id,
            })
            .await
            .map_err(map_turn_error)?;
        Ok(types::reborn_retry_run_response(response))
    }

    pub async fn get_run_state(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornGetRunStateRequest,
    ) -> Result<RebornGetRunStateResponse, ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let run_id = parse_run_id_field("run_id", request.run_id)?;
        let scope = caller.turn_scope(thread_id);
        let actor = caller.actor();
        // Ownership probe with automation-trigger fallback. Without this gate
        // any caller sharing (tenant, agent, project) could read another user's
        // run state by guessing thread_id and run_id. The fallback also allows
        // the owner of an automation to poll run state on a trigger-fired thread.
        let access = self
            .resolve_thread_access_for_caller(caller, scope, &actor)
            .await?;
        let state = self
            .turn_coordinator
            .get_run_state(GetRunStateRequest {
                scope: access.scope,
                run_id,
            })
            .await
            .map_err(map_turn_error)?;
        // Price a default-model run (no `resolved_model_route`) against the
        // runtime's live active model. Cheap synchronous read; `None` when no
        // reader is wired or no concrete model is configured, in which case the
        // run reports usage without cost (unchanged behavior).
        let active_model = self
            .active_model_reader
            .as_ref()
            .and_then(|reader| reader.active_model_id());
        Ok(RebornGetRunStateResponse::from_run_state(
            state,
            active_model.as_deref(),
        ))
    }
}
