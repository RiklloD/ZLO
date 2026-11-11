use super::*;

impl HostRuntimeLoopCapabilityPort {
    /// Persist the durable, model-visible [`GateRecord`] a later resume turn
    /// renders from
    /// (§5.2.9), keyed by the freshly-minted [`GateRef`] on the resolution
    /// channel (#6242 mapping / #6243 store). `DenyRecord` is terminal and
    /// same-turn (per #6243) and is intentionally NOT persisted; `Done` and
    /// `Suspended(Process)` carry no gate record and no-op.
    ///
    /// Fail-closed: a store write failure is a genuine host storage fault and
    /// propagates, consistent with `record_loop_completed`/`record_runtime_completed`.
    ///
    /// The idempotency key (the write-once replay guard) is derived HERE, after
    /// dispatch and only once a gate record exists, from the SAME store-derived
    /// input_ref dispatch used (hazard 3, §5.3 Stage 0): on a resume the payload
    /// is reconstituted from the store, not the advisory loop-supplied
    /// `resume.input_ref`, so the key is byte-stable. Deriving it lazily keeps a
    /// missing/stale resume payload from pre-empting dispatch's own resume
    /// identity/activity validation — a malformed resume surfaces
    /// `InvalidInvocation` from dispatch, never a spurious payload-missing error.
    pub(super) async fn persist_gate_record_for_mapped(
        &self,
        request: &LoopRequest,
        gated: GatedResolution,
    ) -> Result<Resolution, AgentLoopHostError> {
        let Some(record) = gated.gate_record.as_ref() else {
            // Done / Denied / Suspended(Process): nothing durable to persist, no
            // idempotency key needed, and no gate ref that must stay loadable.
            return Ok(gated.resolution);
        };
        let Some(gate_ref) = gate_ref_for_resolution(&gated.resolution) else {
            // A gate record without a gate-ref-bearing channel is a mapping
            // invariant violation, not a recoverable model-visible error.
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                "mapped gate record has no gate ref on its resolution channel",
            ));
        };
        // The dispatch that produced this gate outcome already reconstituted (and
        // for a fresh raise, persisted) the resume payload, so this load hits a
        // present record on a resume and returns `None` on a fresh dispatch.
        let resume_payload = self.resume_replay_payload(request).await?;
        let effective_input_ref = resume_payload
            .as_ref()
            .map(|payload| &payload.input_ref)
            .unwrap_or(&request.input_ref);
        let idempotency_key =
            invocation_idempotency_key(&self.run_context, request, effective_input_ref)?;
        // Reserve-or-wait: exactly one caller (the owner) persists the record;
        // repeats and concurrent duplicates WAIT for that durable save before
        // receiving the resolution, so a concurrent replay never receives a
        // blocked resolution whose gate record is not yet persisted (#6287). The
        // mapping mints a fresh random `GateRef` per call, so a waiter must
        // return the owner's resolution — the one whose gate ref the record is
        // under — not its own re-mint. A failed save clears the reservation and
        // wakes the waiters so one re-owns and retries.
        let owner_notify = loop {
            let wait_notify = {
                let mut reserved = lock_mut(
                    &self.persisted_gate_resolutions,
                    "gate resolution replay cache",
                )?;
                match reserved.get(&idempotency_key) {
                    Some(GateResolutionState::Persisted(resolution)) => {
                        return Ok(resolution.as_ref().clone());
                    }
                    Some(GateResolutionState::InFlight(notify)) => Arc::clone(notify),
                    None => {
                        let notify = Arc::new(Notify::new());
                        reserved.insert(
                            idempotency_key.clone(),
                            GateResolutionState::InFlight(Arc::clone(&notify)),
                        );
                        break notify;
                    }
                }
            };
            // Lost-wakeup-safe wait (mirrors `wait_for_dispatch_completion`):
            // register on the notify, then re-check under the lock; only await if
            // the entry is still the SAME in-flight reservation.
            let notified = wait_notify.notified();
            tokio::pin!(notified);
            if self.gate_resolution_in_flight_matches(&idempotency_key, &wait_notify)? {
                notified.await;
            }
        };

        // RAII cleanup: if this future is cancelled (dropped mid-`save`) or
        // returns early before we commit, the guard clears the reservation and
        // wakes its waiters so a same-key replay re-owns and retries — never left
        // waiting on an orphaned in-flight entry (#6287 IronLoop). The success
        // path commits the guard AFTER publishing the durable resolution.
        let reservation_guard = GateResolutionReservationGuard {
            port: self,
            key: idempotency_key.clone(),
            committed: false,
        };
        let scope = self.visible_request.context.resource_scope.clone();
        let save_result = self
            .gate_record_store
            .save(scope, gate_ref, record.clone())
            .await;
        match save_result {
            // Success: publish the resolution (so waiters receive the SAME gate
            // ref the record is under), wake them, and commit the guard so its
            // drop is a no-op.
            Ok(()) => {
                self.publish_gate_resolution(&idempotency_key, &gated.resolution)?;
                owner_notify.notify_waiters();
                reservation_guard.commit();
                Ok(gated.resolution)
            }
            // A deterministic gate-record key (the auth gate's `for_auth_gate`,
            // and the approval gate's `for_approval_request` on the authorize
            // path) means a re-raise of the SAME gate — a deny-then-retry, or a
            // fresh port instance whose in-memory reservation was reset across
            // turns — derives the SAME content-addressed key and an identical
            // record. The write-once store reports `GateRecordAlreadyExists`;
            // that is benign (already persisted, byte-identical), never a fault.
            // "Byte-identical" holds because the auth-gate fingerprint
            // (`stable_auth_gate_id`) covers `setup` as well as provider /
            // requester / scopes (#6299 IronLoop), so two requirements that
            // differ in their setup flow derive DIFFERENT keys and never reach
            // this branch with a stale record.
            // Mirrors `persist_replay_payload_for_fresh_gate`'s tolerance of
            // `ReplayPayloadAlreadyExists`. Publish + commit like the success path.
            Err(ApprovalStoreError::GateRecordAlreadyExists { .. }) => {
                tracing::debug!(
                    %gate_ref,
                    "gate record already persisted for this deterministic key; keeping existing record"
                );
                self.publish_gate_resolution(&idempotency_key, &gated.resolution)?;
                owner_notify.notify_waiters();
                reservation_guard.commit();
                Ok(gated.resolution)
            }
            // Transient fault: do NOT commit. The guard's drop clears the
            // reservation and wakes the waiters so one re-owns and retries the
            // persist instead of hanging on an orphaned in-flight entry.
            Err(error) => Err(gate_record_store_error(error)),
        }
    }

    /// Publish the durable resolution for `key` so a waiting or later same-key
    /// replay receives the SAME gate ref the record was persisted under.
    pub(super) fn publish_gate_resolution(
        &self,
        key: &IdempotencyKey,
        resolution: &Resolution,
    ) -> Result<(), AgentLoopHostError> {
        lock_mut(
            &self.persisted_gate_resolutions,
            "gate resolution replay cache",
        )?
        .insert(
            key.clone(),
            GateResolutionState::Persisted(Box::new(resolution.clone())),
        );
        Ok(())
    }

    /// Clear an in-flight gate-resolution reservation for `key` and wake its
    /// waiters so one re-owns and retries. Only clears an `InFlight` entry (never
    /// a published resolution), so a committed owner's guard is a no-op here.
    pub(super) fn clear_gate_resolution_reservation(
        &self,
        key: &IdempotencyKey,
    ) -> Result<(), AgentLoopHostError> {
        let mut reserved = lock_mut(
            &self.persisted_gate_resolutions,
            "gate resolution replay cache",
        )?;
        if let Some(GateResolutionState::InFlight(notify)) = reserved.get(key) {
            let notify = Arc::clone(notify);
            reserved.remove(key);
            drop(reserved);
            notify.notify_waiters();
        }
        Ok(())
    }

    /// True iff `key`'s reservation is still the SAME in-flight entry `notify`
    /// belongs to — the re-check that makes [`Self::persist_gate_record_for_mapped`]'s
    /// wait lost-wakeup-safe (mirrors [`Self::dispatch_in_flight_matches`]).
    pub(super) fn gate_resolution_in_flight_matches(
        &self,
        key: &IdempotencyKey,
        notify: &Arc<Notify>,
    ) -> Result<bool, AgentLoopHostError> {
        let reserved = lock_mut(
            &self.persisted_gate_resolutions,
            "gate resolution replay cache",
        )?;
        Ok(match reserved.get(key) {
            Some(GateResolutionState::InFlight(existing)) => Arc::ptr_eq(existing, notify),
            _ => false,
        })
    }

    /// Persist the host-private [`ReplayPayload`] a later gate/auth resume
    /// reconstitutes `{input, estimate}` from (arch-simplification §5.3 Stage
    /// 2a-i), keyed by `invocation_id`. Only an approval/auth gate outcome
    /// carries a resume; every other outcome no-ops.
    ///
    /// Called ONLY on a fresh dispatch, so `prior_approval` is always absent here
    /// (a fresh invocation has passed no prior approval gate) and the write cannot
    /// collide with an existing entry for a reused invocation id. The payload is
    /// invocation-stable, so a benign duplicate (`ReplayPayloadAlreadyExists`) is
    /// tolerated rather than ending the run; any other store fault is a genuine
    /// host storage failure and fails closed.
    pub(super) async fn persist_replay_payload_for_fresh_gate(
        &self,
        invocation_id: InvocationId,
        input_ref: &CapabilityInputRef,
        input: &Value,
        estimate: &ResourceEstimate,
        correlation_id: CorrelationId,
        outcome: &RuntimeCapabilityOutcome,
    ) -> Result<(), AgentLoopHostError> {
        if !matches!(
            outcome,
            RuntimeCapabilityOutcome::ApprovalRequired(_)
                | RuntimeCapabilityOutcome::AuthRequired(_)
        ) {
            return Ok(());
        }
        let payload = ReplayPayload {
            input: input.clone(),
            estimate: estimate.clone(),
            // Fresh dispatch: no prior approval. The approval→auth bridge keeps
            // the prior-approval identity on the loop-facing resume wire in this
            // slice (it moves host-side in §5.3 Stage 2a-ii).
            prior_approval: None,
            input_ref: input_ref.clone(),
            correlation_id,
        };
        let scope = self.visible_request.context.resource_scope.clone();
        match self
            .replay_payload_store
            .save(scope, invocation_id, payload)
            .await
        {
            Ok(()) => Ok(()),
            Err(ReplayPayloadStoreError::ReplayPayloadAlreadyExists { .. }) => {
                // Invocation-stable payload already persisted; the resume-read path
                // will load the identical record. Benign, not a fault.
                tracing::debug!(
                    invocation_id = %invocation_id,
                    "replay payload already persisted for fresh gate raise; keeping existing record"
                );
                Ok(())
            }
            Err(error) => Err(replay_payload_store_error(error)),
        }
    }

    /// Load the host-private replay payload persisted at the fresh gate raise for
    /// `invocation_id` (recovered from the resume token). **Fail closed on a
    /// miss:** a resume whose payload is absent — including a wrong-scope read the
    /// store reports as unknown — is a sanitized terminal failure, never a silent
    /// empty-input dispatch (arch-simplification §5.3 Stage 2a-i).
    /// Reconstitute the host-private replay payload a resume binds to, if this is
    /// a resume. On a gate/auth resume the loop-supplied `input_ref` is ADVISORY:
    /// the payload persisted at the FRESH gate raise is the host-side source of
    /// truth for `input_ref` (and `{input, estimate}`), so the idempotency key
    /// stays byte-stable regardless of what the loop echoes back, and a resume
    /// whose payload is absent fails CLOSED (§5.3 Stage 2a-i / Stage 0).
    ///
    /// Returns `None` for a fresh dispatch and for the mutually-exclusive
    /// both-resume-modes case — `invoke_capability_dispatch`'s `resume_mode`
    /// resolution surfaces the latter as `InvalidInvocation`; this helper does not
    /// pre-empt that with a payload load. Keeping the derivation in one place lets
    /// the `invoke_capability` seam and dispatch compute the SAME key.
    pub(super) async fn resume_replay_payload(
        &self,
        request: &LoopRequest,
    ) -> Result<Option<ReplayPayload>, AgentLoopHostError> {
        let invocation_id = match (
            request.approval_resume.as_ref(),
            request.auth_resume.as_ref(),
        ) {
            (Some(_), Some(_)) | (Option::None, Option::None) => return Ok(Option::None),
            (Some(resume), Option::None) => invocation_id_from_resume_token(&resume.resume_token)?,
            (Option::None, Some(auth_resume)) => {
                let Some(resume_token) = auth_resume.resume_token.as_ref() else {
                    return Ok(Option::None);
                };
                invocation_id_from_resume_token(resume_token)?
            }
        };
        Ok(Some(self.replay_payload_for_resume(invocation_id).await?))
    }

    pub(super) async fn replay_payload_for_resume(
        &self,
        invocation_id: InvocationId,
    ) -> Result<ReplayPayload, AgentLoopHostError> {
        let scope = self.visible_request.context.resource_scope.clone();
        let payload = self
            .replay_payload_store
            .load(&scope, invocation_id)
            .await
            .map_err(replay_payload_store_error)?;
        payload.ok_or_else(|| {
            tracing::warn!(
                invocation_id = %invocation_id,
                "capability resume replay payload is missing; failing the run closed"
            );
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                "capability resume replay payload is unavailable",
            )
        })
    }
}
