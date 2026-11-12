use super::*;

pub(super) struct PreparedProviderToolCall {
    pub(super) surface_version: ironclaw_loop_contracts::CapabilitySurfaceVersion,
    pub(super) capability_id: CapabilityId,
    pub(super) provider_turn_id: String,
    pub(super) normalized_arguments: serde_json::Value,
    pub(super) effective_capability_ids: Vec<CapabilityId>,
}

pub(super) const MAX_IN_MEMORY_DISPATCH_RECORDS: usize = 128;

#[derive(Clone)]
pub(super) enum DispatchRecord {
    InFlight {
        notify: Arc<Notify>,
    },
    RuntimeCompleted {
        invocation_id: InvocationId,
        correlation_id: CorrelationId,
        requested_capability_id: CapabilityId,
        outcome: RuntimeCapabilityOutcome,
    },
    TerminalMilestonePending {
        invocation_id: InvocationId,
        result: Result<GatedResolution, AgentLoopHostError>,
        milestone: LoopHostMilestoneKind,
    },
    LoopCompleted {
        invocation_id: InvocationId,
        result: Result<GatedResolution, AgentLoopHostError>,
    },
}

pub(super) struct RuntimeOutcomeCompletion<'a> {
    pub(super) input_ref: &'a CapabilityInputRef,
    pub(super) invocation_id: InvocationId,
    pub(super) correlation_id: CorrelationId,
    pub(super) requested_capability_id: &'a CapabilityId,
    pub(super) provider: ExtensionId,
    pub(super) runtime: RuntimeKind,
    pub(super) outcome: RuntimeCapabilityOutcome,
}

pub(super) struct RuntimeOutcomeConversion<'a> {
    pub(super) input_ref: &'a CapabilityInputRef,
    pub(super) invocation_id: InvocationId,
    pub(super) correlation_id: CorrelationId,
    pub(super) requested_capability_id: &'a CapabilityId,
    pub(super) outcome: RuntimeCapabilityOutcome,
}

pub(super) fn ensure_cached_invocation_matches_activity(
    cached_invocation_id: InvocationId,
    requested_invocation_id: InvocationId,
) -> Result<(), AgentLoopHostError> {
    if cached_invocation_id == requested_invocation_id {
        return Ok(());
    }
    Err(AgentLoopHostError::new(
        AgentLoopHostErrorKind::InvalidInvocation,
        "cached capability dispatch activity identity does not match the requested activity",
    ))
}

impl<'a> RuntimeOutcomeCompletion<'a> {
    pub(super) fn conversion(&self) -> RuntimeOutcomeConversion<'a> {
        RuntimeOutcomeConversion {
            input_ref: self.input_ref,
            invocation_id: self.invocation_id,
            correlation_id: self.correlation_id,
            requested_capability_id: self.requested_capability_id,
            outcome: self.outcome.clone(),
        }
    }
}

#[derive(Default)]
pub(super) struct DispatchRecordStore {
    pub(super) records: HashMap<String, DispatchRecord>,
    pub(super) insertion_order: VecDeque<String>,
}

impl DispatchRecordStore {
    pub(super) fn reserve(
        &mut self,
        key: &IdempotencyKey,
        requested_invocation_id: InvocationId,
    ) -> Result<DispatchReservation, AgentLoopHostError> {
        let key_value = key.as_str().to_string();
        match self.records.get(key.as_str()).cloned() {
            Some(DispatchRecord::InFlight { notify }) => Ok(DispatchReservation::Wait(notify)),
            Some(DispatchRecord::RuntimeCompleted {
                invocation_id,
                correlation_id,
                requested_capability_id,
                outcome,
            }) => {
                ensure_cached_invocation_matches_activity(invocation_id, requested_invocation_id)?;
                self.records.insert(
                    key_value,
                    DispatchRecord::InFlight {
                        notify: Arc::new(Notify::new()),
                    },
                );
                Ok(DispatchReservation::RuntimeCompleted {
                    invocation_id,
                    correlation_id,
                    requested_capability_id,
                    outcome,
                })
            }
            Some(DispatchRecord::TerminalMilestonePending {
                invocation_id,
                result,
                milestone,
            }) => {
                ensure_cached_invocation_matches_activity(invocation_id, requested_invocation_id)?;
                self.records.insert(
                    key_value,
                    DispatchRecord::InFlight {
                        notify: Arc::new(Notify::new()),
                    },
                );
                Ok(DispatchReservation::TerminalMilestonePending {
                    invocation_id,
                    result,
                    milestone,
                })
            }
            Some(DispatchRecord::LoopCompleted {
                invocation_id,
                result,
            }) => {
                ensure_cached_invocation_matches_activity(invocation_id, requested_invocation_id)?;
                Ok(DispatchReservation::LoopCompleted(result))
            }
            None => {
                self.evict_completed_until_below_limit()?;
                self.insertion_order.push_back(key_value.clone());
                self.records.insert(
                    key_value,
                    DispatchRecord::InFlight {
                        notify: Arc::new(Notify::new()),
                    },
                );
                Ok(DispatchReservation::Reserved)
            }
        }
    }

    pub(super) fn record(
        &mut self,
        key: &IdempotencyKey,
        record: DispatchRecord,
    ) -> Option<Arc<Notify>> {
        let previous = self.records.insert(key.as_str().to_string(), record);
        match previous {
            Some(DispatchRecord::InFlight { notify }) => Some(notify),
            _ => None,
        }
    }

    pub(super) fn remove(&mut self, key: &IdempotencyKey) -> Option<Arc<Notify>> {
        let removed = self.records.remove(key.as_str());
        self.insertion_order
            .retain(|candidate| candidate != key.as_str());
        match removed {
            Some(DispatchRecord::InFlight { notify }) => Some(notify),
            _ => None,
        }
    }

    pub(super) fn in_flight_matches(&self, key: &IdempotencyKey, notify: &Arc<Notify>) -> bool {
        matches!(
            self.records.get(key.as_str()),
            Some(DispatchRecord::InFlight { notify: current }) if Arc::ptr_eq(current, notify)
        )
    }

    pub(super) fn evict_completed_until_below_limit(&mut self) -> Result<(), AgentLoopHostError> {
        let mut scanned = 0;
        let scan_limit = self.insertion_order.len();
        while self.records.len() >= MAX_IN_MEMORY_DISPATCH_RECORDS && scanned < scan_limit {
            let Some(candidate) = self.insertion_order.pop_front() else {
                break;
            };
            scanned += 1;
            match self.records.get(&candidate) {
                None => {}
                Some(DispatchRecord::InFlight { .. }) => self.insertion_order.push_back(candidate),
                Some(DispatchRecord::RuntimeCompleted { .. })
                | Some(DispatchRecord::TerminalMilestonePending { .. })
                | Some(DispatchRecord::LoopCompleted { .. }) => {
                    self.records.remove(&candidate);
                }
            }
        }
        if self.records.len() >= MAX_IN_MEMORY_DISPATCH_RECORDS {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                "capability dispatch record store is full",
            ));
        }
        Ok(())
    }
}

pub(super) enum DispatchReservation {
    Reserved,
    Wait(Arc<Notify>),
    RuntimeCompleted {
        invocation_id: InvocationId,
        correlation_id: CorrelationId,
        requested_capability_id: CapabilityId,
        outcome: RuntimeCapabilityOutcome,
    },
    TerminalMilestonePending {
        invocation_id: InvocationId,
        result: Result<GatedResolution, AgentLoopHostError>,
        milestone: LoopHostMilestoneKind,
    },
    LoopCompleted(Result<GatedResolution, AgentLoopHostError>),
}

/// RAII guard for an `InFlight` dispatch reservation: if the holder drops
/// without calling [`Self::commit`], the reservation is cleared and any
/// waiters are notified. Clearing failures are logged but do not panic, since
/// dropping happens on unwind paths where there's nothing useful to propagate.
pub(super) struct DispatchReservationGuard<'a> {
    pub(super) port: &'a HostRuntimeLoopCapabilityPort,
    pub(super) key: IdempotencyKey,
    pub(super) committed: bool,
}

impl DispatchReservationGuard<'_> {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for DispatchReservationGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(error) = self.port.clear_dispatch(&self.key) {
            tracing::warn!(
                cleanup_error = %error,
                "failed to clean up dispatch reservation after early return"
            );
        }
    }
}

/// RAII guard for an `InFlight` gate-resolution reservation: if the owning
/// persist future drops without calling [`Self::commit`] (cancellation, a
/// transient store fault, or any early error), the reservation is cleared and
/// its waiters woken so a same-key replay re-owns and retries — never left
/// waiting on an orphaned in-flight entry. Mirrors [`DispatchReservationGuard`].
pub(super) struct GateResolutionReservationGuard<'a> {
    pub(super) port: &'a HostRuntimeLoopCapabilityPort,
    pub(super) key: IdempotencyKey,
    pub(super) committed: bool,
}

impl GateResolutionReservationGuard<'_> {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for GateResolutionReservationGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(error) = self.port.clear_gate_resolution_reservation(&self.key) {
            tracing::warn!(
                cleanup_error = %error,
                "failed to clean up gate resolution reservation after early return"
            );
        }
    }
}

#[derive(Default)]
pub(super) struct ProviderToolCallRegistrationStore {
    pub(super) records: HashMap<String, ProviderToolCallRegistrationRecord>,
}

#[derive(Clone)]
pub(super) struct ProviderToolCallRegistrationRecord {
    pub(super) activity_id: CapabilityActivityId,
    pub(super) capability_id: CapabilityId,
    pub(super) effective_capability_ids: Option<HashSet<CapabilityId>>,
}

impl ProviderToolCallRegistrationStore {
    /// Register one canonical provider tool call for this run. `input_ref` is
    /// only the lookup key; the activity id remains an independent UI identity
    /// stored with the registration record.
    pub(super) fn record(
        &mut self,
        input_ref: &CapabilityInputRef,
        capability_id: &CapabilityId,
        activity_id: Option<CapabilityActivityId>,
        effective_capability_ids: Option<HashSet<CapabilityId>>,
    ) -> Result<CapabilityActivityId, AgentLoopHostError> {
        let key = input_ref.as_str().to_string();
        let record =
            self.records
                .entry(key)
                .or_insert_with(|| ProviderToolCallRegistrationRecord {
                    activity_id: activity_id.unwrap_or_default(),
                    capability_id: capability_id.clone(),
                    effective_capability_ids: None,
                });
        if record.capability_id != *capability_id {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "provider tool-call capability identity changed",
            ));
        }
        if let Some(activity_id) = activity_id
            && record.activity_id != activity_id
        {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "provider tool-call activity identity changed",
            ));
        }
        if let Some(next_effective_capability_ids) = effective_capability_ids {
            match &record.effective_capability_ids {
                Some(existing) if existing != &next_effective_capability_ids => {
                    return Err(AgentLoopHostError::new(
                        AgentLoopHostErrorKind::InvalidInvocation,
                        "provider tool-call effective capability identity changed",
                    ));
                }
                Some(_) => {}
                None => {
                    record.effective_capability_ids = Some(next_effective_capability_ids);
                }
            }
        }
        Ok(record.activity_id)
    }

    pub(super) fn registration_for(
        &self,
        input_ref: &CapabilityInputRef,
    ) -> Option<ProviderToolCallRegistrationRecord> {
        self.records.get(input_ref.as_str()).cloned()
    }
}
