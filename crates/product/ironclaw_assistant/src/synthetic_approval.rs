//! The approval gate a mutating synthetic capability runs before it acts.
//!
//! Synthetic capabilities do not cross the host catalog's authorization fold,
//! so a mutating one asks for approval itself, with the same durable records
//! the fold uses: per-tool overrides decide first (`disabled` denies,
//! `ask_each_time` asks), then "Full access", always-allow, or global
//! auto-approve let it run; otherwise it raises a durable approval request and
//! resumes only with a matching, claimed lease, consumed before it acts so a
//! cut-short call cannot replay it. The steps are `crate::supermemory`'s write
//! gate, made reusable.

use std::sync::Arc;

use ironclaw_approvals::{
    ApprovalRequestStorePort, ApprovalSettingsProvider, ApprovalStatus, ApprovalStoreError,
    GateRecordStorePort, ToolPermissionOverride,
};
use ironclaw_authorization::{
    CapabilityLeaseError, CapabilityLeaseStatus, CapabilityLeaseStorePort,
};
use ironclaw_capabilities::{ReplayPayload, ReplayPayloadStoreError, ReplayPayloadStorePort};
use ironclaw_host_api::{
    action::Action,
    approval::{ApprovalRequest, InvocationFingerprint},
    gate_record::GateRecord,
    ids::{ApprovalRequestId, CapabilityId, CorrelationId, GateRef, InvocationId, UserId},
    resolution::Resolution,
    resource::{ResourceEstimate, ResourceScope},
    safe_summary::SafeSummary,
    scope::Principal,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityApprovalResume,
    CapabilityDeniedReasonKind, CapabilityResumeToken, LoopRunContext, resolution,
};
use ironclaw_loop_host::SyntheticCapabilityInvocation;
use ironclaw_turns::LoopGateRef;
use serde_json::Value;

/// The approval stores a gate persists to, all owned by composition.
#[derive(Clone)]
pub struct SyntheticApprovalStores {
    pub approval_requests: Arc<dyn ApprovalRequestStorePort>,
    pub capability_leases: Arc<dyn CapabilityLeaseStorePort>,
    pub approval_settings: Arc<dyn ApprovalSettingsProvider>,
    pub replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
    pub gate_record_store: Arc<dyn GateRecordStorePort>,
}

/// What the caller does next.
pub(crate) enum GateOutcome {
    /// Act now: settings allow it, or an approved lease was claimed and consumed.
    Proceed,
    /// Return this resolution instead of acting (an approval request, or a
    /// model-visible denial).
    Respond(Box<Resolution>),
}

pub(crate) struct SyntheticApprovalGate {
    stores: SyntheticApprovalStores,
    fallback_user_id: UserId,
    /// Fixed, interpolation-free gate text (gate record and loop outcome).
    summary: &'static str,
    /// Stable denial-reason slug.
    denial_reason: &'static str,
}

impl SyntheticApprovalGate {
    pub(crate) fn new(
        stores: SyntheticApprovalStores,
        fallback_user_id: UserId,
        summary: &'static str,
        denial_reason: &'static str,
    ) -> Self {
        Self {
            stores,
            fallback_user_id,
            summary,
            denial_reason,
        }
    }

    /// Decide whether `invocation` of `capability_id` may act now. `reason`
    /// is the human-facing text of a new approval request.
    pub(crate) async fn check(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        capability_id: &CapabilityId,
        reason: impl FnOnce() -> String,
    ) -> Result<GateOutcome, AgentLoopHostError> {
        if invocation.request.auth_resume.is_some() {
            return Err(host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                "approval-gated synthetic capabilities do not support auth resume",
            ));
        }
        if let Some(resume) = invocation.request.approval_resume.clone() {
            return self.resume(invocation, &resume, capability_id).await;
        }
        let settings = &self.stores.approval_settings;
        let scope = self.settings_scope(&invocation.run_context);
        let thread_scope = invocation
            .run_context
            .acting_resource_scope(&self.fallback_user_id);
        let tool_override = settings.tool_override(&scope, capability_id).await;
        if matches!(tool_override, Some(ToolPermissionOverride::Disabled)) {
            return self
                .denied("this tool is disabled by tool approval settings")
                .map(|resolution| GateOutcome::Respond(Box::new(resolution)));
        }
        // "Full access" runs every call without asking; only `disabled` denies.
        if settings.full_access(&thread_scope).await {
            return Ok(GateOutcome::Proceed);
        }
        if matches!(tool_override, Some(ToolPermissionOverride::AskEachTime)) {
            return self
                .request_approval(invocation, capability_id, reason())
                .await;
        }
        if settings
            .tool_always_allow(&scope, capability_id, &grantee()?)
            .await
            || settings.global_auto_approve(&scope).await
        {
            return Ok(GateOutcome::Proceed);
        }
        self.request_approval(invocation, capability_id, reason())
            .await
    }

    fn settings_scope(&self, run: &LoopRunContext) -> ResourceScope {
        ResourceScope {
            tenant_id: run.scope.tenant_id.clone(),
            user_id: run.acting_user_id(&self.fallback_user_id),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        }
    }

    fn invocation_scope(&self, run: &LoopRunContext, invocation_id: InvocationId) -> ResourceScope {
        let mut scope = run.acting_resource_scope(&self.fallback_user_id);
        scope.invocation_id = invocation_id;
        scope
    }

    async fn request_approval(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        capability_id: &CapabilityId,
        reason: String,
    ) -> Result<GateOutcome, AgentLoopHostError> {
        let run = &invocation.run_context;
        let approval_request_id = ApprovalRequestId::new();
        let correlation_id = CorrelationId::new();
        let invocation_id = InvocationId::new();
        let estimate = ResourceEstimate::default();
        let scope = self.invocation_scope(run, invocation_id);
        let fingerprint = fingerprint(&scope, capability_id, &estimate, &invocation.input)?;
        let replay_scope = run.acting_resource_scope(&self.fallback_user_id);
        self.stores
            .replay_payload_store
            .save(
                replay_scope.clone(),
                invocation_id,
                ReplayPayload {
                    input: invocation.input.clone(),
                    estimate: estimate.clone(),
                    prior_approval: None,
                    input_ref: invocation.request.input_ref.clone(),
                    correlation_id,
                },
            )
            .await
            .map_err(replay_error)?;
        self.stores
            .approval_requests
            .save_pending(
                scope,
                ApprovalRequest {
                    id: approval_request_id,
                    correlation_id,
                    requested_by: grantee()?,
                    action: Box::new(Action::Dispatch {
                        capability: capability_id.clone(),
                        estimated_resources: estimate,
                    }),
                    invocation_fingerprint: Some(fingerprint),
                    reason,
                    reusable_scope: None,
                },
            )
            .await
            .map_err(|error| store_error("save_pending_approval", error))?;
        let summary = SafeSummary::new(self.summary).map_err(|error| {
            ironclaw_loop_host::raw_agent_loop_host_error(
                "synthetic_approval",
                "gate_record_summary",
                AgentLoopHostErrorKind::Internal,
                "synthetic approval gate summary is not renderable",
                error,
            )
        })?;
        self.stores
            .gate_record_store
            .save(
                replay_scope,
                GateRef::for_approval_request(approval_request_id),
                GateRecord::Approval { summary },
            )
            .await
            .map_err(|error| store_error("save_gate_record", error))?;
        let gate_ref = LoopGateRef::new(format!("gate:approval-{approval_request_id}"))
            .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))?;
        let resume_token = CapabilityResumeToken::new(invocation_id.to_string())
            .map_err(|reason| host_error(AgentLoopHostErrorKind::Internal, reason.to_string()))?;
        Ok(GateOutcome::Respond(Box::new(
            resolution::approval_required(
                gate_ref,
                self.summary.to_string(),
                Some(CapabilityApprovalResume {
                    approval_request_id,
                    resume_token,
                    correlation_id,
                    input_ref: invocation.request.input_ref.clone(),
                }),
            )
            .resolution,
        )))
    }

    /// A resumed call acts only with the approved, matching lease, which is
    /// claimed and consumed here. A lost or ungranted approval is a denial the
    /// user can re-request; a mismatch is a host error.
    async fn resume(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        resume: &CapabilityApprovalResume,
        capability_id: &CapabilityId,
    ) -> Result<GateOutcome, AgentLoopHostError> {
        let run = &invocation.run_context;
        let invocation_id = InvocationId::parse(resume.resume_token.as_str()).map_err(|error| {
            host_error(AgentLoopHostErrorKind::InvalidInvocation, error.to_string())
        })?;
        let replay = self
            .stores
            .replay_payload_store
            .load(
                &run.acting_resource_scope(&self.fallback_user_id),
                invocation_id,
            )
            .await
            .map_err(replay_error)?
            .ok_or_else(|| {
                host_error(
                    AgentLoopHostErrorKind::Unavailable,
                    "synthetic approval replay payload is unavailable",
                )
            })?;
        let mismatch = |what: &str| {
            host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                format!("synthetic approval {what} does not match"),
            )
        };
        if replay.input != invocation.input {
            return Err(mismatch("input"));
        }
        let scope = self.invocation_scope(run, invocation_id);
        let fingerprint = fingerprint(&scope, capability_id, &replay.estimate, &invocation.input)?;
        let Some(record) = self
            .stores
            .approval_requests
            .get(&scope, resume.approval_request_id)
            .await
            .map_err(|error| store_error("load_approval", error))?
        else {
            return self
                .denied("the approval is unavailable; re-request approval")
                .map(|resolution| GateOutcome::Respond(Box::new(resolution)));
        };
        if record.status != ApprovalStatus::Approved {
            return self
                .denied("the approval has not been granted; re-request approval")
                .map(|resolution| GateOutcome::Respond(Box::new(resolution)));
        }
        if record.request.correlation_id != replay.correlation_id {
            return Err(mismatch("correlation"));
        }
        if record.request.invocation_fingerprint.as_ref() != Some(&fingerprint) {
            return Err(mismatch("fingerprint"));
        }
        if !matches!(record.request.action.as_ref(), Action::Dispatch { capability, .. } if capability == capability_id)
        {
            return Err(mismatch("action"));
        }
        let leases = &self.stores.capability_leases;
        let Some(lease) = leases
            .leases_for_scope(&scope)
            .await
            .into_iter()
            .find(|lease| {
                lease.status == CapabilityLeaseStatus::Active
                    && lease.grant.capability == *capability_id
                    && lease.grant.grantee == record.request.requested_by
                    && lease.invocation_fingerprint.as_ref() == Some(&fingerprint)
            })
        else {
            return self
                .denied("the approval lease is unavailable; re-request approval")
                .map(|resolution| GateOutcome::Respond(Box::new(resolution)));
        };
        if let Err(error) = leases.claim(&scope, lease.grant.id, &fingerprint).await {
            return self.lease_outcome("claim_approval_lease", error);
        }
        if let Err(error) = leases.consume(&scope, lease.grant.id).await {
            return self.lease_outcome("consume_approval_lease", error);
        }
        Ok(GateOutcome::Proceed)
    }

    fn denied(&self, summary: &str) -> Result<Resolution, AgentLoopHostError> {
        let reason = CapabilityDeniedReasonKind::unknown(self.denial_reason)
            .map_err(|reason| host_error(AgentLoopHostErrorKind::Internal, reason.to_string()))?;
        Ok(resolution::denied(reason, summary.to_string()).resolution)
    }

    /// A stale or lost lease is a recoverable denial; lease storage faults end the run.
    fn lease_outcome(
        &self,
        operation: &'static str,
        error: CapabilityLeaseError,
    ) -> Result<GateOutcome, AgentLoopHostError> {
        match error {
            CapabilityLeaseError::UnknownLease { .. }
            | CapabilityLeaseError::ExpiredLease { .. }
            | CapabilityLeaseError::ExhaustedLease { .. }
            | CapabilityLeaseError::UnclaimedFingerprintLease { .. }
            | CapabilityLeaseError::FingerprintMismatch { .. }
            | CapabilityLeaseError::InactiveLease { .. } => self
                .denied("the approval lease is no longer valid; re-request approval")
                .map(|resolution| GateOutcome::Respond(Box::new(resolution))),
            CapabilityLeaseError::Persistence { .. }
            | CapabilityLeaseError::VersionMismatch
            | CapabilityLeaseError::CasExhausted => {
                Err(ironclaw_loop_host::raw_agent_loop_host_error(
                    "synthetic_approval",
                    operation,
                    AgentLoopHostErrorKind::Unavailable,
                    "synthetic approval lease operation failed",
                    error,
                ))
            }
        }
    }
}

/// The same synthetic grantee the other approval-gated builtin tools use.
fn grantee() -> Result<Principal, AgentLoopHostError> {
    crate::outbound_delivery_synthetic_provider()
        .map(Principal::Extension)
        .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))
}

fn fingerprint(
    scope: &ResourceScope,
    capability_id: &CapabilityId,
    estimate: &ResourceEstimate,
    input: &Value,
) -> Result<InvocationFingerprint, AgentLoopHostError> {
    InvocationFingerprint::for_dispatch(scope, capability_id, estimate, input)
        .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))
}

fn host_error(kind: AgentLoopHostErrorKind, message: impl Into<String>) -> AgentLoopHostError {
    AgentLoopHostError::new(kind, message)
}

fn replay_error(error: ReplayPayloadStoreError) -> AgentLoopHostError {
    tracing::debug!(error = %error, "failed to access synthetic approval replay payload");
    host_error(
        AgentLoopHostErrorKind::Unavailable,
        "failed to access capability replay payload",
    )
}

fn store_error(operation: &'static str, error: ApprovalStoreError) -> AgentLoopHostError {
    ironclaw_loop_host::raw_agent_loop_host_error(
        "synthetic_approval",
        operation,
        AgentLoopHostErrorKind::Unavailable,
        "synthetic approval state operation failed",
        error,
    )
}

#[cfg(test)]
mod tests;
