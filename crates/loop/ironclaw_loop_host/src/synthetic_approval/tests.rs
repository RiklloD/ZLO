//! The approval gate at the synthetic dispatch boundary, driven through
//! `wrap_synthetic_capabilities` — the one entry every synthetic call takes.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ironclaw_approvals::{
    ApprovalResolver, LeaseApproval, ToolPermissionOverride,
    in_memory_backed_approval_request_store, in_memory_backed_gate_record_store,
};
use ironclaw_authorization::in_memory_backed_capability_lease_store;
use ironclaw_capabilities::ReplayPayloadStore;
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::{
    action::NetworkPolicy,
    capability::{EffectKind, GrantConstraints},
    ids::{AgentId, TenantId, ThreadId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
    turn::CapabilityActivityId,
};
use ironclaw_loop_contracts::{
    CapabilityInputRef, CapabilityProgress, InMemoryRunProfileResolver, LoopCapabilityPort,
    ProviderToolCall, RegisterProviderToolCallRequest, RunProfileResolutionRequest,
    RunProfileResolver, VisibleCapabilityRequest,
};
use ironclaw_turns::{LoopResultRef, TurnId, TurnRunId, TurnScope};
use serde_json::json;

use super::*;
use crate::{
    CapabilityResultWrite, CapabilityWriteResult, EmptyLoopCapabilityPort,
    LoopCapabilityInputResolver, LoopCapabilityResultWriter, SyntheticCapability,
    SyntheticCapabilityDescriptor, SyntheticCapabilityEffect, SyntheticCapabilityHandler,
    SyntheticCapabilityInvocation, wrap_synthetic_capabilities,
};

const CAPABILITY: &str = "test.mutating";
const TOOL: &str = "test__mutating";

/// Settings the test mutates between calls. `global_auto_approve` answers
/// only for a scope WITHOUT a thread; a thread-scoped question gets
/// `thread_auto_approve` — the shape of a thread profile set to "Ask first".
struct Settings {
    tool_override: StdMutex<Option<ToolPermissionOverride>>,
    global_auto_approve: bool,
    thread_auto_approve: bool,
    always_allow: bool,
    full_access: bool,
}

impl Settings {
    fn new(global_auto_approve: bool) -> Arc<Self> {
        Arc::new(Self {
            tool_override: StdMutex::new(None),
            global_auto_approve,
            thread_auto_approve: global_auto_approve,
            always_allow: false,
            full_access: false,
        })
    }

    fn set_override(&self, value: Option<ToolPermissionOverride>) {
        *self.tool_override.lock().unwrap() = value;
    }
}

#[async_trait]
impl ApprovalSettingsProvider for Settings {
    async fn tool_override_lookup(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
    ) -> ironclaw_approvals::ToolOverrideLookup {
        ironclaw_approvals::ToolOverrideLookup::from_setting(*self.tool_override.lock().unwrap())
    }

    async fn global_auto_approve(&self, scope: &ResourceScope) -> bool {
        if scope.thread_id.is_some() {
            self.thread_auto_approve
        } else {
            self.global_auto_approve
        }
    }

    async fn tool_always_allow(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
        _grantee: &Principal,
    ) -> bool {
        self.always_allow
    }

    async fn full_access(&self, _scope: &ResourceScope) -> bool {
        self.full_access
    }
}

struct Counting(Arc<AtomicUsize>);

#[async_trait]
impl SyntheticCapabilityHandler for Counting {
    fn validate_provider_arguments(&self, _arguments: &Value) -> Result<(), AgentLoopHostError> {
        Ok(())
    }

    async fn invoke(
        &self,
        _invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(resolution::completed(
            LoopResultRef::new("result:mutating").unwrap(),
            "done".to_string(),
            CapabilityProgress::MadeProgress,
            false,
            0,
            None,
            None,
        ))
    }
}

struct Input;

#[async_trait]
impl LoopCapabilityInputResolver for Input {
    async fn resolve_capability_input(
        &self,
        _run_context: &LoopRunContext,
        _input_ref: &CapabilityInputRef,
    ) -> Result<Value, AgentLoopHostError> {
        Ok(json!({"name": "report"}))
    }

    async fn register_provider_tool_call_input(
        &self,
        _run_context: &LoopRunContext,
        tool_call: &ProviderToolCall,
    ) -> Result<CapabilityInputRef, AgentLoopHostError> {
        Ok(CapabilityInputRef::new(format!("input:{}", tool_call.id)).unwrap())
    }
}

struct NoWrites;

#[async_trait]
impl LoopCapabilityResultWriter for NoWrites {
    async fn write_capability_result(
        &self,
        _write: CapabilityResultWrite<'_>,
    ) -> Result<CapabilityWriteResult, AgentLoopHostError> {
        Ok(CapabilityWriteResult::without_output_digest(
            LoopResultRef::new("result:mutating").unwrap(),
            0,
        ))
    }
}

struct Fixture {
    port: Arc<dyn LoopCapabilityPort>,
    stores: SyntheticApprovalStores,
    settings: Arc<Settings>,
    gate: Arc<SyntheticApprovalGate>,
    approval: SyntheticApproval,
    runs: Arc<AtomicUsize>,
    run: LoopRunContext,
}

fn user() -> UserId {
    UserId::new("user-gate").unwrap()
}

fn replay_filesystem() -> Arc<ScopedFilesystem<InMemoryBackend>> {
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/replay-payloads").unwrap(),
        VirtualPath::new("/tenants/test/users/test/replay-payloads").unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    Arc::new(ScopedFilesystem::with_fixed_view(
        Arc::new(InMemoryBackend::new()),
        mounts,
    ))
}

async fn fixture(settings: Arc<Settings>) -> Fixture {
    fixture_with(
        settings,
        SyntheticApproval::new(
            "this action requires approval",
            "Run the mutating test capability",
        ),
    )
    .await
}

async fn fixture_with(settings: Arc<Settings>, approval: SyntheticApproval) -> Fixture {
    let stores = SyntheticApprovalStores {
        approval_requests: Arc::new(in_memory_backed_approval_request_store()),
        capability_leases: Arc::new(in_memory_backed_capability_lease_store()),
        approval_settings: settings.clone(),
        replay_payload_store: Arc::new(ReplayPayloadStore::new(replay_filesystem())),
        gate_record_store: Arc::new(in_memory_backed_gate_record_store()),
    };
    let gate = Arc::new(SyntheticApprovalGate::new(
        stores.clone(),
        Principal::Extension(ironclaw_host_api::ids::ExtensionId::new("builtin").unwrap()),
        true,
    ));
    port_over(
        gate,
        stores,
        settings,
        approval,
        Arc::new(AtomicUsize::new(0)),
        TurnId::new(),
    )
    .await
}

/// A port for one turn of the run over an existing gate (and its per-task
/// approval state), stores and settings.
async fn port_over(
    gate: Arc<SyntheticApprovalGate>,
    stores: SyntheticApprovalStores,
    settings: Arc<Settings>,
    approval: SyntheticApproval,
    runs: Arc<AtomicUsize>,
    turn: TurnId,
) -> Fixture {
    let profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .unwrap();
    let run = LoopRunContext::new(
        TurnScope::new(
            TenantId::new("tenant-gate").unwrap(),
            Some(AgentId::new("agent-gate").unwrap()),
            None,
            ThreadId::new("thread-gate").unwrap(),
        ),
        turn,
        TurnRunId::new(),
        profile,
    )
    .with_actor(ironclaw_turns::TurnActor::new(user()));
    let capability = SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            CAPABILITY,
            TOOL,
            "A mutating test capability",
            json!({"type": "object"}),
            SyntheticCapabilityEffect::Mutating(approval.clone()),
        )
        .unwrap(),
        Arc::new(Counting(Arc::clone(&runs))),
    );
    let port = wrap_synthetic_capabilities(
        Arc::new(EmptyLoopCapabilityPort),
        vec![capability],
        run.clone(),
        user(),
        Arc::new(Input),
        Arc::new(NoWrites),
        None,
        Arc::clone(&gate),
    )
    .unwrap();
    port.visible_capabilities(VisibleCapabilityRequest {})
        .await
        .unwrap();
    Fixture {
        port,
        stores,
        settings,
        runs,
        run,
        gate,
        approval,
    }
}

impl Fixture {
    /// The same gate, stores and settings in another turn (another task).
    async fn next_turn(&self) -> Fixture {
        port_over(
            Arc::clone(&self.gate),
            self.stores.clone(),
            Arc::clone(&self.settings),
            self.approval.clone(),
            Arc::clone(&self.runs),
            TurnId::new(),
        )
        .await
    }

    /// A fresh model call of the capability: registered, then invoked.
    async fn call(&self) -> (Resolution, LoopRequest) {
        let activity_id = CapabilityActivityId::new();
        let candidate = self
            .port
            .register_provider_tool_call(RegisterProviderToolCallRequest::for_activity(
                ProviderToolCall {
                    provider_id: "test-provider".to_string(),
                    provider_model_id: "test-model".to_string(),
                    turn_id: Some("provider-turn".to_string()),
                    id: format!("call-{}", ApprovalRequestId::new()),
                    name: ironclaw_host_api::ids::ProviderToolName::new(TOOL).unwrap(),
                    arguments: json!({"name": "report"}),
                    response_reasoning: None,
                    response_text: None,
                    reasoning: None,
                    signature: None,
                },
                activity_id,
            ))
            .await
            .unwrap();
        let request = LoopRequest {
            activity_id,
            surface_version: candidate.surface_version,
            capability_id: candidate.capability_id,
            input_ref: candidate.input_ref,
            approval_resume: None,
            auth_resume: None,
        };
        (self.invoke(request.clone()).await, request)
    }

    /// The loop's resume of a parked call: the same activity and input, with
    /// the approval it was answered with.
    async fn resume(&self, parked: &LoopRequest, resume: CapabilityApprovalResume) -> Resolution {
        let mut request = parked.clone();
        request.approval_resume = Some(resume);
        self.invoke(request).await
    }

    async fn invoke(&self, request: LoopRequest) -> Resolution {
        self.port.invoke_capability(request).await.unwrap()
    }

    /// The one pending approval a call raised, the resume that answers it,
    /// and the scope its records live under.
    async fn pending(&self, parked: &LoopRequest) -> (ResourceScope, CapabilityApprovalResume) {
        let scope = self.run.acting_resource_scope(&user());
        let records = self
            .stores
            .approval_requests
            .records_for_scope(&scope)
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "exactly one durable approval request");
        let record = &records[0];
        let resume = CapabilityApprovalResume {
            approval_request_id: record.request.id,
            resume_token: CapabilityResumeToken::new(record.scope.invocation_id.to_string())
                .unwrap(),
            correlation_id: record.request.correlation_id,
            input_ref: parked.input_ref.clone(),
        };
        (record.scope.clone(), resume)
    }

    async fn approve(&self, scope: &ResourceScope, request_id: ApprovalRequestId) {
        ApprovalResolver::new(
            self.stores.approval_requests.as_ref(),
            self.stores.capability_leases.as_ref(),
        )
        .approve_dispatch(
            scope,
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
    }

    async fn lease_status(&self, scope: &ResourceScope) -> CapabilityLeaseStatus {
        let leases = self.stores.capability_leases.leases_for_scope(scope).await;
        assert_eq!(leases.len(), 1, "one approval lease");
        leases[0].status
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn mutating_synthetic_parks_until_approved_and_acts_exactly_once() {
    let gate = fixture(Settings::new(false)).await;
    let (first, parked) = gate.call().await;
    assert!(matches!(first, Resolution::Blocked(_)));
    assert_eq!(gate.runs(), 0, "nothing runs before the approval");
    let (scope, resume) = gate.pending(&parked).await;

    assert_eq!(gate.resume(&parked, resume.clone()).await.kind(), "denied");
    assert_eq!(gate.runs(), 0, "an unanswered approval never acts");

    gate.approve(&scope, resume.approval_request_id).await;
    assert_eq!(gate.resume(&parked, resume.clone()).await.kind(), "done");
    assert_eq!(gate.runs(), 1);
    assert_eq!(
        gate.lease_status(&scope).await,
        CapabilityLeaseStatus::Consumed
    );
    assert_eq!(
        gate.resume(&parked, resume).await.kind(),
        "denied",
        "the consumed lease cannot be replayed"
    );
    assert_eq!(gate.runs(), 1);
}

#[tokio::test]
async fn a_disabled_mutating_synthetic_is_refused_even_with_auto_approve_on() {
    let gate = fixture(Settings::new(true)).await;
    gate.settings
        .set_override(Some(ToolPermissionOverride::Disabled));
    assert_eq!(gate.call().await.0.kind(), "denied");
    assert_eq!(gate.runs(), 0);
    assert!(
        gate.stores
            .approval_requests
            .records_for_scope(&gate.run.acting_resource_scope(&user()))
            .await
            .unwrap()
            .is_empty(),
        "a disabled tool raises no approval request"
    );

    gate.settings.set_override(None);
    assert_eq!(
        gate.call().await.0.kind(),
        "done",
        "auto-approve on runs it"
    );
}

/// Finding 3: approve, then disable, then resume — refused, nothing runs, and
/// the approval's lease is not spent.
#[tokio::test]
async fn disabling_after_approval_refuses_the_resume_without_spending_the_lease() {
    let gate = fixture(Settings::new(false)).await;
    let (_, parked) = gate.call().await;
    let (scope, resume) = gate.pending(&parked).await;
    gate.approve(&scope, resume.approval_request_id).await;

    gate.settings
        .set_override(Some(ToolPermissionOverride::Disabled));
    assert_eq!(gate.resume(&parked, resume).await.kind(), "denied");
    assert_eq!(
        gate.runs(),
        0,
        "a disabled tool never acts on an old approval"
    );
    assert_eq!(
        gate.lease_status(&scope).await,
        CapabilityLeaseStatus::Active,
        "a refused resume does not claim or consume the lease"
    );
}

/// Finding 2: the gate asks with the invocation's thread scope, so a thread
/// profile set to "Ask first" wins over global auto-approve ON.
#[tokio::test]
async fn a_thread_that_asks_first_is_not_overridden_by_global_auto_approve() {
    let settings = Arc::new(Settings {
        tool_override: StdMutex::new(None),
        global_auto_approve: true,
        thread_auto_approve: false,
        always_allow: false,
        full_access: false,
    });
    let gate = fixture(settings).await;
    assert!(
        matches!(gate.call().await.0, Resolution::Blocked(_)),
        "the thread's own answer (ask) decides, not the thread-less global switch"
    );
    assert_eq!(gate.runs(), 0);
}

/// Computer use's policy: global auto-approve never lets it run.
fn desktop_approval() -> SyntheticApproval {
    SyntheticApproval::new("computer use requires approval", "Use this Mac")
        .ignoring_global_auto_approve()
        .lasting_for_task()
}

fn settings(always_allow: bool, full_access: bool) -> Arc<Settings> {
    Arc::new(Settings {
        tool_override: StdMutex::new(None),
        global_auto_approve: true,
        thread_auto_approve: true,
        always_allow,
        full_access,
    })
}

#[tokio::test]
async fn ignoring_global_auto_approve_still_asks_but_always_allow_runs_unless_ask_each_time() {
    let asks = fixture_with(settings(false, false), desktop_approval()).await;
    assert!(
        matches!(asks.call().await.0, Resolution::Blocked(_)),
        "global auto-approve alone never runs it"
    );

    let allowed = fixture_with(settings(true, false), desktop_approval()).await;
    assert_eq!(
        allowed.call().await.0.kind(),
        "done",
        "always-allow runs it"
    );
    allowed
        .settings
        .set_override(Some(ToolPermissionOverride::AskEachTime));
    assert!(
        matches!(allowed.call().await.0, Resolution::Blocked(_)),
        "ask-each-time beats always-allow"
    );
}

#[tokio::test]
async fn full_access_never_asks_even_with_ask_each_time_but_disabled_denies() {
    let gate = fixture_with(settings(false, true), desktop_approval()).await;
    assert_eq!(gate.call().await.0.kind(), "done");
    gate.settings
        .set_override(Some(ToolPermissionOverride::AskEachTime));
    assert_eq!(
        gate.call().await.0.kind(),
        "done",
        "Full access skips ask-each-time"
    );
    gate.settings
        .set_override(Some(ToolPermissionOverride::Disabled));
    assert_eq!(
        gate.call().await.0.kind(),
        "denied",
        "disabled still denies"
    );
    assert_eq!(gate.runs(), 2);
}

#[tokio::test]
async fn one_approval_lasting_for_the_task_covers_its_later_calls_only() {
    let gate = fixture_with(settings(false, false), desktop_approval()).await;
    let (_, parked) = gate.call().await;
    let (scope, resume) = gate.pending(&parked).await;
    gate.approve(&scope, resume.approval_request_id).await;
    assert_eq!(gate.resume(&parked, resume).await.kind(), "done");
    assert_eq!(
        gate.call().await.0.kind(),
        "done",
        "the approved task runs on"
    );
    assert_eq!(gate.runs(), 2);

    gate.settings
        .set_override(Some(ToolPermissionOverride::AskEachTime));
    assert!(
        matches!(gate.call().await.0, Resolution::Blocked(_)),
        "a tool set to ask each time still asks"
    );

    // The SAME gate in another turn (another task) asks again: the task
    // approval is scoped to the turn it was given in.
    gate.settings.set_override(None);
    let next_task = gate.next_turn().await;
    assert!(
        matches!(next_task.call().await.0, Resolution::Blocked(_)),
        "another task on the same gate asks again"
    );
    assert_eq!(gate.runs(), 2, "nothing ran for the new task");
    // …while the approved turn still runs on.
    assert_eq!(gate.call().await.0.kind(), "done");
}
