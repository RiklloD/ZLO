//! The approval gate every mutating synthetic capability passes before its
//! handler runs.
//!
//! Synthetic capabilities do not cross the capability host's authorization
//! fold, so [`SyntheticCapabilityPort`](crate::synthetic_capability) runs this
//! gate itself, at its one dispatch boundary, for every capability declared
//! [`SyntheticCapabilityEffect::Mutating`](crate::SyntheticCapabilityEffect).
//! The decision is the kernel's own precedence
//! ([`ironclaw_approvals::approval_gate_verdict`]) over the invocation's full
//! resource scope, thread included, so a thread profile decides exactly as it
//! does for builtins. A fresh call that must ask raises a durable approval
//! request; a resumed call acts only after the decision allows it AND its
//! approved, fingerprinted lease is claimed and consumed — a tool disabled
//! after approval is refused before the lease is touched.

use std::{collections::VecDeque, sync::Arc, sync::Mutex};

use ironclaw_approvals::{
    ApprovalGateQuery, ApprovalGateVerdict, ApprovalRequestStorePort, ApprovalSettingsProvider,
    ApprovalStatus, ApprovalStoreError, GateRecordStorePort, approval_gate_verdict,
};
use ironclaw_authorization::{
    CapabilityLeaseError, CapabilityLeaseStatus, CapabilityLeaseStorePort,
};
use ironclaw_capabilities::{ReplayPayload, ReplayPayloadStoreError, ReplayPayloadStorePort};
use ironclaw_host_api::{
    action::Action,
    approval::{ApprovalRequest, InvocationFingerprint},
    gate_record::GateRecord,
    ids::{
        ApprovalRequestId, CapabilityGrantId, CapabilityId, CorrelationId, GateRef, InvocationId,
        UserId,
    },
    resolution::Resolution,
    resource::{ResourceEstimate, ResourceScope},
    safe_summary::SafeSummary,
    scope::Principal,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityApprovalResume,
    CapabilityDeniedReasonKind, CapabilityResumeToken, LoopRequest, LoopRunContext, resolution,
};
use ironclaw_turns::{LoopGateRef, TurnId};
use serde_json::Value;

/// The approval stores the gate persists to, all owned by composition.
#[derive(Clone)]
pub struct SyntheticApprovalStores {
    pub approval_requests: Arc<dyn ApprovalRequestStorePort>,
    pub capability_leases: Arc<dyn CapabilityLeaseStorePort>,
    pub approval_settings: Arc<dyn ApprovalSettingsProvider>,
    pub replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
    pub gate_record_store: Arc<dyn GateRecordStorePort>,
}

/// How one mutating synthetic capability asks for approval.
#[derive(Clone)]
pub struct SyntheticApproval {
    /// Fixed, interpolation-free gate text (gate record and loop outcome).
    summary: &'static str,
    /// What the person is asked to approve, from the call's input.
    reason: Arc<dyn Fn(&Value) -> String + Send + Sync>,
    global_auto_approve: bool,
    lasts_for_task: bool,
}

impl std::fmt::Debug for SyntheticApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntheticApproval")
            .field("summary", &self.summary)
            .field("global_auto_approve", &self.global_auto_approve)
            .field("lasts_for_task", &self.lasts_for_task)
            .finish_non_exhaustive()
    }
}

impl SyntheticApproval {
    pub fn new(summary: &'static str, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self::with_input_reason(summary, move |_| reason.clone())
    }

    /// Like [`Self::new`], with the approval text derived from the call's
    /// (already validated) input — never interpolated into `summary`.
    pub fn with_input_reason(
        summary: &'static str,
        reason: impl Fn(&Value) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            summary,
            reason: Arc::new(reason),
            global_auto_approve: true,
            lasts_for_task: false,
        }
    }

    pub fn honors_global_auto_approve(&self) -> bool {
        self.global_auto_approve
    }

    pub fn lasts_for_task(&self) -> bool {
        self.lasts_for_task
    }

    /// The global auto-approve switch never lets this capability run —
    /// neither directly nor as the Full access a thread merely inherits from
    /// it; only an explicit Full-access profile, a per-tool always-allow, or
    /// an approval does.
    pub fn ignoring_global_auto_approve(mut self) -> Self {
        self.global_auto_approve = false;
        self
    }

    /// One approval covers the rest of the task: later calls in the same turn
    /// that share this approval's summary run without asking (unless the tool
    /// is set to ask each time).
    pub fn lasting_for_task(mut self) -> Self {
        self.lasts_for_task = true;
        self
    }
}

pub(crate) enum GateOutcome {
    /// Act now.
    Proceed,
    /// Return this resolution instead of acting (an approval request, or a
    /// model-visible denial).
    Respond(Box<Resolution>),
}

/// Turns whose task approval was given, in memory and bounded; a restart asks
/// again.
const APPROVED_TASKS_KEPT: usize = 64;

pub struct SyntheticApprovalGate {
    stores: SyntheticApprovalStores,
    grantee: Principal,
    policy_requires_approval: bool,
    approved_tasks: Mutex<VecDeque<(TurnId, &'static str)>>,
}

struct MatchedLease {
    scope: ResourceScope,
    lease_id: CapabilityGrantId,
    fingerprint: InvocationFingerprint,
}

impl SyntheticApprovalGate {
    /// `grantee` is the principal approvals and always-allow grants name;
    /// `policy_requires_approval` is whether the runtime policy gates a
    /// state-changing effect at all (the gate's last step).
    pub fn new(
        stores: SyntheticApprovalStores,
        grantee: Principal,
        policy_requires_approval: bool,
    ) -> Self {
        Self {
            stores,
            grantee,
            policy_requires_approval,
            approved_tasks: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn replay_payload_store(&self) -> &Arc<dyn ReplayPayloadStorePort> {
        &self.stores.replay_payload_store
    }

    pub(crate) async fn check(
        &self,
        run: &LoopRunContext,
        fallback_user_id: &UserId,
        request: &LoopRequest,
        input: &Value,
        approval: &SyntheticApproval,
    ) -> Result<GateOutcome, AgentLoopHostError> {
        if request.auth_resume.is_some() {
            return Err(host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                "approval-gated synthetic capabilities do not support auth resume",
            ));
        }
        let capability_id = &request.capability_id;
        let lease = match &request.approval_resume {
            Some(resume) => {
                self.matched_lease(run, fallback_user_id, resume, capability_id, input)
                    .await?
            }
            None => Err("approval required"),
        };
        // The invocation's full scope — thread included — like the capability
        // host's own gate, so the thread profile decides Full access and
        // auto-approve; a fresh invocation id keeps per-call caches per call.
        let mut scope = run.acting_resource_scope(fallback_user_id);
        scope.invocation_id = InvocationId::new();
        let verdict = approval_gate_verdict(
            self.stores.approval_settings.as_ref(),
            ApprovalGateQuery {
                scope: &scope,
                capability_id,
                grantee: &self.grantee,
                approved_for_this_invocation: lease.is_ok(),
                force_approval: false,
                exempt_from_approval: false,
                global_auto_approve_eligible: approval.global_auto_approve,
                // A Full access the thread only inherits from the global
                // switch is global auto-approve by another name.
                inherited_full_access_eligible: approval.global_auto_approve,
                always_allow_eligible: true,
                approved_for_this_task: approval.lasts_for_task
                    && self.task_approved(&run.turn_id, approval.summary),
                policy_requires_approval: self.policy_requires_approval,
            },
        )
        .await;
        match verdict {
            // Disabled beats a one-shot approval: refused before the lease is
            // claimed, so the approval is not spent.
            ApprovalGateVerdict::Deny => self
                .denied("this tool is disabled by tool approval settings")
                .map(|resolution| GateOutcome::Respond(Box::new(resolution))),
            // Unreadable tool settings refuse too, before the lease is touched.
            ApprovalGateVerdict::SettingsUnavailable => self
                .settings_unavailable()
                .map(|resolution| GateOutcome::Respond(Box::new(resolution))),
            ApprovalGateVerdict::Allow => {
                if let Ok(lease) = lease {
                    if let Some(denial) = self.spend(&lease).await? {
                        return Ok(GateOutcome::Respond(Box::new(denial)));
                    }
                    if approval.lasts_for_task {
                        self.approve_task(&run.turn_id, approval.summary);
                    }
                }
                Ok(GateOutcome::Proceed)
            }
            ApprovalGateVerdict::RequireApproval => match (&request.approval_resume, lease) {
                (Some(_), Err(reason)) => self
                    .denied(reason)
                    .map(|resolution| GateOutcome::Respond(Box::new(resolution))),
                _ => {
                    self.request_approval(run, fallback_user_id, request, input, approval)
                        .await
                }
            },
        }
    }

    fn task_approved(&self, turn: &TurnId, summary: &'static str) -> bool {
        self.approved_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|(approved, group)| approved == turn && *group == summary)
    }

    fn approve_task(&self, turn: &TurnId, summary: &'static str) {
        let mut tasks = self
            .approved_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !tasks
            .iter()
            .any(|(approved, group)| approved == turn && *group == summary)
        {
            if tasks.len() == APPROVED_TASKS_KEPT {
                tasks.pop_front();
            }
            tasks.push_back((*turn, summary));
        }
    }

    fn invocation_scope(
        run: &LoopRunContext,
        fallback_user_id: &UserId,
        invocation_id: InvocationId,
    ) -> ResourceScope {
        let mut scope = run.acting_resource_scope(fallback_user_id);
        scope.invocation_id = invocation_id;
        scope
    }

    async fn request_approval(
        &self,
        run: &LoopRunContext,
        fallback_user_id: &UserId,
        request: &LoopRequest,
        input: &Value,
        approval: &SyntheticApproval,
    ) -> Result<GateOutcome, AgentLoopHostError> {
        let capability_id = &request.capability_id;
        let approval_request_id = ApprovalRequestId::new();
        let correlation_id = CorrelationId::new();
        let invocation_id = InvocationId::new();
        let estimate = ResourceEstimate::default();
        let scope = Self::invocation_scope(run, fallback_user_id, invocation_id);
        let fingerprint = fingerprint(&scope, capability_id, &estimate, input)?;
        let replay_scope = run.acting_resource_scope(fallback_user_id);
        self.stores
            .replay_payload_store
            .save(
                replay_scope.clone(),
                invocation_id,
                ReplayPayload {
                    input: input.clone(),
                    estimate: estimate.clone(),
                    prior_approval: None,
                    input_ref: request.input_ref.clone(),
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
                    requested_by: self.grantee.clone(),
                    action: Box::new(Action::Dispatch {
                        capability: capability_id.clone(),
                        estimated_resources: estimate,
                    }),
                    invocation_fingerprint: Some(fingerprint),
                    reason: (approval.reason)(input),
                    reusable_scope: None,
                },
            )
            .await
            .map_err(|error| store_error("save_pending_approval", error))?;
        let summary = SafeSummary::new(approval.summary).map_err(|error| {
            crate::raw_agent_loop_host_error(
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
                approval.summary.to_string(),
                Some(CapabilityApprovalResume {
                    approval_request_id,
                    resume_token,
                    correlation_id,
                    input_ref: request.input_ref.clone(),
                }),
            )
            .resolution,
        )))
    }

    /// The approved, active lease that exactly matches this resumed call.
    /// `Ok(Err(reason))` is a lost or ungranted approval the user can
    /// re-request; `Err` is a mismatch (a host error). Nothing is claimed.
    async fn matched_lease(
        &self,
        run: &LoopRunContext,
        fallback_user_id: &UserId,
        resume: &CapabilityApprovalResume,
        capability_id: &CapabilityId,
        input: &Value,
    ) -> Result<Result<MatchedLease, &'static str>, AgentLoopHostError> {
        let invocation_id = InvocationId::parse(resume.resume_token.as_str()).map_err(|error| {
            host_error(AgentLoopHostErrorKind::InvalidInvocation, error.to_string())
        })?;
        let replay = self
            .stores
            .replay_payload_store
            .load(&run.acting_resource_scope(fallback_user_id), invocation_id)
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
        if &replay.input != input {
            return Err(mismatch("input"));
        }
        let scope = Self::invocation_scope(run, fallback_user_id, invocation_id);
        let fingerprint = fingerprint(&scope, capability_id, &replay.estimate, input)?;
        let Some(record) = self
            .stores
            .approval_requests
            .get(&scope, resume.approval_request_id)
            .await
            .map_err(|error| store_error("load_approval", error))?
        else {
            return Ok(Err("the approval is unavailable; re-request approval"));
        };
        if record.status != ApprovalStatus::Approved {
            return Ok(Err(
                "the approval has not been granted; re-request approval",
            ));
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
        let Some(lease) = self
            .stores
            .capability_leases
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
            return Ok(Err(
                "the approval lease is unavailable; re-request approval",
            ));
        };
        Ok(Ok(MatchedLease {
            scope,
            lease_id: lease.grant.id,
            fingerprint,
        }))
    }

    /// Claim and consume the lease before acting, so a cut-short call cannot
    /// replay it. `Some(denial)` when the lease went stale meanwhile.
    async fn spend(&self, lease: &MatchedLease) -> Result<Option<Resolution>, AgentLoopHostError> {
        let leases = &self.stores.capability_leases;
        if let Err(error) = leases
            .claim(&lease.scope, lease.lease_id, &lease.fingerprint)
            .await
        {
            return self.lease_outcome("claim_approval_lease", error).map(Some);
        }
        if let Err(error) = leases.consume(&lease.scope, lease.lease_id).await {
            return self
                .lease_outcome("consume_approval_lease", error)
                .map(Some);
        }
        Ok(None)
    }

    fn denied(&self, summary: &str) -> Result<Resolution, AgentLoopHostError> {
        Self::denied_as("synthetic_approval_required", summary)
    }

    /// Unreadable settings refuse with their own reason, so the model reads a
    /// temporary outage to wait out rather than a permanent policy refusal.
    fn settings_unavailable(&self) -> Result<Resolution, AgentLoopHostError> {
        Self::denied_as(
            "settings_unavailable",
            "tool approval settings could not be read, so this tool was not run; \
             try again shortly",
        )
    }

    fn denied_as(kind: &str, summary: &str) -> Result<Resolution, AgentLoopHostError> {
        let reason = CapabilityDeniedReasonKind::unknown(kind)
            .map_err(|reason| host_error(AgentLoopHostErrorKind::Internal, reason.to_string()))?;
        Ok(resolution::denied(reason, summary.to_string()).resolution)
    }

    /// A stale or lost lease is a recoverable denial; lease storage faults end the run.
    fn lease_outcome(
        &self,
        operation: &'static str,
        error: CapabilityLeaseError,
    ) -> Result<Resolution, AgentLoopHostError> {
        match error {
            CapabilityLeaseError::UnknownLease { .. }
            | CapabilityLeaseError::ExpiredLease { .. }
            | CapabilityLeaseError::ExhaustedLease { .. }
            | CapabilityLeaseError::UnclaimedFingerprintLease { .. }
            | CapabilityLeaseError::FingerprintMismatch { .. }
            | CapabilityLeaseError::InactiveLease { .. } => {
                self.denied("the approval lease is no longer valid; re-request approval")
            }
            CapabilityLeaseError::Persistence { .. }
            | CapabilityLeaseError::VersionMismatch
            | CapabilityLeaseError::CasExhausted => Err(crate::raw_agent_loop_host_error(
                "synthetic_approval",
                operation,
                AgentLoopHostErrorKind::Unavailable,
                "synthetic approval lease operation failed",
                error,
            )),
        }
    }
}

/// A gate over in-memory stores with no settings (global auto-approve off),
/// for test ports. Tests only — ships zero bytes in production.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn in_memory_synthetic_approval_gate_for_test(
    replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
) -> SyntheticApprovalGate {
    SyntheticApprovalGate::new(
        SyntheticApprovalStores {
            approval_requests: Arc::new(
                ironclaw_approvals::in_memory_backed_approval_request_store(),
            ),
            capability_leases: Arc::new(
                ironclaw_authorization::in_memory_backed_capability_lease_store(),
            ),
            approval_settings: Arc::new(ironclaw_approvals::EmptyApprovalSettingsProvider),
            replay_payload_store,
            gate_record_store: Arc::new(ironclaw_approvals::in_memory_backed_gate_record_store()),
        },
        Principal::HostRuntime,
        true,
    )
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
    crate::raw_agent_loop_host_error(
        "synthetic_approval",
        operation,
        AgentLoopHostErrorKind::Unavailable,
        "synthetic approval state operation failed",
        error,
    )
}

#[cfg(test)]
mod tests;
