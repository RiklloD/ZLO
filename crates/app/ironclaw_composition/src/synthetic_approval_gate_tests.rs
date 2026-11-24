//! The shared synthetic approval gate over the production settings providers
//! (`StoreApprovalSettingsProvider`, wrapped in `ProfileApprovalSettingsProvider`
//! exactly as composition wires them), driven through
//! `ironclaw_loop_host::wrap_synthetic_capabilities` — the boundary every
//! synthetic call crosses.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use ironclaw_approvals::{
    ApprovalResolver, ApprovalSettingsProvider, AutoApproveSettingInput,
    AutoApproveSettingStorePort as _, CapabilityPermissionOverrideInput,
    CapabilityPermissionOverrideKey, CapabilityPermissionOverrideRecord,
    CapabilityPermissionOverrideStorePort, CapabilityPermissionStoreError, LeaseApproval,
    in_memory_backed_approval_request_store, in_memory_backed_gate_record_store,
    test_support::{
        in_memory_backed_auto_approve_setting_store,
        in_memory_backed_capability_permission_override_store,
        in_memory_backed_persistent_approval_policy_store,
    },
};
use ironclaw_authorization::{CapabilityLeaseStatus, in_memory_backed_capability_lease_store};
use ironclaw_capabilities::ReplayPayloadStore;
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::{
    action::NetworkPolicy,
    capability::{EffectKind, GrantConstraints},
    ids::{AgentId, ApprovalRequestId, ExtensionId, ProviderToolName, TenantId, ThreadId, UserId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
    resolution::Resolution,
    resource::ResourceScope,
    scope::Principal,
    turn::CapabilityActivityId,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, CapabilityApprovalResume, CapabilityInputRef, CapabilityProgress,
    CapabilityResumeToken, InMemoryRunProfileResolver, LoopCapabilityPort, LoopRequest,
    LoopRunContext, ProviderToolCall, RegisterProviderToolCallRequest, RunProfileResolutionRequest,
    RunProfileResolver, VisibleCapabilityRequest, resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, CapabilityWriteResult, EmptyLoopCapabilityPort,
    LoopCapabilityInputResolver, LoopCapabilityResultWriter, SyntheticApproval,
    SyntheticApprovalGate, SyntheticApprovalStores, SyntheticCapability,
    SyntheticCapabilityDescriptor, SyntheticCapabilityEffect, SyntheticCapabilityHandler,
    SyntheticCapabilityInvocation, wrap_synthetic_capabilities,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::profiles::{ProfileOwner, parse_profile_slug};
use ironclaw_product_contracts::profiles::{ApprovalMode, ProfileSettings};
use ironclaw_turns::{LoopResultRef, TurnActor, TurnId, TurnRunId, TurnScope};
use serde_json::{Value, json};

use crate::capability_authorization::StoreApprovalSettingsProvider;
use crate::profile_approval_mode::{ProfileApprovalEngineSlot, ProfileApprovalSettingsProvider};

const CAPABILITY: &str = "computer_use.click";
const TOOL: &str = "computer_use_click";

fn tenant() -> TenantId {
    TenantId::new("tenant-gate").unwrap()
}

fn user() -> UserId {
    UserId::new("user-gate").unwrap()
}

/// Computer use's approval policy.
fn desktop() -> SyntheticApproval {
    SyntheticApproval::new("computer use requires approval", "Use this Mac")
        .ignoring_global_auto_approve()
        .lasting_for_task()
}

/// An override store that answers like an empty one until told to fail.
pub(crate) struct FlakyOverrides {
    inner: Arc<dyn CapabilityPermissionOverrideStorePort>,
    pub(crate) failing: AtomicBool,
}

impl FlakyOverrides {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(in_memory_backed_capability_permission_override_store()),
            failing: AtomicBool::new(false),
        })
    }

    fn fail(&self) -> Result<(), CapabilityPermissionStoreError> {
        if self.failing.load(Ordering::SeqCst) {
            Err(CapabilityPermissionStoreError::Filesystem(
                "override store unreadable".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl CapabilityPermissionOverrideStorePort for FlakyOverrides {
    async fn set(
        &self,
        input: CapabilityPermissionOverrideInput,
    ) -> Result<CapabilityPermissionOverrideRecord, CapabilityPermissionStoreError> {
        self.inner.set(input).await
    }

    async fn get(
        &self,
        key: &CapabilityPermissionOverrideKey,
    ) -> Result<Option<CapabilityPermissionOverrideRecord>, CapabilityPermissionStoreError> {
        self.fail()?;
        self.inner.get(key).await
    }

    fn supports_scope_listing(&self) -> bool {
        self.inner.supports_scope_listing()
    }

    async fn list_for_scope(
        &self,
        scope: &ResourceScope,
    ) -> Result<Vec<CapabilityPermissionOverrideRecord>, CapabilityPermissionStoreError> {
        self.fail()?;
        self.inner.list_for_scope(scope).await
    }

    async fn clear(
        &self,
        key: &CapabilityPermissionOverrideKey,
    ) -> Result<(), CapabilityPermissionStoreError> {
        self.inner.clear(key).await
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
            LoopResultRef::new("result:gate").unwrap(),
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
        Ok(json!({"app": "Calculator"}))
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
            LoopResultRef::new("result:gate").unwrap(),
            0,
        ))
    }
}

struct Gate {
    port: Arc<dyn LoopCapabilityPort>,
    stores: SyntheticApprovalStores,
    runs: Arc<AtomicUsize>,
    run: LoopRunContext,
}

async fn run_on(thread: &ThreadId) -> LoopRunContext {
    let profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .unwrap();
    LoopRunContext::new(
        TurnScope::new(
            tenant(),
            Some(AgentId::new("agent-gate").unwrap()),
            None,
            thread.clone(),
        ),
        TurnId::new(),
        TurnRunId::new(),
        profile,
    )
    .with_actor(TurnActor::new(user()))
}

async fn gate(
    settings: Arc<dyn ApprovalSettingsProvider>,
    approval: SyntheticApproval,
    run: LoopRunContext,
) -> Gate {
    let replay = Arc::new(ScopedFilesystem::with_fixed_view(
        Arc::new(InMemoryBackend::new()),
        MountView::new(vec![MountGrant::new(
            MountAlias::new("/replay-payloads").unwrap(),
            VirtualPath::new("/tenants/test/users/test/replay-payloads").unwrap(),
            MountPermissions::read_write_list_delete(),
        )])
        .unwrap(),
    ));
    let stores = SyntheticApprovalStores {
        approval_requests: Arc::new(in_memory_backed_approval_request_store()),
        capability_leases: Arc::new(in_memory_backed_capability_lease_store()),
        approval_settings: settings,
        replay_payload_store: Arc::new(ReplayPayloadStore::new(replay)),
        gate_record_store: Arc::new(in_memory_backed_gate_record_store()),
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let port = wrap_synthetic_capabilities(
        Arc::new(EmptyLoopCapabilityPort),
        vec![SyntheticCapability::new(
            SyntheticCapabilityDescriptor::new(
                CAPABILITY,
                TOOL,
                "A gated test capability",
                json!({"type": "object"}),
                SyntheticCapabilityEffect::Mutating(approval),
            )
            .unwrap(),
            Arc::new(Counting(Arc::clone(&runs))),
        )],
        run.clone(),
        user(),
        Arc::new(Input),
        Arc::new(NoWrites),
        None,
        Arc::new(SyntheticApprovalGate::new(
            stores.clone(),
            Principal::Extension(ExtensionId::new("builtin").unwrap()),
            true,
        )),
    )
    .unwrap();
    port.visible_capabilities(VisibleCapabilityRequest {})
        .await
        .unwrap();
    Gate {
        port,
        stores,
        runs,
        run,
    }
}

impl Gate {
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
                    name: ProviderToolName::new(TOOL).unwrap(),
                    arguments: json!({"app": "Calculator"}),
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
        let resolution = self.port.invoke_capability(request.clone()).await.unwrap();
        (resolution, request)
    }

    async fn resume(&self, parked: &LoopRequest, resume: CapabilityApprovalResume) -> Resolution {
        let mut request = parked.clone();
        request.approval_resume = Some(resume);
        self.port.invoke_capability(request).await.unwrap()
    }

    /// Approve the one pending request; the resume that answers it and the
    /// scope its lease lives under.
    async fn approve_pending(
        &self,
        parked: &LoopRequest,
    ) -> (ResourceScope, CapabilityApprovalResume) {
        let scope = self.run.acting_resource_scope(&user());
        let records = self
            .stores
            .approval_requests
            .records_for_scope(&scope)
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "exactly one durable approval request");
        let record = &records[0];
        ApprovalResolver::new(
            self.stores.approval_requests.as_ref(),
            self.stores.capability_leases.as_ref(),
        )
        .approve_dispatch(
            &record.scope,
            record.request.id,
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
        (
            record.scope.clone(),
            CapabilityApprovalResume {
                approval_request_id: record.request.id,
                resume_token: CapabilityResumeToken::new(record.scope.invocation_id.to_string())
                    .unwrap(),
                correlation_id: record.request.correlation_id,
                input_ref: parked.input_ref.clone(),
            },
        )
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

/// The production settings stack: store-backed settings under the
/// profile-aware wrapper, the engine slot filled.
pub(crate) fn production_settings(
    overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
    auto_approve: Arc<dyn ironclaw_approvals::AutoApproveSettingStorePort>,
    engine: Option<Arc<Engine>>,
) -> Arc<dyn ApprovalSettingsProvider> {
    let store = Arc::new(StoreApprovalSettingsProvider::new(
        overrides,
        auto_approve,
        Arc::new(in_memory_backed_persistent_approval_policy_store()),
    ));
    let slot: ProfileApprovalEngineSlot = Arc::new(OnceLock::new());
    if let Some(engine) = engine {
        assert!(slot.set(engine).is_ok());
    }
    Arc::new(ProfileApprovalSettingsProvider::new(store, slot))
}

pub(crate) async fn engine() -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    (dir, Engine::open_libsql(runtime, "ironclaw").await.unwrap())
}

/// A thread registered with the engine, optionally on its own profile.
async fn thread_on(engine: &Engine, id: &str, mode: Option<ApprovalMode>) -> ThreadId {
    let thread = ThreadId::new(id).unwrap();
    engine
        .ensure_thread(
            &thread,
            &ironclaw_orchestrator::model::ThreadScope {
                tenant_id: tenant(),
                user_id: user(),
                agent_id: None,
                project_id: None,
            },
            None,
        )
        .await
        .unwrap();
    if let Some(mode) = mode {
        let profile = parse_profile_slug(id).unwrap();
        engine
            .store()
            .create_profile(
                &ProfileOwner {
                    tenant_id: tenant(),
                    user_id: user(),
                },
                &profile,
                &ProfileSettings {
                    display_name: id.to_string(),
                    approval_mode: Some(mode),
                    ..ProfileSettings::default()
                },
            )
            .await
            .unwrap();
        assert!(
            engine
                .store()
                .set_thread_profile(&thread, &profile)
                .await
                .unwrap()
        );
    }
    thread
}

/// Finding 1: a thread whose profile follows the global switch is not "Full
/// access" for a capability that ignores global auto-approve (computer use);
/// an explicit Full-access profile is. Builtin-shaped approvals keep running
/// on the follows-global thread.
#[tokio::test]
async fn only_an_explicit_full_access_profile_bypasses_an_approval_that_ignores_global_auto_approve()
 {
    let (_dir, engine) = engine().await;
    let follows_global = thread_on(&engine, "follows-global", None).await;
    let full_access = thread_on(&engine, "full-access", Some(ApprovalMode::FullAccess)).await;
    // Global auto-approve ON is the never-configured default.
    let settings = || {
        production_settings(
            Arc::new(in_memory_backed_capability_permission_override_store()),
            Arc::new(in_memory_backed_auto_approve_setting_store()),
            Some(Arc::clone(&engine)),
        )
    };

    let inherited = gate(settings(), desktop(), run_on(&follows_global).await).await;
    assert!(
        matches!(inherited.call().await.0, Resolution::Blocked(_)),
        "global auto-approve inherited by the thread must not hand over the desktop"
    );
    assert_eq!(inherited.runs(), 0);

    let explicit = gate(settings(), desktop(), run_on(&full_access).await).await;
    assert_eq!(
        explicit.call().await.0.kind(),
        "done",
        "an explicit Full-access profile runs computer use without asking"
    );

    let builtin_shaped = gate(
        settings(),
        SyntheticApproval::new("this action requires approval", "Do it"),
        run_on(&follows_global).await,
    )
    .await;
    assert_eq!(
        builtin_shaped.call().await.0.kind(),
        "done",
        "an approval that honors global auto-approve still runs on the follows-global thread"
    );
}

/// Finding 2: an unreadable override store refuses — a fresh call raises no
/// approval and runs nothing, and an already-approved resume runs nothing and
/// leaves its lease unspent.
#[tokio::test]
async fn an_unreadable_override_store_refuses_fresh_and_approved_calls() {
    let overrides = FlakyOverrides::new();
    let auto_approve = Arc::new(in_memory_backed_auto_approve_setting_store());
    auto_approve
        .set(AutoApproveSettingInput {
            updated_by: Principal::User(user()),
            scope: ResourceScope {
                tenant_id: tenant(),
                user_id: user(),
                agent_id: None,
                project_id: None,
                mission_id: None,
                thread_id: None,
                invocation_id: ironclaw_host_api::ids::InvocationId::new(),
            },
            enabled: false,
        })
        .await
        .unwrap();
    let settings = production_settings(overrides.clone(), auto_approve, None);
    let thread = ThreadId::new("thread-unreadable-overrides").unwrap();
    let gate = gate(
        settings,
        SyntheticApproval::new("this action requires approval", "Do it"),
        run_on(&thread).await,
    )
    .await;

    // Approved while the store reads fine…
    let (first, parked) = gate.call().await;
    assert!(matches!(first, Resolution::Blocked(_)));
    let (lease_scope, resume) = gate.approve_pending(&parked).await;

    // …then the store fails.
    overrides.failing.store(true, Ordering::SeqCst);
    assert_eq!(
        gate.resume(&parked, resume).await.kind(),
        "denied",
        "an approved resume must not run on unreadable tool settings"
    );
    assert_eq!(
        gate.lease_status(&lease_scope).await,
        CapabilityLeaseStatus::Active,
        "a refused resume leaves the approval lease unspent"
    );
    let (fresh, _) = gate.call().await;
    match &fresh {
        Resolution::Denied(denial) => assert_eq!(
            denial.reason_kind,
            Some(ironclaw_host_api::decision::DenyReason::SettingsUnavailable),
            "the model must see a temporary settings outage, not a policy refusal"
        ),
        other => panic!("a fresh call refuses too, got {other:?}"),
    }
    assert_eq!(gate.runs(), 0, "nothing runs on unreadable tool settings");

    // Once the store reads again, the same call goes through normal approval.
    overrides.failing.store(false, Ordering::SeqCst);
    assert!(
        matches!(gate.call().await.0, Resolution::Blocked(_)),
        "with settings readable again the call parks for approval as usual"
    );
    assert_eq!(gate.runs(), 0);
}
