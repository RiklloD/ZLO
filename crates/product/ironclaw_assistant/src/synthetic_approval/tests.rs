use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ironclaw_approvals::{
    ApprovalResolver, LeaseApproval, in_memory_backed_approval_request_store,
    in_memory_backed_gate_record_store,
};
use ironclaw_authorization::in_memory_backed_capability_lease_store;
use ironclaw_host_api::action::NetworkPolicy;
use ironclaw_host_api::capability::{EffectKind, GrantConstraints};
use ironclaw_host_api::ids::{TenantId, ThreadId};
use ironclaw_host_api::mount::MountView;
use ironclaw_host_api::turn::CapabilityActivityId;
use ironclaw_loop_contracts::{
    CapabilityInputRef, CapabilitySurfaceVersion, InMemoryRunProfileResolver, LoopRequest,
    RunProfileResolutionRequest, RunProfileResolver,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, CapabilityWriteResult, LoopCapabilityResultWriter,
};
use ironclaw_turns::{TurnActor, TurnId, TurnRunId, TurnScope};
use serde_json::json;

use super::*;

const CAPABILITY: &str = "builtin.t3_delegate";

struct Settings {
    tool_override: Option<ToolPermissionOverride>,
    auto_approve: bool,
}

#[async_trait]
impl ApprovalSettingsProvider for Settings {
    async fn tool_override(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
    ) -> Option<ToolPermissionOverride> {
        self.tool_override
    }

    async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
        self.auto_approve
    }

    async fn tool_always_allow(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
        _grantee: &Principal,
    ) -> bool {
        false
    }
}

#[derive(Default)]
struct MemoryReplay(Mutex<HashMap<InvocationId, ReplayPayload>>);

#[async_trait]
impl ReplayPayloadStorePort for MemoryReplay {
    async fn save(
        &self,
        _scope: ResourceScope,
        invocation_id: InvocationId,
        payload: ReplayPayload,
    ) -> Result<(), ReplayPayloadStoreError> {
        self.0.lock().unwrap().insert(invocation_id, payload);
        Ok(())
    }

    async fn load(
        &self,
        _scope: &ResourceScope,
        invocation_id: InvocationId,
    ) -> Result<Option<ReplayPayload>, ReplayPayloadStoreError> {
        Ok(self.0.lock().unwrap().get(&invocation_id).cloned())
    }
}

struct NoWrites;

#[async_trait]
impl LoopCapabilityResultWriter for NoWrites {
    async fn write_capability_result(
        &self,
        _write: CapabilityResultWrite<'_>,
    ) -> Result<CapabilityWriteResult, AgentLoopHostError> {
        Err(host_error(AgentLoopHostErrorKind::Internal, "unused"))
    }
}

fn gate(
    tool_override: Option<ToolPermissionOverride>,
    auto_approve: bool,
) -> SyntheticApprovalGate {
    SyntheticApprovalGate::new(
        SyntheticApprovalStores {
            approval_requests: Arc::new(in_memory_backed_approval_request_store()),
            capability_leases: Arc::new(in_memory_backed_capability_lease_store()),
            approval_settings: Arc::new(Settings {
                tool_override,
                auto_approve,
            }),
            replay_payload_store: Arc::new(MemoryReplay::default()),
            gate_record_store: Arc::new(in_memory_backed_gate_record_store()),
        },
        user(),
        "this action requires approval",
        "test_approval_required",
    )
}

fn user() -> UserId {
    UserId::new("user-gate").unwrap()
}

async fn run() -> LoopRunContext {
    let profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .unwrap();
    LoopRunContext::new(
        TurnScope::new(
            TenantId::new("tenant-gate").unwrap(),
            None,
            None,
            ThreadId::new("thread-gate").unwrap(),
        ),
        TurnId::new(),
        TurnRunId::new(),
        profile,
    )
    .with_actor(TurnActor::new(user()))
}

async fn invocation(resume: Option<CapabilityApprovalResume>) -> SyntheticCapabilityInvocation {
    SyntheticCapabilityInvocation {
        run_context: run().await,
        request: LoopRequest {
            activity_id: CapabilityActivityId::new(),
            surface_version: CapabilitySurfaceVersion::new("v1").unwrap(),
            capability_id: capability(),
            input_ref: CapabilityInputRef::new("input:gate").unwrap(),
            approval_resume: resume,
            auth_resume: None,
        },
        input: json!({"task": "write the report"}),
        result_writer: Arc::new(NoWrites),
    }
}

fn capability() -> CapabilityId {
    CapabilityId::new(CAPABILITY).unwrap()
}

async fn check(
    gate: &SyntheticApprovalGate,
    resume: Option<CapabilityApprovalResume>,
) -> GateOutcome {
    gate.check(&invocation(resume).await, &capability(), || {
        "test".to_string()
    })
    .await
    .unwrap()
}

fn kind(outcome: &GateOutcome) -> &'static str {
    match outcome {
        GateOutcome::Proceed => "proceed",
        GateOutcome::Respond(resolution) => resolution.kind(),
    }
}

/// The one pending request a gate raised, and the resume that answers it.
async fn pending(
    gate: &SyntheticApprovalGate,
) -> (ResourceScope, ApprovalRequestId, CapabilityApprovalResume) {
    let scope = run().await.acting_resource_scope(&user());
    let records = gate
        .stores
        .approval_requests
        .records_for_scope(&scope)
        .await
        .unwrap();
    assert_eq!(records.len(), 1, "exactly one durable approval request");
    let record = &records[0];
    let resume = CapabilityApprovalResume {
        approval_request_id: record.request.id,
        resume_token: CapabilityResumeToken::new(record.scope.invocation_id.to_string()).unwrap(),
        correlation_id: record.request.correlation_id,
        input_ref: CapabilityInputRef::new("input:gate").unwrap(),
    };
    (record.scope.clone(), record.request.id, resume)
}

#[tokio::test]
async fn auto_approve_off_asks_and_an_approved_resume_acts_exactly_once() {
    let gate = gate(None, false);
    let first = check(&gate, None).await;
    assert_eq!(
        kind(&first),
        "blocked",
        "auto-approve off raises an approval gate"
    );
    let (scope, request_id, resume) = pending(&gate).await;

    // Resuming before the user answers is a denial, not an action.
    assert_eq!(kind(&check(&gate, Some(resume.clone())).await), "denied");

    ApprovalResolver::new(
        gate.stores.approval_requests.as_ref(),
        gate.stores.capability_leases.as_ref(),
    )
    .approve_dispatch(
        &scope,
        request_id,
        LeaseApproval {
            issued_by: Principal::HostRuntime,
            constraints: GrantConstraints {
                allowed_effects: vec![EffectKind::DispatchCapability],
                mounts: MountView::default(),
                network: NetworkPolicy::default(),
                secrets: Vec::new(),
                resource_ceiling: None,
                expires_at: None,
                max_invocations: Some(1),
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(
        kind(&check(&gate, Some(resume.clone())).await),
        "proceed",
        "an approved resume acts"
    );
    assert_eq!(
        kind(&check(&gate, Some(resume)).await),
        "denied",
        "the lease is consumed: replaying the approved resume does not act twice"
    );
}

#[tokio::test]
async fn settings_decide_before_asking() {
    assert_eq!(
        kind(&check(&gate(None, true), None).await),
        "proceed",
        "auto-approve on runs it"
    );
    assert_eq!(
        kind(&check(&gate(Some(ToolPermissionOverride::Disabled), true), None).await),
        "denied",
        "a disabled tool is refused even with auto-approve on"
    );
    let asking = gate(Some(ToolPermissionOverride::AskEachTime), true);
    assert_eq!(
        kind(&check(&asking, None).await),
        "blocked",
        "ask-each-time asks even with auto-approve on"
    );
    pending(&asking).await;
}
