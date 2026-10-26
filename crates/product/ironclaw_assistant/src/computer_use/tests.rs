use std::sync::Mutex;

use std::collections::HashMap;

use ironclaw_approvals::{
    in_memory_backed_approval_request_store, in_memory_backed_gate_record_store,
};
use ironclaw_authorization::in_memory_backed_capability_lease_store;
use ironclaw_host_api::ids::{ProcessId, TenantId};
use ironclaw_host_api::turn::CapabilityActivityId;
use ironclaw_loop_contracts::{
    CapabilityInputRef, CapabilitySurfaceVersion, InMemoryRunProfileResolver, LoopRequest,
    RunProfileResolutionRequest, RunProfileResolver,
};
use ironclaw_loop_host::{CapabilityWriteResult, LoopCapabilityResultWriter};
use ironclaw_processes::{JournaledProcessSnapshot, ProcessJournalCursor, ProcessJournalKind};
use ironclaw_product_contracts::computer_use::ComputerUseControlAction;
use ironclaw_turns::{TurnActor, TurnId, TurnRunId, TurnScope};

use super::*;

/// Records what reached the session layer; nothing touches a screen.
#[derive(Default)]
struct FakeSessions {
    tools: Vec<ComputerUseToolSpec>,
    calls: Mutex<Vec<String>>,
}

impl FakeSessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tools: vec![ComputerUseToolSpec {
                name: "click".to_string(),
                description: "Click".to_string(),
                input_schema: json!({"type": "object", "required": ["app"]}),
            }],
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ComputerUseSessions for FakeSessions {
    fn tools(&self) -> &[ComputerUseToolSpec] {
        &self.tools
    }

    async fn call_tool(&self, _scope: ResourceScope, tool: &str, _input: Value) -> Value {
        self.calls.lock().unwrap().push(format!("call:{tool}"));
        json!({"ok": true})
    }

    fn session_view(&self, session: &ComputerUseSession, _after: Option<u64>) -> Value {
        json!({"enabled": true, "user": session.user_id.as_str()})
    }

    fn viewer_state(&self, _tenant_id: &TenantId, user_id: &UserId, viewer: Option<&str>) -> Value {
        json!({"user": user_id.as_str(), "viewer": viewer})
    }

    async fn window_frame(
        &self,
        session: &ComputerUseSession,
        request: &ComputerUseFrameRequest,
    ) -> Value {
        json!({"thread": session.thread_id.as_str(), "window": request.window_id})
    }

    async fn control(
        &self,
        session: &ComputerUseSession,
        action: ComputerUseControlAction,
        viewer: Option<&str>,
    ) -> Value {
        self.calls.lock().unwrap().push(format!(
            "control:{action:?}:{}:{}",
            session.thread_id.as_str(),
            viewer.unwrap_or("-")
        ));
        json!({"ok": true})
    }

    async fn thread_stopped(&self, _tenant_id: &TenantId, thread_id: &ThreadId) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("stopped:{}", thread_id.as_str()));
    }

    async fn turn_ended(&self, _tenant_id: &TenantId, thread_id: &ThreadId) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ended:{}", thread_id.as_str()));
    }
}

/// Global auto-approve is always on (computer use must ignore it);
/// per-tool always-allow is set by the test.
struct PermissiveSettings {
    tool_override: Option<ToolPermissionOverride>,
    always_allow: bool,
}

#[async_trait]
impl ApprovalSettingsProvider for PermissiveSettings {
    async fn tool_override(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
    ) -> Option<ToolPermissionOverride> {
        self.tool_override
    }

    async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
        true
    }

    async fn tool_always_allow(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
        _grantee: &Principal,
    ) -> bool {
        self.always_allow
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
        Err(host_error(
            AgentLoopHostErrorKind::Internal,
            "no result may be written before approval",
        ))
    }
}

fn handler(
    sessions: &Arc<FakeSessions>,
    tool_override: Option<ToolPermissionOverride>,
    always_allow: bool,
) -> (ToolHandler, Arc<ComputerUseApprovalStores>) {
    let stores = Arc::new(ComputerUseApprovalStores {
        approval_requests: Arc::new(in_memory_backed_approval_request_store()),
        capability_leases: Arc::new(in_memory_backed_capability_lease_store()),
        approval_settings: Arc::new(PermissiveSettings {
            tool_override,
            always_allow,
        }),
        replay_payload_store: Arc::new(MemoryReplay::default()),
        gate_record_store: Arc::new(in_memory_backed_gate_record_store()),
    });
    let handler = ToolHandler {
        tool: sessions.tools[0].clone(),
        sessions: Arc::clone(sessions) as Arc<dyn ComputerUseSessions>,
        stores: Arc::clone(&stores),
        approved_turns: Arc::new(ApprovedTurns::default()),
        fallback_user_id: UserId::new("user-cu").unwrap(),
    };
    (handler, stores)
}

async fn run() -> LoopRunContext {
    let profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .unwrap();
    LoopRunContext::new(
        TurnScope::new(
            TenantId::new("tenant-cu").unwrap(),
            None,
            None,
            ThreadId::new("thread-cu").unwrap(),
        ),
        TurnId::new(),
        TurnRunId::new(),
        profile,
    )
    .with_actor(TurnActor::new(UserId::new("user-cu").unwrap()))
}

async fn invocation(
    approval_resume: Option<CapabilityApprovalResume>,
) -> SyntheticCapabilityInvocation {
    SyntheticCapabilityInvocation {
        run_context: run().await,
        request: LoopRequest {
            activity_id: CapabilityActivityId::new(),
            surface_version: CapabilitySurfaceVersion::new("v1").unwrap(),
            capability_id: CapabilityId::new("computer_use.click").unwrap(),
            input_ref: CapabilityInputRef::new("input:cu").unwrap(),
            approval_resume,
            auth_resume: None,
        },
        input: json!({"app": "Calculator", "element_index": "3"}),
        result_writer: Arc::new(NoWrites),
    }
}

#[tokio::test]
async fn every_call_raises_an_approval_when_only_global_auto_approve_is_on() {
    let sessions = FakeSessions::new();
    let (handler, stores) = handler(&sessions, None, false);

    let first = handler.invoke(invocation(None).await).await.unwrap();
    let Resolution::Blocked(_) = &first else {
        panic!("expected an approval gate, got {}", first.kind());
    };
    assert!(sessions.calls().is_empty(), "nothing ran before approval");
    let scope = run().await.acting_resource_scope(&handler.fallback_user_id);
    let pending = stores
        .approval_requests
        .records_for_scope(&scope)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "one durable approval request");
    assert_eq!(pending[0].status, ApprovalStatus::Pending);

    // Resuming while the request is still pending is a denial, not an action.
    let record = &pending[0];
    let resume = CapabilityApprovalResume {
        approval_request_id: record.request.id,
        resume_token: CapabilityResumeToken::new(record.scope.invocation_id.to_string()).unwrap(),
        correlation_id: record.request.correlation_id,
        input_ref: CapabilityInputRef::new("input:cu").unwrap(),
    };
    let resumed = handler
        .invoke(invocation(Some(resume)).await)
        .await
        .unwrap();
    assert_eq!(resumed.kind(), "denied");
    assert!(
        sessions.calls().is_empty(),
        "an ungranted approval never acts"
    );
}

#[tokio::test]
async fn a_disabled_tool_is_denied_without_an_approval_request() {
    let sessions = FakeSessions::new();
    let (handler, stores) = handler(&sessions, Some(ToolPermissionOverride::Disabled), true);
    let outcome = handler.invoke(invocation(None).await).await.unwrap();
    assert_eq!(outcome.kind(), "denied");
    let scope = run().await.acting_resource_scope(&handler.fallback_user_id);
    assert!(
        stores
            .approval_requests
            .records_for_scope(&scope)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(sessions.calls().is_empty());
    assert!(
        handler
            .validate_provider_arguments(&json!({"element_index": "1"}))
            .is_err(),
        "the schema's required app is enforced before dispatch"
    );
}

#[tokio::test]
async fn always_allow_runs_the_tool_without_an_approval_unless_ask_each_time() {
    let sessions = FakeSessions::new();
    let (allowed, stores) = handler(&sessions, None, true);
    // The test result writer refuses writes; what matters is that the call ran.
    let _ = allowed.invoke(invocation(None).await).await;
    assert_eq!(
        sessions.calls().len(),
        1,
        "always-allow reaches the session"
    );
    let scope = run().await.acting_resource_scope(&allowed.fallback_user_id);
    assert!(
        stores
            .approval_requests
            .records_for_scope(&scope)
            .await
            .unwrap()
            .is_empty(),
        "no approval request under always-allow"
    );

    let sessions = FakeSessions::new();
    let (asking, _) = handler(&sessions, Some(ToolPermissionOverride::AskEachTime), true);
    let outcome = asking.invoke(invocation(None).await).await.unwrap();
    assert!(
        matches!(outcome, Resolution::Blocked(_)),
        "ask-each-time wins"
    );
    assert!(sessions.calls().is_empty());
}

#[tokio::test]
async fn one_approval_covers_the_rest_of_its_task_only() {
    let sessions = FakeSessions::new();
    let (handler, _) = handler(&sessions, None, false);
    let approved = invocation(None).await;
    handler
        .approved_turns
        .approve(&approved.run_context.turn_id);
    let mut same_task = invocation(None).await;
    same_task.run_context.turn_id = approved.run_context.turn_id;
    // The test result writer refuses writes; what matters is that the call ran.
    let _ = handler.invoke(same_task).await;
    assert_eq!(
        sessions.calls().len(),
        1,
        "the approved task acts without asking"
    );

    let next_task = handler.invoke(invocation(None).await).await.unwrap();
    assert!(
        matches!(next_task, Resolution::Blocked(_)),
        "a new task asks again"
    );
    assert_eq!(sessions.calls().len(), 1);

    let sessions = FakeSessions::new();
    let (asking, _) = handler_with(&sessions, Some(ToolPermissionOverride::AskEachTime));
    let task = invocation(None).await;
    asking.approved_turns.approve(&task.run_context.turn_id);
    let outcome = asking.invoke(task).await.unwrap();
    assert!(
        matches!(outcome, Resolution::Blocked(_)),
        "a tool set to ask each time still asks"
    );
}

fn handler_with(
    sessions: &Arc<FakeSessions>,
    tool_override: Option<ToolPermissionOverride>,
) -> (ToolHandler, Arc<ComputerUseApprovalStores>) {
    handler(sessions, tool_override, false)
}

fn commit(status: ProcessLifecycleStatus) -> ProcessJournalCommit {
    let scope = ResourceScope {
        tenant_id: TenantId::new("tenant-cu").unwrap(),
        user_id: UserId::new("user-cu").unwrap(),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: Some(ThreadId::new("thread-cu").unwrap()),
        invocation_id: InvocationId::new(),
    };
    ProcessJournalCommit {
        state: JournaledProcessSnapshot {
            process_id: ProcessId::new(),
            process_kind: ProcessKind::AgentTurn,
            scope,
            status,
            suspension: None,
            checkpoint_ref: None,
            checkpoint_kind: None,
            input_ref: None,
            failure: None,
            journal_cursor: ProcessJournalCursor(1),
            lease: None,
            crash_reclaim_count: 0,
            created_at: chrono::Utc::now(),
            owner_user_id: None,
            concurrency_class: None,
            parent_process_id: None,
            root_process_id: None,
            metadata: Value::Null,
        },
        kind: ProcessJournalKind::Completed,
        occurred_at: None,
        sanitized_reason: None,
    }
}

#[tokio::test]
async fn a_stopped_turn_ends_the_session_and_a_finished_one_only_notifies() {
    let sessions = FakeSessions::new();
    let observer = stop_observer(&sessions);
    observer
        .observe_process_commit(commit(ProcessLifecycleStatus::Running))
        .await
        .unwrap();
    observer
        .observe_process_commit(commit(ProcessLifecycleStatus::CancelRequested))
        .await
        .unwrap();
    observer
        .observe_process_commit(commit(ProcessLifecycleStatus::Completed))
        .await
        .unwrap();
    assert_eq!(sessions.calls(), ["stopped:thread-cu", "ended:thread-cu"]);
}

/// Everything not computer use falls through to here.
struct NotFoundSurface;

#[async_trait]
impl ProductSurface for NotFoundSurface {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }

    async fn query(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }

    async fn stream_events(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

#[tokio::test]
async fn the_surface_serves_the_live_view_frames_and_controls_for_the_caller() {
    let sessions = FakeSessions::new();
    let surface = wrap_surface(Arc::new(NotFoundSurface), Some(Arc::clone(&sessions)));
    let caller = ProductSurfaceCaller::new(
        TenantId::new("t1").unwrap(),
        UserId::new("alice").unwrap(),
        None,
        None,
    );
    let query = |view: &str, input: Value| ProductSurfaceQueryRequest {
        view_id: view.to_string(),
        input,
        cursor: None,
        limit: None,
    };

    let state = surface
        .query(
            caller.clone(),
            query("computer_use.state", json!({"viewer": "v1abc"})),
        )
        .await
        .unwrap();
    assert_eq!(state.items[0], json!({"user": "alice", "viewer": "v1abc"}));
    let bad_viewer = surface
        .query(
            caller.clone(),
            query("computer_use.state", json!({"viewer": "../x"})),
        )
        .await;
    assert!(bad_viewer.is_err(), "viewer ids are bounded labels");

    let frame = surface
        .query(
            caller.clone(),
            query(
                "computer_use.frame",
                json!({"thread_id": "thread-1", "window_id": 77, "width": 480}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(frame.items[0], json!({"thread": "thread-1", "window": 77}));

    surface
        .invoke(
            caller.clone(),
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(COMPUTER_USE_CONTROL_COMMAND_ID).unwrap(),
                input: json!({"thread_id": "thread-1", "action": "take_over", "viewer": "v1abc"}),
                activity_id: ironclaw_host_api::ids::ActivityId::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(sessions.calls(), vec!["control:TakeOver:thread-1:v1abc"]);

    let passthrough = surface.query(caller, query("threads", json!({}))).await;
    assert!(passthrough.is_err(), "other views reach the inner surface");

    let off = wrap_surface::<FakeSessions>(Arc::new(NotFoundSurface), None);
    let state = off
        .query(
            ProductSurfaceCaller::new(
                TenantId::new("t1").unwrap(),
                UserId::new("alice").unwrap(),
                None,
                None,
            ),
            query("computer_use.state", json!({})),
        )
        .await
        .unwrap();
    assert_eq!(state.items[0]["enabled"], false);
}

#[test]
fn a_window_tree_reaches_the_model_in_full_not_behind_result_read() {
    use ironclaw_loop_contracts::{ObservationTrust, ToolObservationStatus};

    // A real Outlook state read is 4-20 KB: past the generic 3 KB first look.
    let tree = "\t12 row Inbox message from a sender\n".repeat(150);
    let output = serde_json::json!({"ok": true, "result": tree});
    let serialized = serde_json::to_vec(&output).expect("serialize");
    let result_ref = ironclaw_turns::LoopResultRef::new("result:cu_run.1").expect("result ref");
    let first_look = ModelVisibleToolObservation {
        schema_version: 1,
        status: ToolObservationStatus::Success,
        summary: "Tool completed; preview is a bounded JSON view.".to_string(),
        detail: ToolObservationDetail::ResultReference {
            result_ref: result_ref.as_str().to_string(),
            byte_len: serialized.len() as u64,
            preview: Some("{\"content\":{\"ok\":true}}".to_string()),
            structured_json_view: true,
            total_bytes: Some(serialized.len() as u64),
            next_offset: None,
            item_count: None,
        },
        artifacts: Vec::new(),
        recovery: None,
        trust: ObservationTrust::UntrustedToolOutput,
    };

    let observation = inline_observation(first_look, &result_ref, &serialized);

    assert_eq!(
        observation.summary,
        "Tool completed; preview contains the full result."
    );
    let ToolObservationDetail::ResultReference { preview, .. } = observation.detail else {
        panic!("expected a result reference");
    };
    let preview = preview.expect("preview");
    assert!(preview.contains("Inbox message from a sender"));
    assert!(preview.len() > 4 * 1024);
}
