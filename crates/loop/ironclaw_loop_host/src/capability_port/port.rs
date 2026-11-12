use super::*;

pub struct HostRuntimeLoopCapabilityPort {
    pub(super) runtime: Arc<dyn HostRuntime>,
    pub(super) run_context: LoopRunContext,
    pub(super) visible_request: ironclaw_host_runtime::VisibleCapabilityRequest,
    pub(super) input_resolver: Arc<dyn LoopCapabilityInputResolver>,
    pub(super) result_writer: Arc<dyn LoopCapabilityResultWriter>,
    pub(super) milestone_sink: Arc<dyn LoopHostMilestoneSink>,
    pub(super) execution_mounts: MountView,
    pub(super) capability_execution_mounts: HashMap<CapabilityId, MountView>,
    pub(super) snapshots: Mutex<HashMap<String, SurfaceSnapshot>>,
    pub(super) current_surface_version: Mutex<Option<String>>,
    pub(super) dispatch_records: Mutex<DispatchRecordStore>,
    pub(super) provider_tool_call_registrations: Mutex<ProviderToolCallRegistrationStore>,
    pub(super) trajectory_observer: Option<Arc<dyn CapabilityTrajectoryObserver>>,
    /// Durable store for the model-visible [`GateRecord`] a pending gate renders
    /// from on a later resume turn (§5.2.9). Written at the capability seam when a
    /// gate/suspension outcome is produced; see `persist_gate_record_for_mapped`.
    pub(super) gate_record_store: Arc<dyn GateRecordStorePort>,
    /// Host-private store for the raw replay payload (tool `input` + `estimate`)
    /// a gate/auth resume re-dispatches from (arch-simplification §5.3 Stage
    /// 2a-i). Written at a FRESH gate raise keyed by `InvocationId`
    /// (`persist_replay_payload_for_fresh_gate`); loaded on resume by the
    /// invocation id recovered from the resume token
    /// (`replay_payload_for_resume`). Never model-visible.
    pub(super) replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
    /// Per-idempotency-key reservation for a gate outcome's persisted
    /// [`Resolution`]. The mapping mints a fresh random `GateRef` per call for
    /// the approval/resource/dependent/external channels, so a replayed
    /// invocation (same key) must return the FIRST invocation's resolution — the
    /// one whose gate ref the record is under — not a freshly-minted ref no
    /// record exists under (#6287). Exactly one caller (the owner) persists the
    /// record; concurrent duplicates and later replays WAIT on the reservation's
    /// notify for that durable save before receiving the resolution, so a
    /// concurrent replay never receives a blocked resolution whose record is not
    /// yet persisted. A failed save clears the reservation and wakes the waiters
    /// so one of them re-owns and retries.
    pub(super) persisted_gate_resolutions: Mutex<HashMap<IdempotencyKey, GateResolutionState>>,
}

/// Reservation state for a gate outcome's persisted resolution, keyed by
/// idempotency key. Mirrors the `InFlight`/completed shape of
/// [`DispatchRecord`] so the wait is the same lost-wakeup-safe pattern as
/// [`HostRuntimeLoopCapabilityPort::wait_for_dispatch_completion`].
pub(super) enum GateResolutionState {
    /// The owning invocation is persisting the record. Waiters block on the
    /// notify until it either publishes `Persisted` or clears the entry.
    InFlight(Arc<Notify>),
    /// The record is durably persisted; the resolution is safe to hand back to
    /// a replayed invocation. Boxed so this variant does not dominate the map
    /// entry's size over the pointer-sized `InFlight`.
    Persisted(Box<Resolution>),
}

/// Lock a poisoned-aware `Mutex` and wrap a poison error as the canonical
/// "<label> is unavailable" host error. Every store in this module is reached
/// via this helper so the error message stays consistent and the call sites
/// shrink to one line.
pub(super) fn lock_mut<'a, T>(
    mutex: &'a Mutex<T>,
    label: &'static str,
) -> Result<std::sync::MutexGuard<'a, T>, AgentLoopHostError> {
    mutex.lock().map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::Unavailable,
            format!("{label} is unavailable"),
        )
    })
}

impl HostRuntimeLoopCapabilityPort {
    pub fn new(
        runtime: Arc<dyn HostRuntime>,
        run_context: LoopRunContext,
        visible_request: ironclaw_host_runtime::VisibleCapabilityRequest,
        input_resolver: Arc<dyn LoopCapabilityInputResolver>,
        result_writer: Arc<dyn LoopCapabilityResultWriter>,
        milestone_sink: Arc<dyn LoopHostMilestoneSink>,
    ) -> Self {
        let input_resolver: Arc<dyn LoopCapabilityInputResolver> =
            Arc::new(ProviderToolCallInputResolver::new(input_resolver));
        Self {
            runtime,
            run_context,
            visible_request,
            input_resolver,
            result_writer,
            milestone_sink,
            execution_mounts: MountView::default(),
            capability_execution_mounts: HashMap::new(),
            snapshots: Mutex::new(HashMap::new()),
            current_surface_version: Mutex::new(None),
            dispatch_records: Mutex::new(DispatchRecordStore::default()),
            provider_tool_call_registrations: Mutex::new(
                ProviderToolCallRegistrationStore::default(),
            ),
            trajectory_observer: None,
            // Transitional no-op default; composition wires the durable store
            // through the factory's `with_gate_record_store`, which forwards via
            // the port-level builder below. See `NoopGateRecordStore`.
            gate_record_store: Arc::new(NoopGateRecordStore),
            // Transitional fail-closed default; composition wires the durable
            // store through the factory's `with_replay_payload_store`. See
            // `NoopReplayPayloadStore`.
            replay_payload_store: Arc::new(NoopReplayPayloadStore),
            persisted_gate_resolutions: Mutex::new(HashMap::new()),
        }
    }

    /// Wire the durable [`GateRecordStorePort`] this port persists pending-gate
    /// records into (§5.2.9). Defaults to the transitional
    /// [`NoopGateRecordStore`] when unset.
    pub fn with_gate_record_store(mut self, store: Arc<dyn GateRecordStorePort>) -> Self {
        self.gate_record_store = store;
        self
    }

    /// Wire the durable host-private [`ReplayPayloadStorePort`] this port persists
    /// gate/auth replay payloads into and reconstitutes them from on resume
    /// (arch-simplification §5.3 Stage 2a-i). Defaults to the transitional
    /// fail-closed [`NoopReplayPayloadStore`] when unset.
    pub fn with_replay_payload_store(mut self, store: Arc<dyn ReplayPayloadStorePort>) -> Self {
        self.replay_payload_store = store;
        self
    }

    /// Attach a [`CapabilityTrajectoryObserver`] notified of each capability's
    /// resolved input as this port executes it. No-op when unset.
    pub fn with_trajectory_observer(
        mut self,
        observer: Option<Arc<dyn CapabilityTrajectoryObserver>>,
    ) -> Self {
        self.trajectory_observer = observer;
        self
    }

    pub fn with_execution_mounts(mut self, mounts: MountView) -> Self {
        self.execution_mounts = mounts;
        self
    }

    pub fn with_capability_execution_mounts(
        mut self,
        mounts: HashMap<CapabilityId, MountView>,
    ) -> Self {
        self.capability_execution_mounts = mounts;
        self
    }

    pub(super) fn execution_mounts_for(&self, capability_id: &CapabilityId) -> &MountView {
        self.capability_execution_mounts
            .get(capability_id)
            .unwrap_or(&self.execution_mounts)
    }

    pub(super) fn snapshot_for(
        &self,
        version: &ironclaw_loop_contracts::CapabilitySurfaceVersion,
    ) -> Result<SurfaceSnapshot, AgentLoopHostError> {
        let snapshots = lock_mut(&self.snapshots, "capability surface snapshot store")?;
        snapshots.get(version.as_str()).cloned().ok_or_else(|| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::StaleSurface,
                "capability surface is stale or unknown",
            )
        })
    }

    pub(super) fn current_snapshot(
        &self,
    ) -> Result<Option<(String, SurfaceSnapshot)>, AgentLoopHostError> {
        let snapshots = lock_mut(&self.snapshots, "capability surface snapshot store")?;
        let version = lock_mut(
            &self.current_surface_version,
            "capability surface snapshot pointer",
        )?
        .clone();
        let Some(version) = version else {
            return Ok(None);
        };
        let snapshot = snapshots.get(&version).cloned().ok_or_else(|| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::StaleSurface,
                "current capability surface snapshot is unavailable",
            )
        })?;
        Ok(Some((version, snapshot)))
    }

    pub(super) fn reserve_dispatch(
        &self,
        key: &IdempotencyKey,
        requested_invocation_id: InvocationId,
    ) -> Result<DispatchReservation, AgentLoopHostError> {
        lock_mut(&self.dispatch_records, "capability dispatch record store")?
            .reserve(key, requested_invocation_id)
    }

    pub(super) fn dispatch_in_flight_matches(
        &self,
        key: &IdempotencyKey,
        notify: &Arc<Notify>,
    ) -> Result<bool, AgentLoopHostError> {
        Ok(
            lock_mut(&self.dispatch_records, "capability dispatch record store")?
                .in_flight_matches(key, notify),
        )
    }

    pub(super) fn record_runtime_completed(
        &self,
        key: &IdempotencyKey,
        invocation_id: InvocationId,
        correlation_id: CorrelationId,
        requested_capability_id: CapabilityId,
        outcome: RuntimeCapabilityOutcome,
    ) -> Result<(), AgentLoopHostError> {
        let notify = lock_mut(&self.dispatch_records, "capability dispatch record store")?.record(
            key,
            DispatchRecord::RuntimeCompleted {
                invocation_id,
                correlation_id,
                requested_capability_id,
                outcome,
            },
        );
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        Ok(())
    }

    pub(super) fn record_terminal_milestone_pending(
        &self,
        key: &IdempotencyKey,
        invocation_id: InvocationId,
        result: Result<GatedResolution, AgentLoopHostError>,
        milestone: LoopHostMilestoneKind,
    ) -> Result<(), AgentLoopHostError> {
        let notify = lock_mut(&self.dispatch_records, "capability dispatch record store")?.record(
            key,
            DispatchRecord::TerminalMilestonePending {
                invocation_id,
                result,
                milestone,
            },
        );
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        Ok(())
    }

    pub(super) fn record_loop_completed(
        &self,
        key: &IdempotencyKey,
        invocation_id: InvocationId,
        result: Result<GatedResolution, AgentLoopHostError>,
    ) -> Result<(), AgentLoopHostError> {
        let notify = lock_mut(&self.dispatch_records, "capability dispatch record store")?.record(
            key,
            DispatchRecord::LoopCompleted {
                invocation_id,
                result,
            },
        );
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        Ok(())
    }

    pub(super) fn clear_dispatch(&self, key: &IdempotencyKey) -> Result<(), AgentLoopHostError> {
        let notify =
            lock_mut(&self.dispatch_records, "capability dispatch record store")?.remove(key);
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        Ok(())
    }

    pub(super) fn record_provider_tool_call_registration(
        &self,
        input_ref: &CapabilityInputRef,
        capability_id: &CapabilityId,
        activity_id: Option<CapabilityActivityId>,
        effective_capability_ids: Option<HashSet<CapabilityId>>,
    ) -> Result<CapabilityActivityId, AgentLoopHostError> {
        lock_mut(
            &self.provider_tool_call_registrations,
            "provider tool-call registration store",
        )?
        .record(
            input_ref,
            capability_id,
            activity_id,
            effective_capability_ids,
        )
    }

    pub(super) fn provider_tool_call_registration_for(
        &self,
        input_ref: &CapabilityInputRef,
    ) -> Result<Option<ProviderToolCallRegistrationRecord>, AgentLoopHostError> {
        Ok(lock_mut(
            &self.provider_tool_call_registrations,
            "provider tool-call registration store",
        )?
        .registration_for(input_ref))
    }

    pub(super) fn validate_provider_tool_call_registration_activity(
        &self,
        input_ref: &CapabilityInputRef,
        activity_id: CapabilityActivityId,
    ) -> Result<(), AgentLoopHostError> {
        if let Some(registration) = self.provider_tool_call_registration_for(input_ref)?
            && registration.activity_id != activity_id
        {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "registered provider tool-call activity identity does not match the requested activity",
            ));
        }
        Ok(())
    }

    /// Drop guard for an `InFlight` dispatch reservation. Releases the
    /// reservation (and wakes any waiters) unless [`commit`] is called first.
    /// Use after a successful `reserve_dispatch` returns `Reserved` so any
    /// early-return error path between reservation and outcome recording
    /// unwinds the reservation automatically.
    pub(super) fn dispatch_reservation_guard<'a>(
        &'a self,
        key: &IdempotencyKey,
    ) -> DispatchReservationGuard<'a> {
        DispatchReservationGuard {
            port: self,
            key: key.clone(),
            committed: false,
        }
    }

    pub(super) fn validate_visible_request_scope(&self) -> Result<(), AgentLoopHostError> {
        let context = &self.visible_request.context;
        context.validate().map_err(|_| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "capability execution context is invalid",
            )
        })?;
        if context.tenant_id != self.run_context.scope.tenant_id
            || context.agent_id != self.run_context.scope.agent_id
            || context.project_id != self.run_context.scope.project_id
            || context.thread_id.as_ref() != Some(&self.run_context.thread_id)
            || context.resource_scope.tenant_id != self.run_context.scope.tenant_id
            || context.resource_scope.agent_id != self.run_context.scope.agent_id
            || context.resource_scope.project_id != self.run_context.scope.project_id
            || context.resource_scope.thread_id.as_ref() != Some(&self.run_context.thread_id)
        {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::ScopeMismatch,
                "capability execution context is not scoped to this loop run",
            ));
        }
        if context.mounts != MountView::default() {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unauthorized,
                "capability execution context must not carry caller-supplied mounts",
            ));
        }
        Ok(())
    }

    pub(super) async fn finish_runtime_outcome(
        &self,
        key: &IdempotencyKey,
        completion: RuntimeOutcomeCompletion<'_>,
    ) -> Result<GatedResolution, AgentLoopHostError> {
        let result = runtime_outcome_to_loop(
            &self.run_context,
            self.result_writer.as_ref(),
            completion.conversion(),
        )
        .await;
        if should_retry_result_write(&completion.outcome, &result) {
            self.record_runtime_completed(
                key,
                completion.invocation_id,
                completion.correlation_id,
                completion.requested_capability_id.clone(),
                completion.outcome,
            )?;
            return result;
        }
        if result.is_err() {
            self.record_loop_completed(key, completion.invocation_id, result.clone())?;
            return result;
        }
        let terminal_milestone = match runtime_terminal_milestone(
            CapabilityActivityId::from_uuid(completion.invocation_id.as_uuid()),
            completion.provider,
            completion.runtime,
            &completion.outcome,
        ) {
            Ok(milestone) => milestone,
            Err(error) => {
                let result = Err(error);
                self.record_loop_completed(key, completion.invocation_id, result.clone())?;
                return result;
            }
        };
        self.complete_terminal_milestone(key, completion.invocation_id, result, terminal_milestone)
            .await
    }

    pub(super) async fn finish_auth_decline_outcome(
        &self,
        key: &IdempotencyKey,
        conversion: RuntimeOutcomeConversion<'_>,
        diagnostic: Option<&ModelDiagnostic>,
    ) -> Result<GatedResolution, AgentLoopHostError> {
        let RuntimeOutcomeConversion {
            input_ref,
            invocation_id,
            correlation_id,
            requested_capability_id,
            outcome,
        } = conversion;
        let (outcome, failure_capability_id, failure_kind, failure_summary) = match outcome {
            RuntimeCapabilityOutcome::Failed(failure) => {
                let failure = if let Some(diagnostic) = diagnostic {
                    failure.with_model_visible_cause(diagnostic.as_str())
                } else {
                    failure
                };
                let capability_id = failure.capability_id.clone();
                let kind = failure.kind;
                let summary = runtime_failure_loop_safe_summary(&failure);
                (
                    RuntimeCapabilityOutcome::Failed(failure),
                    capability_id,
                    kind,
                    summary,
                )
            }
            _ => {
                let result = Err(AgentLoopHostError::new(
                    AgentLoopHostErrorKind::Internal,
                    "capability auth decline returned a non-terminal runtime outcome",
                ));
                self.record_loop_completed(key, invocation_id, result.clone())?;
                return result;
            }
        };
        let result = runtime_outcome_to_loop(
            &self.run_context,
            self.result_writer.as_ref(),
            RuntimeOutcomeConversion {
                input_ref,
                invocation_id,
                correlation_id,
                requested_capability_id,
                outcome: outcome.clone(),
            },
        )
        .await;
        if should_retry_result_write(&outcome, &result) {
            self.record_runtime_completed(
                key,
                invocation_id,
                correlation_id,
                requested_capability_id.clone(),
                outcome.clone(),
            )?;
            return result;
        }
        if result.is_err() {
            self.record_loop_completed(key, invocation_id, result.clone())?;
            return result;
        }
        let milestone = LoopHostMilestoneKind::CapabilityFailed {
            activity_id: CapabilityActivityId::from_uuid(invocation_id.as_uuid()),
            capability_id: failure_capability_id,
            // The current contract may already be gone. Durable invocation
            // identity, rather than stale provider/runtime metadata, is the
            // authority for this terminal transition.
            provider: None,
            runtime: None,
            reason_kind: failure_kind,
            safe_summary: failure_summary,
        };
        self.complete_terminal_milestone(key, invocation_id, result, Some(milestone))
            .await
    }

    pub(super) async fn complete_terminal_milestone(
        &self,
        key: &IdempotencyKey,
        invocation_id: InvocationId,
        result: Result<GatedResolution, AgentLoopHostError>,
        terminal_milestone: Option<LoopHostMilestoneKind>,
    ) -> Result<GatedResolution, AgentLoopHostError> {
        if let Some(milestone) = terminal_milestone
            && let Err(error) = self.emit_capability_milestone(milestone.clone()).await
        {
            self.record_terminal_milestone_pending(key, invocation_id, result.clone(), milestone)?;
            return Err(error);
        }
        self.record_loop_completed(key, invocation_id, result.clone())?;
        result
    }

    pub(super) async fn wait_for_dispatch_completion(
        &self,
        key: &IdempotencyKey,
        notify: Arc<Notify>,
    ) -> Result<(), AgentLoopHostError> {
        let notified = notify.notified();
        tokio::pin!(notified);
        if self.dispatch_in_flight_matches(key, &notify)? {
            notified.await;
        }
        Ok(())
    }

    pub(super) async fn emit_capability_milestone(
        &self,
        kind: LoopHostMilestoneKind,
    ) -> Result<(), AgentLoopHostError> {
        self.milestone_sink
            .publish_loop_milestone(LoopHostMilestone {
                scope: self.run_context.scope.clone(),
                actor: self.run_context.actor.clone(),
                turn_id: self.run_context.turn_id,
                run_id: self.run_context.run_id,
                loop_driver_id: self.run_context.loop_driver_id.clone(),
                kind,
            })
            .await
    }

    pub(super) async fn invoke_synthetic_capability(
        &self,
        request: LoopRequest,
        capability: SyntheticSurfaceCapabilitySnapshot,
        snapshot: SurfaceSnapshot,
    ) -> Result<GatedResolution, AgentLoopHostError> {
        let input = self
            .input_resolver
            .resolve_capability_input(&self.run_context, &request.input_ref)
            .await?;
        let registration = self.provider_tool_call_registration_for(&request.input_ref)?;
        let effective_capability_ids = registration
            .and_then(|registration| registration.effective_capability_ids)
            .unwrap_or_default();
        let output = match capability.output(&input, |requested| {
            let capability = snapshot.capability_info(requested)?;
            if !effective_capability_ids.contains(capability.capability_id) {
                return None;
            }
            Some(capability)
        }) {
            Ok(output) => output,
            Err(error) if error.kind == AgentLoopHostErrorKind::InvalidInvocation => {
                // Synthetic capability InvalidInvocation errors are model-side input failures
                // such as bad arguments or an unknown capability_info target. Keep those
                // model-visible so the driver can retry instead of terminalizing the host.
                // INVARIANT: synthetic capabilities must not use InvalidInvocation for
                // internal or host-fatal conditions.
                let detail = diagnostic_detail_from_raw(&error.safe_summary);
                return Ok(GatedResolution::bare(resolution::failed(
                    FailureKind::InputEncode,
                    error.safe_summary,
                    detail,
                )));
            }
            Err(error) => return Err(error),
        };
        let write_result = self
            .result_writer
            .write_capability_result(CapabilityResultWrite {
                run_context: &self.run_context,
                input_ref: &request.input_ref,
                invocation_id: InvocationId::from_uuid(request.activity_id.as_uuid()),
                capability_id: &request.capability_id,
                output,
                display_preview: None,
                durable_persistence: DurablePersistence::Persist,
            })
            .await?;
        Ok(GatedResolution::bare(resolution::completed(
            write_result.result_ref,
            "capability info returned".to_string(),
            ironclaw_loop_contracts::CapabilityProgress::MadeProgress,
            false,
            write_result.byte_len,
            write_result.output_digest,
            write_result.model_observation,
        )))
    }

    pub(super) fn prepare_provider_tool_call(
        &self,
        tool_call: &ProviderToolCall,
    ) -> Result<PreparedProviderToolCall, AgentLoopHostError> {
        self.validate_visible_request_scope()?;
        validate_provider_tool_call(tool_call)?;
        let provider_turn_id = tool_call.turn_id.clone().ok_or_else(|| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "provider tool call is missing a provider turn id",
            )
        })?;
        let Some((version, snapshot)) = self.current_snapshot()? else {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::StaleSurface,
                "capability surface is unavailable",
            ));
        };
        let (capability_id, capability) = snapshot.provider_capability(&tool_call.name)?;
        let prepared =
            capability.prepare_provider_tool_call(capability_id, &snapshot, tool_call)?;
        Ok(PreparedProviderToolCall {
            surface_version: loop_surface_version(&version)?,
            capability_id: prepared.capability_id,
            provider_turn_id,
            normalized_arguments: prepared.normalized_arguments,
            effective_capability_ids: prepared.effective_capability_ids,
        })
    }

    pub(super) async fn register_provider_tool_call_with_activity(
        &self,
        tool_call: ProviderToolCall,
        activity_id: Option<CapabilityActivityId>,
    ) -> Result<ironclaw_loop_contracts::CapabilityCallCandidate, AgentLoopHostError> {
        let prepared = self.prepare_provider_tool_call(&tool_call)?;
        let mut normalized_tool_call = tool_call.clone();
        normalized_tool_call.arguments = prepared.normalized_arguments;
        let input_ref = self
            .input_resolver
            .register_provider_tool_call_input(&self.run_context, &normalized_tool_call)
            .await?;
        // Record the activity-card display input now that both the canonical
        // `input_ref` and the resolved dotted `capability_id` are in hand, so
        // the card shows `nearai.web_search   <query>` (not the lossy provider
        // tool name `nearai__web_search`) and the per-tool summary matches.
        self.input_resolver.record_provider_tool_call_display_input(
            &self.run_context,
            &input_ref,
            &prepared.capability_id,
            &normalized_tool_call,
        );
        let registered_effective_capability_ids = (prepared.capability_id.as_str()
            == crate::capability_info::CAPABILITY_ID)
            .then(|| prepared.effective_capability_ids.iter().cloned().collect());
        let activity_id = self.record_provider_tool_call_registration(
            &input_ref,
            &prepared.capability_id,
            activity_id,
            registered_effective_capability_ids,
        )?;
        Ok(ironclaw_loop_contracts::CapabilityCallCandidate {
            activity_id,
            surface_version: prepared.surface_version,
            capability_id: prepared.capability_id,
            input_ref,
            effective_capability_ids: prepared.effective_capability_ids,
            provider_replay: Some(ProviderToolCallReplay {
                provider_id: tool_call.provider_id,
                provider_model_id: tool_call.provider_model_id,
                provider_turn_id: prepared.provider_turn_id,
                provider_call_id: tool_call.id,
                provider_tool_name: tool_call.name,
                arguments: tool_call.arguments,
                response_reasoning: tool_call.response_reasoning,
                reasoning: tool_call.reasoning,
                signature: tool_call.signature,
            }),
        })
    }
}
