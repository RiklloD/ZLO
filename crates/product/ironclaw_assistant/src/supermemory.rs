//! `supermemory.search` and `supermemory.write` as synthetic loop
//! capabilities, offered only to runs Supermemory is on for (key set and a
//! container binding). Those runs also lose the local `ironclaw.memory.*`
//! tools, so Supermemory is their only memory.
//!
//! Search is read-only and ungated, like `ironclaw.memory.search`. Write is
//! gated like `ironclaw.memory.write`: unless tool settings allow it, the
//! call raises a durable approval request and resumes only with a matching,
//! claimed lease. The gate follows `builtin.notification_channels_set`'s
//! approval dance step for step.

use std::sync::Arc;

use async_trait::async_trait;
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
    capability::CapabilityGrant,
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
    CapabilityDeniedReasonKind, CapabilityProgress, CapabilityResumeToken, LoopRunContext,
    resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticCapability, SyntheticCapabilityDescriptor,
    SyntheticCapabilityHandler, SyntheticCapabilityInvocation,
};
use ironclaw_supermemory::{
    SUPERMEMORY_SEARCH_CAPABILITY_ID, SUPERMEMORY_SEARCH_DESCRIPTION, SUPERMEMORY_SEARCH_TOOL_NAME,
    SUPERMEMORY_WRITE_CAPABILITY_ID, SUPERMEMORY_WRITE_DESCRIPTION, SUPERMEMORY_WRITE_TOOL_NAME,
    Supermemory, SupermemoryCaller, hides_local_memory_capability,
};
use ironclaw_turns::LoopGateRef;
use serde_json::Value;

/// Fixed, interpolation-free approval text shared by the gate record and the
/// loop outcome.
const WRITE_APPROVAL_SUMMARY: &str = "saving to Supermemory requires approval";

/// The approval stores the write gate persists to, all owned by composition.
pub struct SupermemoryApprovalStores {
    pub approval_requests: Arc<dyn ApprovalRequestStorePort>,
    pub capability_leases: Arc<dyn CapabilityLeaseStorePort>,
    pub approval_settings: Arc<dyn ApprovalSettingsProvider>,
    pub replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
    pub gate_record_store: Arc<dyn GateRecordStorePort>,
    /// Whether external writes need approval under the runtime policy.
    pub write_requires_approval: bool,
}

fn caller_for_run(run: &LoopRunContext, fallback_user_id: &UserId) -> SupermemoryCaller {
    SupermemoryCaller::from_turn_scope(&run.scope, run.acting_user_id(fallback_user_id))
}

/// The Supermemory service when this run uses it instead of local memory,
/// with the local `ironclaw.memory.*` grants removed from `grants`. `None`
/// (grants untouched) when no service is wired or it is off for the caller.
pub async fn active_for_run<'a>(
    supermemory: Option<&'a Arc<Supermemory>>,
    run: &LoopRunContext,
    fallback_user_id: &UserId,
    grants: &mut Vec<CapabilityGrant>,
) -> Option<&'a Arc<Supermemory>> {
    let supermemory = supermemory?;
    if !supermemory
        .is_active(&caller_for_run(run, fallback_user_id))
        .await
    {
        return None;
    }
    grants.retain(|grant| !hides_local_memory_capability(grant.capability.as_str()));
    Some(supermemory)
}

/// The two Supermemory tools for one run's capability surface.
pub fn supermemory_capabilities(
    supermemory: Arc<Supermemory>,
    stores: SupermemoryApprovalStores,
    fallback_user_id: UserId,
) -> Result<Vec<SyntheticCapability>, AgentLoopHostError> {
    let search = SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            SUPERMEMORY_SEARCH_CAPABILITY_ID,
            SUPERMEMORY_SEARCH_TOOL_NAME,
            SUPERMEMORY_SEARCH_DESCRIPTION,
            ironclaw_supermemory::search_schema(),
        )?,
        Arc::new(SearchHandler {
            supermemory: Arc::clone(&supermemory),
            fallback_user_id: fallback_user_id.clone(),
        }),
    );
    let write = SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            SUPERMEMORY_WRITE_CAPABILITY_ID,
            SUPERMEMORY_WRITE_TOOL_NAME,
            SUPERMEMORY_WRITE_DESCRIPTION,
            ironclaw_supermemory::write_schema(),
        )?,
        Arc::new(WriteHandler {
            supermemory,
            stores,
            fallback_user_id,
        }),
    );
    Ok(vec![search, write])
}

fn validate(capability_id: &str, arguments: &Value) -> Result<(), AgentLoopHostError> {
    ironclaw_supermemory::validate_arguments(capability_id, arguments).map_err(|error| {
        AgentLoopHostError::new(AgentLoopHostErrorKind::InvalidInvocation, error.to_string())
    })
}

async fn complete(
    invocation: &SyntheticCapabilityInvocation,
    output: Value,
    name: &str,
) -> Result<Resolution, AgentLoopHostError> {
    let summary = match output.get("ok") {
        Some(Value::Bool(false)) => format!(
            "{name} failed: {}",
            output
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("error")
        ),
        _ => format!("{name} done"),
    };
    let input_ref = invocation
        .request
        .approval_resume
        .as_ref()
        .map(|resume| &resume.input_ref)
        .unwrap_or(&invocation.request.input_ref);
    let write = invocation
        .result_writer
        .write_capability_result(CapabilityResultWrite {
            run_context: &invocation.run_context,
            input_ref,
            invocation_id: InvocationId::new(),
            capability_id: &invocation.request.capability_id,
            output,
            display_preview: None,
            durable_persistence: DurablePersistence::Persist,
        })
        .await?;
    Ok(resolution::completed(
        write.result_ref,
        summary,
        CapabilityProgress::MadeProgress,
        false,
        write.byte_len,
        write.output_digest,
        write.model_observation,
    ))
}

struct SearchHandler {
    supermemory: Arc<Supermemory>,
    fallback_user_id: UserId,
}

#[async_trait]
impl SyntheticCapabilityHandler for SearchHandler {
    fn validate_provider_arguments(&self, arguments: &Value) -> Result<(), AgentLoopHostError> {
        validate(SUPERMEMORY_SEARCH_CAPABILITY_ID, arguments)
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let caller = caller_for_run(&invocation.run_context, &self.fallback_user_id);
        let output =
            ironclaw_supermemory::run_search(&self.supermemory, &caller, &invocation.input).await;
        complete(&invocation, output, SUPERMEMORY_SEARCH_TOOL_NAME).await
    }
}

struct WriteHandler {
    supermemory: Arc<Supermemory>,
    stores: SupermemoryApprovalStores,
    fallback_user_id: UserId,
}

enum SettingsDecision {
    Allow,
    Ask,
    Deny,
}

struct ApprovedLease {
    scope: ResourceScope,
    lease_id: CapabilityGrantId,
}

#[async_trait]
impl SyntheticCapabilityHandler for WriteHandler {
    fn validate_provider_arguments(&self, arguments: &Value) -> Result<(), AgentLoopHostError> {
        validate(SUPERMEMORY_WRITE_CAPABILITY_ID, arguments)
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        if invocation.request.auth_resume.is_some() {
            return Err(host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                "supermemory write does not support auth resume",
            ));
        }
        let capability_id = write_capability_id()?;
        let lease = match invocation.request.approval_resume.clone() {
            Some(resume) if self.stores.write_requires_approval => {
                match self
                    .verify_resume(&invocation, &resume, &capability_id)
                    .await?
                {
                    Ok(lease) => Some(lease),
                    Err(denied) => return Ok(denied),
                }
            }
            Some(_) => {
                return Err(host_error(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "supermemory write approval resume is not expected",
                ));
            }
            None if self.stores.write_requires_approval => {
                match self.settings_decision(&invocation, &capability_id).await? {
                    SettingsDecision::Allow => None,
                    SettingsDecision::Ask => {
                        return self.request_approval(&invocation, &capability_id).await;
                    }
                    SettingsDecision::Deny => {
                        return denied("supermemory write is disabled by tool approval settings");
                    }
                }
            }
            None => None,
        };
        let caller = caller_for_run(&invocation.run_context, &self.fallback_user_id);
        let output =
            ironclaw_supermemory::run_write(&self.supermemory, &caller, &invocation.input).await;
        if let Some(lease) = lease
            && let Err(error) = self
                .stores
                .capability_leases
                .consume(&lease.scope, lease.lease_id)
                .await
        {
            return lease_outcome("consume_approval_lease", error);
        }
        complete(&invocation, output, SUPERMEMORY_WRITE_TOOL_NAME).await
    }
}

impl WriteHandler {
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

    async fn settings_decision(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        capability_id: &CapabilityId,
    ) -> Result<SettingsDecision, AgentLoopHostError> {
        let settings = &self.stores.approval_settings;
        let scope = self.settings_scope(&invocation.run_context);
        match settings.tool_override(&scope, capability_id).await {
            Some(ToolPermissionOverride::Disabled) => return Ok(SettingsDecision::Deny),
            Some(ToolPermissionOverride::AskEachTime) => return Ok(SettingsDecision::Ask),
            None => {}
        }
        if settings
            .tool_always_allow(&scope, capability_id, &grantee()?)
            .await
            || settings.global_auto_approve(&scope).await
        {
            return Ok(SettingsDecision::Allow);
        }
        Ok(SettingsDecision::Ask)
    }

    async fn request_approval(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        capability_id: &CapabilityId,
    ) -> Result<Resolution, AgentLoopHostError> {
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
                    reason: "Store a memory in Supermemory (primary container)".to_string(),
                    reusable_scope: None,
                },
            )
            .await
            .map_err(|error| store_error("save_pending_approval", error))?;
        let summary = SafeSummary::new(WRITE_APPROVAL_SUMMARY).map_err(|error| {
            ironclaw_loop_host::raw_agent_loop_host_error(
                "supermemory_write",
                "gate_record_summary",
                AgentLoopHostErrorKind::Internal,
                "supermemory gate summary is not renderable",
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
        Ok(resolution::approval_required(
            gate_ref,
            WRITE_APPROVAL_SUMMARY.to_string(),
            Some(CapabilityApprovalResume {
                approval_request_id,
                resume_token,
                correlation_id,
                input_ref: invocation.request.input_ref.clone(),
            }),
        )
        .resolution)
    }

    /// `Ok(Ok(lease))` to dispatch, `Ok(Err(denial))` for a lost or
    /// ungranted approval the user can re-request, `Err` for a mismatch.
    async fn verify_resume(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        resume: &CapabilityApprovalResume,
        capability_id: &CapabilityId,
    ) -> Result<Result<ApprovedLease, Resolution>, AgentLoopHostError> {
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
                    "supermemory approval replay payload is unavailable",
                )
            })?;
        let mismatch = |what: &str| {
            host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                format!("supermemory approval {what} does not match"),
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
            return Ok(Err(denied(
                "supermemory approval is unavailable; re-request approval",
            )?));
        };
        if record.status != ApprovalStatus::Approved {
            return Ok(Err(denied(
                "supermemory approval has not been granted; re-request approval",
            )?));
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
            return Ok(Err(denied(
                "supermemory approval lease is unavailable; re-request approval",
            )?));
        };
        if let Err(error) = leases.claim(&scope, lease.grant.id, &fingerprint).await {
            return lease_outcome("claim_approval_lease", error).map(Err);
        }
        Ok(Ok(ApprovedLease {
            scope,
            lease_id: lease.grant.id,
        }))
    }
}

fn write_capability_id() -> Result<CapabilityId, AgentLoopHostError> {
    CapabilityId::new(SUPERMEMORY_WRITE_CAPABILITY_ID)
        .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))
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

fn denied(summary: &str) -> Result<Resolution, AgentLoopHostError> {
    let reason = CapabilityDeniedReasonKind::unknown("supermemory_approval_required")
        .map_err(|reason| host_error(AgentLoopHostErrorKind::Internal, reason.to_string()))?;
    Ok(resolution::denied(reason, summary.to_string()).resolution)
}

fn replay_error(error: ReplayPayloadStoreError) -> AgentLoopHostError {
    tracing::warn!(error = %error, "failed to access supermemory replay payload");
    host_error(
        AgentLoopHostErrorKind::Unavailable,
        "failed to access capability replay payload",
    )
}

fn store_error(operation: &'static str, error: ApprovalStoreError) -> AgentLoopHostError {
    ironclaw_loop_host::raw_agent_loop_host_error(
        "supermemory_write",
        operation,
        AgentLoopHostErrorKind::Unavailable,
        "supermemory approval state operation failed",
        error,
    )
}

/// A stale or lost lease is a recoverable denial; lease storage faults end the run.
fn lease_outcome(
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
            denied("supermemory approval lease is no longer valid; re-request approval")
        }
        CapabilityLeaseError::Persistence { .. }
        | CapabilityLeaseError::VersionMismatch
        | CapabilityLeaseError::CasExhausted => Err(ironclaw_loop_host::raw_agent_loop_host_error(
            "supermemory_write",
            operation,
            AgentLoopHostErrorKind::Unavailable,
            "supermemory approval lease operation failed",
            error,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ironclaw_host_api::capability::GrantConstraints;
    use ironclaw_host_api::ids::{SecretHandle, TenantId, ThreadId};
    use ironclaw_loop_contracts::{
        InMemoryRunProfileResolver, RunProfileResolutionRequest, RunProfileResolver,
    };
    use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
    use ironclaw_product_contracts::test_support::fakes::FakeOperatorSecretValueStore;
    use ironclaw_supermemory::{
        SUPERMEMORY_API_KEY_SECRET, StaticSupermemoryBindingSource, SupermemoryClient,
        SupermemoryConfig,
    };
    use ironclaw_turns::{TurnId, TurnRunId, TurnScope};

    async fn supermemory(with_key: bool) -> Arc<Supermemory> {
        let keys = Arc::new(FakeOperatorSecretValueStore::default());
        if with_key {
            keys.put(
                &SecretHandle::new(SUPERMEMORY_API_KEY_SECRET).unwrap(),
                secrecy::SecretString::from("sm_test_key_not_real".to_string()),
            )
            .await
            .unwrap();
        }
        // The activity check reads only the key and binding; it sends nothing.
        let config = SupermemoryConfig::default();
        let client = SupermemoryClient::production(&config, keys).unwrap();
        let bindings = Arc::new(StaticSupermemoryBindingSource::new(Some(
            ironclaw_supermemory::SupermemoryBinding {
                primary_container: "ironclaw".to_string(),
                read_containers: vec!["career".to_string()],
                instructions: None,
            },
        )));
        Arc::new(Supermemory::new(client, bindings, config))
    }

    async fn run() -> LoopRunContext {
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        LoopRunContext::new(
            TurnScope::new(
                TenantId::new("tenant-sm").unwrap(),
                None,
                None,
                ThreadId::new("thread-sm").unwrap(),
            ),
            TurnId::new(),
            TurnRunId::new(),
            profile,
        )
    }

    fn grant(id: &str) -> CapabilityGrant {
        CapabilityGrant {
            id: CapabilityGrantId::new(),
            capability: CapabilityId::new(id).unwrap(),
            grantee: grantee().unwrap(),
            issued_by: grantee().unwrap(),
            constraints: GrantConstraints {
                allowed_effects: Vec::new(),
                mounts: Default::default(),
                network: Default::default(),
                secrets: Vec::new(),
                resource_ceiling: None,
                expires_at: None,
                max_invocations: None,
            },
        }
    }

    fn grants() -> Vec<CapabilityGrant> {
        [
            "ironclaw.memory.write",
            "ironclaw.memory.search",
            "builtin.project_create",
        ]
        .into_iter()
        .map(grant)
        .collect()
    }

    #[tokio::test]
    async fn key_and_binding_swap_local_memory_tools_for_supermemory() {
        let user = UserId::new("user-sm").unwrap();
        let run = run().await;

        let off = supermemory(false).await;
        let mut untouched = grants();
        assert!(
            active_for_run(Some(&off), &run, &user, &mut untouched)
                .await
                .is_none()
        );
        assert_eq!(untouched.len(), 3, "no key: local memory tools stay");
        assert!(
            active_for_run(None, &run, &user, &mut untouched)
                .await
                .is_none()
        );

        let on = supermemory(true).await;
        let mut swapped = grants();
        let active = active_for_run(Some(&on), &run, &user, &mut swapped).await;
        assert!(active.is_some());
        let left: Vec<&str> = swapped
            .iter()
            .map(|grant| grant.capability.as_str())
            .collect();
        assert_eq!(left, ["builtin.project_create"]);
    }
}
