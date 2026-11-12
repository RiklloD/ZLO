use super::*;

/// The [`GateRef`] a resolution channel renders its gate record from, when the
/// channel is gate-shaped. `Done`/`Denied` carry none; `Suspended(Process)`
/// tracks a process ref (no gate record) so it also answers `None`.
pub(super) fn gate_ref_for_resolution(resolution: &Resolution) -> Option<GateRef> {
    match resolution {
        Resolution::Blocked(blocked) => Some(*blocked.gate_ref()),
        Resolution::Suspended(suspension) => suspension.gate_ref().copied(),
        Resolution::Done(_) | Resolution::Denied(_) => None,
    }
}

/// Map a gate-record store failure to a fail-closed host error. The bound cause
/// (which may carry a host path) is logged server-side at `warn` — a genuine
/// host storage fault operators must see — and never interpolated into the
/// model-visible summary (capability-access contract).
pub(super) fn gate_record_store_error(error: ApprovalStoreError) -> AgentLoopHostError {
    tracing::warn!(error = %error, "failed to persist capability gate record at loop host seam");
    AgentLoopHostError::new(
        AgentLoopHostErrorKind::Unavailable,
        "failed to persist capability gate record",
    )
}

/// Transitional default [`GateRecordStorePort`] used until composition wires a
/// durable store into the capability-port factory via
/// [`HostRuntimeLoopCapabilityPortFactory::with_gate_record_store`].
///
/// It is a deliberate no-op, not fail-closed: the persisted [`GateRecord`] has
/// **no consumer yet** — the resume-turn render path that loads it by `GateRef`
/// (and the loop-ref↔minted-ref association it needs) is the explicit follow-up
/// slice (this PR mints the ref at the seam; the association + read land next).
/// Until then, skipping the write changes no observable behavior, so an unwired
/// path keeps producing gates exactly as before rather than regressing them.
/// When the durable store is wired into every composition path, the follow-up
/// flips this default to fail-closed.
#[derive(Debug, Default)]
pub(super) struct NoopGateRecordStore;

#[async_trait]
impl GateRecordStorePort for NoopGateRecordStore {
    async fn save(
        &self,
        _scope: ResourceScope,
        _gate_ref: GateRef,
        _record: GateRecord,
    ) -> Result<(), ApprovalStoreError> {
        // silent-ok: transitional no-op — the gate record has no reader until the
        // resume-read follow-up; skipping the durable write is behavior-preserving
        // and never regresses an unwired composition path's existing gates.
        tracing::debug!("gate record store not wired; skipping durable gate-record persistence");
        Ok(())
    }

    async fn load(
        &self,
        _scope: &ResourceScope,
        _gate_ref: GateRef,
    ) -> Result<Option<GateRecord>, ApprovalStoreError> {
        Ok(None)
    }
}

/// Map a replay-payload store failure to a fail-closed host error. The bound
/// cause (which may carry a host path) is logged server-side at `warn` — a
/// genuine host storage fault operators must see — and never interpolated into
/// the model-visible summary (capability-access contract). Mirrors
/// `gate_record_store_error`.
pub(super) fn replay_payload_store_error(error: ReplayPayloadStoreError) -> AgentLoopHostError {
    tracing::warn!(error = %error, "failed to persist/load capability replay payload at loop host seam");
    AgentLoopHostError::new(
        AgentLoopHostErrorKind::Unavailable,
        "failed to access capability replay payload",
    )
}

/// Transitional fail-closed default [`ReplayPayloadStorePort`] used until composition
/// wires a durable store into the capability-port factory via
/// [`HostRuntimeLoopCapabilityPortFactory::with_replay_payload_store`].
///
/// Unlike [`NoopGateRecordStore`] this is deliberately fail-closed on read: the
/// replay payload has a real consumer (the resume-read path,
/// `replay_payload_for_resume`), so an unwired store that silently returned an
/// empty payload would dispatch a resume with the WRONG (empty) input. `save`
/// no-ops (an unwired factory persists nothing) and `load` returns `Ok(None)`,
/// which the resume-read path treats as a sanitized terminal failure.
#[derive(Debug, Default)]
pub(super) struct NoopReplayPayloadStore;

#[async_trait]
impl ReplayPayloadStorePort for NoopReplayPayloadStore {
    async fn save(
        &self,
        _scope: ResourceScope,
        _invocation_id: InvocationId,
        _payload: ReplayPayload,
    ) -> Result<(), ReplayPayloadStoreError> {
        // silent-ok: transitional no-op — an unwired factory persists nothing; the
        // fail-closed `load` below turns any resume that needs a payload into a
        // sanitized terminal failure rather than a silent empty-input dispatch.
        tracing::debug!(
            "replay payload store not wired; skipping durable replay-payload persistence"
        );
        Ok(())
    }

    async fn load(
        &self,
        _scope: &ResourceScope,
        _invocation_id: InvocationId,
    ) -> Result<Option<ReplayPayload>, ReplayPayloadStoreError> {
        Ok(None)
    }
}
