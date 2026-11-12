use super::*;

impl HostRuntimeLoopCapabilityPort {
    pub(super) async fn invoke_auth_decline_dispatch(
        &self,
        request: LoopRequest,
        invocation_id: InvocationId,
    ) -> Result<GatedResolution, AgentLoopHostError> {
        let diagnostic = self.auth_resume_diagnostic(&request).await;
        let idempotency_key = auth_decline_idempotency_key(
            &self.run_context,
            request.activity_id,
            invocation_id,
            &request.capability_id,
        )?;
        loop {
            match self.reserve_dispatch(&idempotency_key, invocation_id)? {
                DispatchReservation::Reserved => break,
                DispatchReservation::Wait(notify) => {
                    self.wait_for_dispatch_completion(&idempotency_key, notify)
                        .await?;
                }
                DispatchReservation::RuntimeCompleted {
                    invocation_id,
                    correlation_id,
                    requested_capability_id,
                    outcome,
                } => {
                    return self
                        .finish_auth_decline_outcome(
                            &idempotency_key,
                            RuntimeOutcomeConversion {
                                input_ref: &request.input_ref,
                                invocation_id,
                                correlation_id,
                                requested_capability_id: &requested_capability_id,
                                outcome,
                            },
                            diagnostic.as_ref(),
                        )
                        .await;
                }
                DispatchReservation::TerminalMilestonePending {
                    invocation_id,
                    result,
                    milestone,
                } => {
                    return self
                        .complete_terminal_milestone(
                            &idempotency_key,
                            invocation_id,
                            result,
                            Some(milestone),
                        )
                        .await;
                }
                DispatchReservation::LoopCompleted(result) => return result,
            }
        }

        let guard = self.dispatch_reservation_guard(&idempotency_key);
        let invocation_context = auth_decline_context_from_visible(
            &self.visible_request.context,
            &self.run_context,
            request.activity_id,
        )?;
        let correlation_id = invocation_context.correlation_id;
        let requested_capability_id = request.capability_id.clone();
        self.result_writer.record_running_invocation(
            &self.run_context,
            invocation_id,
            &request.input_ref,
        );
        let activity_id = CapabilityActivityId::from_uuid(invocation_id.as_uuid());
        self.emit_capability_milestone(LoopHostMilestoneKind::CapabilityInvoked {
            activity_id,
            capability_id: requested_capability_id.clone(),
        })
        .await?;

        // Chain-boxing: each port delegation is boxed so the stacked
        // decorator chain never compiles into a single oversized poll
        // frame (see reborn_integration_model_recovery stack-overflow).
        let outcome = match Box::pin(dispatch_runtime_capability_auth_decline(
            self.runtime.as_ref(),
            invocation_context,
            request.capability_id,
        ))
        .await
        {
            Ok(outcome) => outcome,
            Err(error @ HostRuntimeError::Unavailable { .. }) => {
                return Err(host_runtime_error(error));
            }
            Err(error) => {
                let host_error = host_runtime_error(error);
                let milestone = LoopHostMilestoneKind::CapabilityFailed {
                    activity_id,
                    capability_id: requested_capability_id,
                    provider: None,
                    runtime: None,
                    reason_kind: host_error.kind.failure_kind(),
                    safe_summary: None,
                };
                guard.commit();
                return self
                    .complete_terminal_milestone(
                        &idempotency_key,
                        invocation_id,
                        Err(host_error),
                        Some(milestone),
                    )
                    .await;
            }
        };
        guard.commit();
        self.finish_auth_decline_outcome(
            &idempotency_key,
            RuntimeOutcomeConversion {
                input_ref: &request.input_ref,
                invocation_id,
                correlation_id,
                requested_capability_id: &requested_capability_id,
                outcome,
            },
            diagnostic.as_ref(),
        )
        .await
    }

    /// Best-effort recovery of model-visible auth context. The checkpoint's
    /// typed gate ref is trusted resume routing; this read contributes only
    /// explanatory model context and never authorizes or terminalizes the
    /// invocation, so a missing legacy record cannot strand a user denial.
    pub(super) async fn auth_resume_diagnostic(
        &self,
        request: &LoopRequest,
    ) -> Option<ModelDiagnostic> {
        let loop_gate_ref = &request.auth_resume.as_ref()?.gate_ref;
        let gate_id = loop_gate_ref.as_str().strip_prefix("gate:auth-")?;
        let scope = &self.visible_request.context.resource_scope;
        match self
            .gate_record_store
            .load(scope, GateRef::for_auth_gate(gate_id))
            .await
        {
            Ok(Some(GateRecord::Auth { diagnostic, .. })) => diagnostic,
            Ok(Some(record)) => {
                tracing::warn!(
                    loop_gate_ref = loop_gate_ref.as_str(),
                    record_kind = record.kind(),
                    "auth resume gate ref resolved to a non-auth record; omitting diagnostic"
                );
                None
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    loop_gate_ref = loop_gate_ref.as_str(),
                    error = %error,
                    "failed to load auth gate diagnostic; continuing denial without it"
                );
                None
            }
        }
    }

    pub(super) async fn invoke_capability_dispatch(
        &self,
        request: LoopRequest,
    ) -> Result<GatedResolution, AgentLoopHostError> {
        let requested_invocation_id = InvocationId::from_uuid(request.activity_id.as_uuid());
        if let Some(auth_resume) = request.auth_resume.as_ref().filter(|resume| {
            matches!(
                resume.disposition,
                Some(ironclaw_turns::GateResumeDisposition::Denied { .. })
            )
        }) {
            if request.approval_resume.is_some() {
                return Err(AgentLoopHostError::new(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "capability invocation has both approval_resume and auth_resume set; \
                     these resume modes are mutually exclusive",
                ));
            }
            if auth_resume.prior_approval.is_some() {
                return Err(AgentLoopHostError::new(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "denied capability auth resume must not carry prior approval identity",
                ));
            }
            if !auth_resume.gate_ref.as_str().starts_with("gate:auth-") {
                return Err(AgentLoopHostError::new(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "denied capability auth resume carries a non-auth gate ref",
                ));
            }
            if let Some(resume_token) = auth_resume.resume_token.as_ref() {
                let token_invocation_id = invocation_id_from_resume_token(resume_token)?;
                ensure_resume_invocation_matches_activity(
                    token_invocation_id,
                    requested_invocation_id,
                    "auth denial",
                )?;
            }
            // Chain-boxing: each port delegation is boxed so the stacked
            // decorator chain never compiles into a single oversized poll
            // frame (see reborn_integration_model_recovery stack-overflow).
            return Box::pin(self.invoke_auth_decline_dispatch(request, requested_invocation_id))
                .await;
        }
        // Normalize resume mode and validate token/activity identity before
        // dispatch reservation. Cached replay branches can return without
        // touching runtime state, so they must pass the same fail-closed checks
        // as fresh dispatch.
        enum ResolvedResumeMode<'a> {
            Approval {
                resume: &'a CapabilityApprovalResume,
                invocation_id: InvocationId,
            },
            Auth {
                resume: &'a CapabilityAuthResume,
                invocation_id: InvocationId,
            },
            None,
        }
        let resume_mode = match (
            request.approval_resume.as_ref(),
            request.auth_resume.as_ref(),
        ) {
            (Some(_), Some(_)) => {
                return Err(AgentLoopHostError::new(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "capability invocation has both approval_resume and auth_resume set; \
                     these resume modes are mutually exclusive",
                ));
            }
            (Some(resume), _) => {
                let resume_invocation_id = invocation_id_from_resume_token(&resume.resume_token)?;
                ensure_resume_invocation_matches_activity(
                    resume_invocation_id,
                    requested_invocation_id,
                    "approval",
                )?;
                ResolvedResumeMode::Approval {
                    resume,
                    invocation_id: resume_invocation_id,
                }
            }
            (_, Some(auth_resume)) => {
                let resume_token = auth_resume.resume_token.as_ref().ok_or_else(|| {
                    AgentLoopHostError::new(
                        AgentLoopHostErrorKind::InvalidInvocation,
                        "resolved capability auth resume is missing its resume token",
                    )
                })?;
                let resume_invocation_id = invocation_id_from_resume_token(resume_token)?;
                ensure_resume_invocation_matches_activity(
                    resume_invocation_id,
                    requested_invocation_id,
                    "auth",
                )?;
                ResolvedResumeMode::Auth {
                    resume: auth_resume,
                    invocation_id: resume_invocation_id,
                }
            }
            (Option::None, Option::None) => ResolvedResumeMode::None,
        };
        // Host-side resume reconstitution (hazard 3, §5.3 Stage 0): on a resume the
        // effective input_ref used for the idempotency key + validation is derived
        // from the host-private payload persisted at the FRESH gate raise — loaded
        // by the resume's invocation id — NOT from the advisory loop-supplied
        // `resume.input_ref`. `resume_mode` above already validated token identity
        // and the resume→activity match, so that precedence still runs before the
        // registered-activity check below; the payload load fails CLOSED on a miss
        // (§5.3 Stage 2a-i). The same payload is reused for `{input, estimate}`
        // below, so a resume loads it exactly once here.
        let resume_payload = match &resume_mode {
            ResolvedResumeMode::Approval { invocation_id, .. }
            | ResolvedResumeMode::Auth { invocation_id, .. } => {
                // Chain-boxing: each port delegation is boxed so the stacked
                // decorator chain never compiles into a single oversized poll
                // frame (see reborn_integration_model_recovery stack-overflow).
                Some(Box::pin(self.replay_payload_for_resume(*invocation_id)).await?)
            }
            ResolvedResumeMode::None => Option::None,
        };
        // Owned clone so `effective_input_ref` borrows this local, not
        // `resume_payload` — the payload is consumed for `{input, estimate}` below.
        let resume_input_ref = resume_payload
            .as_ref()
            .map(|payload| payload.input_ref.clone());
        let effective_input_ref = resume_input_ref.as_ref().unwrap_or(&request.input_ref);
        // Host-side resume reconstitution of the correlation identity (§5.3 Stage
        // 2a-i, mirroring `input_ref`): the loop-facing `Resolution` no longer
        // carries the original `correlation_id` (it is minted fresh at the loop
        // boundary post-flip), so the authoritative one — the identity the
        // fingerprinted approval lease is scoped to — is reconstituted here from
        // the host-private replay payload persisted at the fresh gate raise. Using
        // the loop's advisory value instead would fail the lease's correlation
        // match ("approval request does not match invocation: correlation_id").
        let resume_correlation_id = resume_payload
            .as_ref()
            .map(|payload| payload.correlation_id);
        self.validate_provider_tool_call_registration_activity(
            effective_input_ref,
            request.activity_id,
        )?;
        let snapshot = self.snapshot_for(&request.surface_version)?;
        let Some(capability) = snapshot.capabilities.get(&request.capability_id).cloned() else {
            return Ok(GatedResolution::bare(
                resolution::denied(
                    capability_denied_reason_kind("outside_visible_surface")?,
                    "capability was not visible on the cited surface".to_string(),
                )
                .resolution,
            ));
        };
        let idempotency_key =
            invocation_idempotency_key(&self.run_context, &request, effective_input_ref)?;
        let requested_invocation_id = InvocationId::from_uuid(request.activity_id.as_uuid());
        loop {
            match self.reserve_dispatch(&idempotency_key, requested_invocation_id)? {
                DispatchReservation::Reserved => break,
                DispatchReservation::Wait(notify) => {
                    self.wait_for_dispatch_completion(&idempotency_key, notify)
                        .await?;
                }
                DispatchReservation::RuntimeCompleted {
                    invocation_id,
                    correlation_id,
                    requested_capability_id,
                    outcome,
                } => {
                    if let SurfaceCapabilitySnapshot::Runtime(capability) = &capability {
                        return self
                            .finish_runtime_outcome(
                                &idempotency_key,
                                RuntimeOutcomeCompletion {
                                    input_ref: effective_input_ref,
                                    invocation_id,
                                    correlation_id,
                                    requested_capability_id: &requested_capability_id,
                                    provider: capability.provider.clone(),
                                    runtime: capability.runtime,
                                    outcome,
                                },
                            )
                            .await;
                    }
                    let result = runtime_outcome_to_loop(
                        &self.run_context,
                        self.result_writer.as_ref(),
                        RuntimeOutcomeConversion {
                            input_ref: effective_input_ref,
                            invocation_id,
                            correlation_id,
                            requested_capability_id: &requested_capability_id,
                            outcome,
                        },
                    )
                    .await;
                    self.record_loop_completed(&idempotency_key, invocation_id, result.clone())?;
                    return result;
                }
                DispatchReservation::TerminalMilestonePending {
                    invocation_id,
                    result,
                    milestone,
                } => {
                    return self
                        .complete_terminal_milestone(
                            &idempotency_key,
                            invocation_id,
                            result,
                            Some(milestone),
                        )
                        .await;
                }
                DispatchReservation::LoopCompleted(result) => return result,
            }
        }

        // Any early `?` between reservation and `finish_runtime_outcome` unwinds
        // the in-flight reservation via the guard's `Drop`. The success path
        // calls `guard.commit()` so the dispatch record is replaced by
        // `finish_runtime_outcome` rather than cleared.
        let guard = self.dispatch_reservation_guard(&idempotency_key);

        let capability = match capability {
            SurfaceCapabilitySnapshot::Runtime(capability) => capability,
            SurfaceCapabilitySnapshot::Synthetic(capability) => {
                let result = self
                    .invoke_synthetic_capability(request, capability, snapshot)
                    .await;
                if result.is_ok() {
                    guard.commit();
                    self.record_loop_completed(
                        &idempotency_key,
                        requested_invocation_id,
                        result.clone(),
                    )?;
                }
                return result;
            }
        };

        let Some(trust_decision) = self
            .visible_request
            .provider_trust
            .get(&capability.provider)
            .cloned()
        else {
            return Ok(GatedResolution::bare(
                resolution::denied(
                    capability_denied_reason_kind("missing_provider_trust")?,
                    "capability provider trust is unavailable".to_string(),
                )
                .resolution,
            ));
        };
        let (input, estimate) = match resume_payload {
            // Host-side resume replay: reconstitute {input, estimate} from the
            // host-private payload loaded up front (keyed by the resume's
            // invocation id). `Some` iff this is a resume; a missing payload
            // already failed CLOSED above — never a silent empty-input dispatch
            // (arch-simplification §5.3 Stage 2a-i).
            Some(payload) => (payload.input, payload.estimate),
            Option::None => {
                let input = self
                    .input_resolver
                    .resolve_capability_input(&self.run_context, effective_input_ref)
                    .await?;
                // Trajectory capture: the resolved input is the model's tool
                // arguments, and this is the one place they are visible (the provider
                // tool-call decorator stages them upstream and bypasses the input
                // resolver hook).
                if let Some(observer) = &self.trajectory_observer {
                    // Best-effort, inline on the capability hot path: a panicking
                    // observer must never unwind the invocation before dispatch.
                    // (Blocking is the observer's own contract.)
                    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        observer.on_capability_input(
                            effective_input_ref.as_str(),
                            request.capability_id.as_str(),
                            &input,
                        );
                    }));
                    if caught.is_err() {
                        tracing::warn!(
                            capability_id = request.capability_id.as_str(),
                            "trajectory observer on_capability_input panicked; dropping event"
                        );
                    }
                }
                let input = match prepare_provider_arguments_with_detail(
                    &input,
                    &capability.parameters_schema,
                    "capability input",
                ) {
                    Ok(input) => input,
                    Err(error)
                        if error.error.kind == AgentLoopHostErrorKind::InvalidInvocation
                            && is_provider_tool_call_input_ref(effective_input_ref) =>
                    {
                        let host_error = *error.error;
                        let detail = error.detail.unwrap_or_else(|| {
                            diagnostic_detail_from_raw(&host_error.safe_summary)
                        });
                        let result = Ok(GatedResolution::bare(resolution::failed(
                            FailureKind::InputEncode,
                            host_error.safe_summary,
                            detail,
                        )));
                        guard.commit();
                        self.record_loop_completed(
                            &idempotency_key,
                            requested_invocation_id,
                            result.clone(),
                        )?;
                        return result;
                    }
                    Err(error) => return Err(*error.error),
                };
                // Runtime-specific request-shape validation belongs to the host
                // runtime. In particular, process-sandbox spawn and resume paths
                // return malformed plans as model-visible `InvalidInput` failures;
                // the mapper below then applies the canonical diagnostic scrubber.
                (input, capability.estimate.clone())
            }
        };
        let mut invocation_context =
            invocation_context_from_visible(VisibleInvocationContextRequest {
                base: &self.visible_request.context,
                run_context: &self.run_context,
                activity_id: request.activity_id,
                capability_id: &request.capability_id,
                capability: &capability,
                trust: trust_decision.effective_trust.class(),
                allowed_effects: &trust_decision.authority_ceiling.allowed_effects,
                execution_mounts: self.execution_mounts_for(&request.capability_id),
            })?;
        match &resume_mode {
            ResolvedResumeMode::Approval {
                resume,
                invocation_id: resume_invocation_id,
            } => {
                invocation_context.invocation_id = *resume_invocation_id;
                // Prefer the host-reconstituted correlation identity (the one the
                // approval lease is scoped to); the loop DTO's is advisory post-flip.
                invocation_context.correlation_id =
                    resume_correlation_id.unwrap_or(resume.correlation_id);
                invocation_context.resource_scope.invocation_id = *resume_invocation_id;
                invocation_context.validate().map_err(|_| {
                    AgentLoopHostError::new(
                        AgentLoopHostErrorKind::InvalidInvocation,
                        "capability approval resume context is invalid",
                    )
                })?;
            }
            ResolvedResumeMode::Auth {
                resume: auth_resume,
                invocation_id: resume_invocation_id,
            } => {
                // Reuse original invocation identifier so the fingerprinted
                // approval lease (scoped to that identifier) can still be matched
                // and claimed.
                invocation_context.invocation_id = *resume_invocation_id;
                invocation_context.resource_scope.invocation_id = *resume_invocation_id;
                // Restore the original correlation identifier so it flows through the
                // full capability lifecycle and matches any fingerprinted lease.
                // Prefer the host-reconstituted value from the replay payload (§5.3
                // Stage 2a-i); fall back to the wire prior-approval identity (kept on
                // the wire this slice, Stage 2a-ii).
                if let Some(correlation_id) = resume_correlation_id {
                    invocation_context.correlation_id = correlation_id;
                } else if let Some(pa) = auth_resume.prior_approval.as_ref() {
                    invocation_context.correlation_id = pa.correlation_id;
                }
                invocation_context.validate().map_err(|_| {
                    AgentLoopHostError::new(
                        AgentLoopHostErrorKind::InvalidInvocation,
                        "capability auth resume context is invalid",
                    )
                })?;
            }
            ResolvedResumeMode::None => {}
        }
        let invocation_id = invocation_context.invocation_id;
        let correlation_id = invocation_context.correlation_id;
        let requested_capability_id = request.capability_id.clone();
        let provider = capability.provider.clone();
        let runtime = capability.runtime;
        // Link this invocation to its staged input ref now that both are known,
        // so the still-running activity frame can surface the input argument
        // before the result completes.
        self.result_writer.record_running_invocation(
            &self.run_context,
            invocation_id,
            effective_input_ref,
        );
        let capability_activity_id = CapabilityActivityId::from_uuid(invocation_id.as_uuid());
        // Chain-boxing: each port delegation is boxed so the stacked
        // decorator chain never compiles into a single oversized poll
        // frame (see reborn_integration_model_recovery stack-overflow).
        Box::pin(
            self.emit_capability_milestone(LoopHostMilestoneKind::CapabilityInvoked {
                activity_id: capability_activity_id,
                capability_id: request.capability_id.clone(),
            }),
        )
        .await?;
        // Only a FRESH dispatch mints a replay payload; an approval/auth resume
        // reuses the invocation id and its already-persisted payload (write-once),
        // so re-persisting would collide. Captured before `resume_mode` is
        // consumed by the dispatch match below.
        let is_fresh_dispatch = matches!(resume_mode, ResolvedResumeMode::None);
        let outcome = match resume_mode {
            ResolvedResumeMode::Approval { resume, .. } => {
                // Chain-boxing: each port delegation is boxed so the stacked
                // decorator chain never compiles into a single oversized poll
                // frame (see reborn_integration_model_recovery stack-overflow).
                Box::pin(dispatch_runtime_capability_resume(
                    self.runtime.as_ref(),
                    invocation_context,
                    resume.approval_request_id,
                    request.capability_id,
                    estimate.clone(),
                    input.clone(),
                ))
                .await
            }
            ResolvedResumeMode::Auth {
                resume: auth_resume,
                ..
            } => {
                let prior_approval_id = auth_resume
                    .prior_approval
                    .as_ref()
                    .map(|pa| pa.approval_request_id);
                tracing::debug!(
                    invocation_id = %invocation_id,
                    auth_resume = true,
                    approval_request_id = prior_approval_id.map(|id| id.to_string()).as_deref().unwrap_or("none"),
                    "capability auth-resume re-dispatch with preserved invocation identity"
                );
                // Chain-boxing: each port delegation is boxed so the stacked
                // decorator chain never compiles into a single oversized poll
                // frame (see reborn_integration_model_recovery stack-overflow).
                Box::pin(dispatch_runtime_capability_auth_resume(
                    self.runtime.as_ref(),
                    invocation_context,
                    request.capability_id,
                    estimate.clone(),
                    input.clone(),
                    prior_approval_id,
                ))
                .await
            }
            ResolvedResumeMode::None => {
                // Chain-boxing: each port delegation is boxed so the stacked
                // decorator chain never compiles into a single oversized poll
                // frame (see reborn_integration_model_recovery stack-overflow).
                Box::pin(dispatch_runtime_capability(
                    self.runtime.as_ref(),
                    invocation_context,
                    request.capability_id,
                    estimate.clone(),
                    input.clone(),
                ))
                .await
            }
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(HostRuntimeError::Unavailable { reason }) => {
                runtime_failed_outcome_for_host_runtime_unavailable(
                    requested_capability_id.clone(),
                    reason,
                )
            }
            Err(error) => {
                let host_error = host_runtime_error(error);
                let terminal_milestone = LoopHostMilestoneKind::CapabilityFailed {
                    activity_id: capability_activity_id,
                    capability_id: requested_capability_id.clone(),
                    provider: Some(provider),
                    runtime: Some(runtime),
                    reason_kind: host_error.kind.failure_kind(),
                    // Host/infra fault, not a model-visible tool error: keep the
                    // detail server-side, surface only the kind.
                    safe_summary: None,
                };
                guard.commit();
                return self
                    .complete_terminal_milestone(
                        &idempotency_key,
                        invocation_id,
                        Err(host_error),
                        Some(terminal_milestone),
                    )
                    .await;
            }
        };
        // Persist the host-private replay payload BEFORE returning the gate to the
        // loop, so a later resume turn can reconstitute {input, estimate} host-side
        // without the loop carrying raw tool args (arch-simplification §5.3 Stage
        // 2a-i; charter: agent-loop state never stores raw tool args). No-op unless
        // this is a fresh dispatch that produced an approval/auth gate.
        //
        // The dispatch reservation is committed only AFTER this fallible store
        // write succeeds (#6287 IronLoop): committing before it means a transient
        // store error would `?`-return with the reservation still `InFlight` and
        // the committed guard skipping its cleanup, stranding retries/duplicates
        // waiting on the key forever. On error the uncommitted guard clears the
        // reservation and wakes waiters so one re-dispatches.
        if is_fresh_dispatch {
            self.persist_replay_payload_for_fresh_gate(
                invocation_id,
                effective_input_ref,
                &input,
                &estimate,
                correlation_id,
                &outcome,
            )
            .await?;
        }
        guard.commit();
        self.finish_runtime_outcome(
            &idempotency_key,
            RuntimeOutcomeCompletion {
                input_ref: effective_input_ref,
                invocation_id,
                correlation_id,
                requested_capability_id: &requested_capability_id,
                provider,
                runtime,
                outcome,
            },
        )
        .await
    }
}
