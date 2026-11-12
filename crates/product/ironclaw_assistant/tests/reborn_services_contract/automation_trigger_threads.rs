//! Access to automation-trigger threads under the trigger creator scope.

use super::*;

/// An automation service whose `resolve_run_thread_scope` always returns a
/// backend error (503 Unavailable, retryable). Used to verify that the timeline
/// call surfaces the backend error rather than masking it as a 404.
struct ErroringAutomationService {
    error: ProductSurfaceError,
}

impl ErroringAutomationService {
    fn unavailable() -> Self {
        Self {
            error: ProductSurfaceError {
                code: ProductSurfaceErrorCode::Unavailable,
                kind: ProductSurfaceErrorKind::ServiceUnavailable,
                status_code: 503,
                retryable: true,
                field: None,
                validation_code: None,
            },
        }
    }
}

#[async_trait]
impl AutomationProductService for ErroringAutomationService {
    async fn list_automations(
        &self,
        _caller: ProductAgentBoundCaller,
        _request: AutomationListRequest,
    ) -> Result<Vec<RebornAutomationInfo>, ProductSurfaceError> {
        Ok(Vec::new())
    }

    async fn resolve_run_thread_scope(
        &self,
        _caller: ProductAgentBoundCaller,
        _thread_id: &ThreadId,
    ) -> Result<Option<TriggerRunThreadScope>, ProductSurfaceError> {
        Err(self.error.clone())
    }
}

// Regression tests for the automation-trigger timeline fallback.
// Bug: `get_timeline` scoped the thread lookup to the WebUI user's
// `owner_user_id`, but trigger-fired threads are stored with the external
// creator's `owner_user_id`.  The user-scoped probe returned `UnknownThread`,
// and the handler propagated `404` without checking whether the thread
// belongs to one of the caller's automations.

#[tokio::test]
async fn get_timeline_succeeds_for_own_automation_trigger_thread() {
    // Trigger thread stored with the EXTERNAL creator's owner_user_id — not the
    // WebUI caller's.  The old guessing code would produce a caller-scoped
    // ThreadScope and miss this thread; the new `resolve_run_thread_scope` path
    // must reconstruct the true scope and return the history.
    let trigger_thread_id = ThreadId::new("thread-trigger-alpha").expect("valid trigger thread id");
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());

    // Store the trigger thread under the external creator's scope (not caller).
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: trigger_thread_scope_for(&caller),
            thread_id: Some(trigger_thread_id.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Scheduled run".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-scheduled-alpha",
            )),
        })
        .await
        .expect("trigger thread stored");

    // The automation service recognises the thread and returns the trigger scope.
    let automation_service = Arc::new(
        StaticAutomationService::new(vec![RebornAutomationInfo {
            automation_id: "trigger-scheduled-alpha".to_string(),
            name: "Morning briefing".to_string(),
            source: RebornAutomationSource::Schedule {
                cron: "0 9 * * *".to_string(),
                timezone: "UTC".to_string(),
            },
            state: RebornAutomationState::Active,
            next_run_at: None,
            last_run_at: None,
            last_status: Some(RebornAutomationRunStatus::Ok),
            recent_runs: vec![RebornAutomationRecentRunInfo {
                run_id: Some(automation_run_id()),
                thread_id: Some(trigger_thread_id.clone()),
                fire_slot: None,
                status: RebornAutomationRecentRunStatus::Ok,
                submitted_at: "2026-06-09T09:00:01Z".parse().expect("submitted_at"),
                completed_at: Some("2026-06-09T09:00:42Z".parse().expect("completed_at")),
            }],
            is_active: true,
            created_at: None,
            active_hold: None,
        }])
        .with_resolve_scope_for_thread(
            trigger_thread_id.clone(),
            trigger_run_thread_scope_for(&caller),
        ),
    );

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service);

    let response = services
        .get_timeline(
            caller,
            RebornTimelineRequest::new(trigger_thread_id.as_str().to_string()),
        )
        .await
        .expect("owner should be able to read their automation trigger thread timeline");

    assert_eq!(response.thread.thread_id, trigger_thread_id);
}

/// Records the scope and storage key each byte read is issued under so a test
/// can assert the reader addressed the right project mount AND resolved the
/// right attachment key.
struct RecordingAttachmentReader {
    bytes: Vec<u8>,
    reads: Mutex<Vec<(ThreadScope, String)>>,
}

#[async_trait]
impl InboundAttachmentReader for RecordingAttachmentReader {
    async fn read(
        &self,
        thread_scope: &ThreadScope,
        storage_key: &str,
    ) -> Result<Vec<u8>, ProductSurfaceError> {
        self.reads
            .lock()
            .expect("lock")
            .push((thread_scope.clone(), storage_key.to_string()));
        Ok(self.bytes.clone())
    }
}

// Regression for the trigger-thread byte-read scope. `read_attachment` shares
// the timeline's automation-trigger fallback, which resolves the thread under
// the trigger creator's scope (not the WebUI caller's session scope). The bytes
// must be read back under that same resolved scope — reading under the caller's
// session scope would address the wrong project mount and 404.
#[tokio::test]
async fn read_attachment_reads_trigger_thread_bytes_under_creator_scope() {
    let trigger_thread_id = ThreadId::new("thread-trigger-bytes").expect("valid trigger thread id");
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());

    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: trigger_thread_scope_for(&caller),
            thread_id: Some(trigger_thread_id.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Scheduled run".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json("trigger-bytes")),
        })
        .await
        .expect("trigger thread stored");

    // A landed image attachment on the trigger thread, stored under the
    // creator's scope.
    let accepted = thread_service
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: trigger_thread_scope_for(&caller),
            thread_id: trigger_thread_id.clone(),
            actor_id: "system".to_string(),
            source_binding_id: None,
            reply_target_binding_id: None,
            external_event_id: Some("trigger-image".to_string()),
            content: MessageContent::with_attachments(
                "see image",
                vec![AttachmentRef {
                    id: "att-0".to_string(),
                    kind: AttachmentKind::Image,
                    mime_type: "image/png".to_string(),
                    filename: Some("p.png".to_string()),
                    size_bytes: Some(4),
                    storage_key: Some("/workspace/attachments/2026-06-14/m-0-p.png".to_string()),
                    extracted_text: None,
                }],
            ),
        })
        .await
        .expect("message with attachment accepted");

    let automation_service = Arc::new(
        StaticAutomationService::new(vec![RebornAutomationInfo {
            automation_id: "trigger-bytes".to_string(),
            name: "Morning briefing".to_string(),
            source: RebornAutomationSource::Schedule {
                cron: "0 9 * * *".to_string(),
                timezone: "UTC".to_string(),
            },
            state: RebornAutomationState::Active,
            next_run_at: None,
            last_run_at: None,
            last_status: Some(RebornAutomationRunStatus::Ok),
            recent_runs: vec![RebornAutomationRecentRunInfo {
                run_id: Some(automation_run_id()),
                thread_id: Some(trigger_thread_id.clone()),
                fire_slot: None,
                status: RebornAutomationRecentRunStatus::Ok,
                submitted_at: "2026-06-09T09:00:01Z".parse().expect("submitted_at"),
                completed_at: Some("2026-06-09T09:00:42Z".parse().expect("completed_at")),
            }],
            is_active: true,
            created_at: None,
            active_hold: None,
        }])
        .with_resolve_scope_for_thread(
            trigger_thread_id.clone(),
            trigger_run_thread_scope_for(&caller),
        ),
    );

    let reader = Arc::new(RecordingAttachmentReader {
        bytes: vec![1, 2, 3, 4],
        reads: Mutex::new(Vec::new()),
    });
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service)
        .with_inbound_attachment_reader(reader.clone());

    let result = services
        .read_attachment(
            caller,
            RebornAttachmentRequest {
                thread_id: trigger_thread_id.as_str().to_string(),
                message_id: accepted.message_id.to_string(),
                attachment_id: "att-0".to_string(),
            },
        )
        .await
        .expect("owner should be able to read their trigger thread's attachment");

    assert_eq!(result.bytes, vec![1, 2, 3, 4]);
    assert_eq!(result.mime_type, "image/png");

    // The fix: the read was issued under the trigger creator's scope (not the
    // caller's session scope) and for the landed attachment's own storage key.
    let reads = reader.reads.lock().expect("lock");
    assert_eq!(reads.len(), 1);
    let (scope, storage_key) = &reads[0];
    assert_eq!(
        scope.owner_user_id,
        Some(UserId::new(TRIGGER_CREATOR_USER_ID).expect("trigger creator user id")),
    );
    assert_eq!(storage_key, "/workspace/attachments/2026-06-14/m-0-p.png");
}

#[tokio::test]
async fn get_timeline_rejects_other_users_automation_trigger_thread() {
    // A trigger thread owned by alice's automation. Bob tries to read it.
    let alice = caller_for_user("user-alice");
    let bob = caller_for_user("user-bob");
    let trigger_thread_id = ThreadId::new("thread-trigger-beta").expect("valid trigger thread id");

    let thread_service = Arc::new(InMemorySessionThreadService::default());
    // Store the thread in alice's trigger scope.
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: trigger_thread_scope_for(&alice),
            thread_id: Some(trigger_thread_id.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Alice's scheduled run".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-alices-job",
            )),
        })
        .await
        .expect("alice trigger thread stored");

    // Bob's service returns no automations and no resolve_scope — the fallback
    // must deny him because resolve_run_thread_scope returns None.
    let automation_service = Arc::new(StaticAutomationService::new(Vec::new()));

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service);

    let err = services
        .get_timeline(
            bob,
            RebornTimelineRequest::new(trigger_thread_id.as_str().to_string()),
        )
        .await
        .expect_err("non-owner must not read another user's trigger thread");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
}

// Contract: backend errors from `resolve_run_thread_scope` must surface as 503
// Unavailable, not be masked as 404 NotFound.  A backend outage should never
// look like an authorization miss to the caller.
#[tokio::test]
async fn get_timeline_surfaces_trigger_scope_lookup_backend_error() {
    // The primary user-scoped lookup will miss (thread stored under trigger
    // creator scope), then the automation fallback fires.  The service returns
    // a 503 Unavailable error — the service must propagate that error rather
    // than converting it to 404.
    let caller = caller();
    let trigger_thread_id =
        ThreadId::new("thread-trigger-backend-err").expect("valid trigger thread id");

    let thread_service = Arc::new(InMemorySessionThreadService::default());
    // Store the thread under the external creator's scope so the user-scoped
    // lookup misses and the automation fallback is invoked.
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: trigger_thread_scope_for(&caller),
            thread_id: Some(trigger_thread_id.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Trigger backend error test thread".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-backend-err-automation",
            )),
        })
        .await
        .expect("trigger thread stored");

    // The automation service returns a 503 backend error from resolve_run_thread_scope.
    let automation_service = Arc::new(ErroringAutomationService::unavailable());

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service);

    let err = services
        .get_timeline(
            caller,
            RebornTimelineRequest::new(trigger_thread_id.as_str().to_string()),
        )
        .await
        .expect_err("backend error from service must propagate, not become 404");

    assert_eq!(
        err.code,
        ProductSurfaceErrorCode::Unavailable,
        "backend lookup error must surface as Unavailable, not NotFound"
    );
    assert_eq!(err.status_code, 503);
    assert!(err.retryable, "backend outage error must be retryable");
}

/// A `SessionThreadService` that returns `UnknownThread` on its first
/// `list_thread_history` call and `Backend(...)` on every subsequent call.
/// Used to test the error-taxonomy contract when the caller-scoped probe misses
/// (→ automation fallback fires) but the trigger-owned scope reload then errors.
struct FirstMissBackendErrorThreadService {
    call_count: Mutex<usize>,
}

impl FirstMissBackendErrorThreadService {
    fn new() -> Self {
        Self {
            call_count: Mutex::new(0),
        }
    }
}

#[async_trait]
impl SessionThreadService for FirstMissBackendErrorThreadService {
    async fn list_thread_history(
        &self,
        request: ThreadHistoryRequest,
    ) -> Result<ThreadHistory, SessionThreadError> {
        let mut count = self.call_count.lock().expect("lock");
        *count += 1;
        if *count == 1 {
            Err(SessionThreadError::UnknownThread {
                thread_id: request.thread_id,
            })
        } else {
            Err(SessionThreadError::Backend(
                "backend error on trigger-owned reload".to_string(),
            ))
        }
    }

    async fn ensure_thread(
        &self,
        _request: EnsureThreadRequest,
    ) -> Result<SessionThreadRecord, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::ensure_thread should not be reached")
    }

    async fn accept_inbound_message(
        &self,
        _request: AcceptInboundMessageRequest,
    ) -> Result<AcceptedInboundMessage, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::accept_inbound_message should not be reached")
    }

    async fn replay_accepted_inbound_message(
        &self,
        _request: ReplayAcceptedInboundMessageRequest,
    ) -> Result<Option<AcceptedInboundMessageReplay>, SessionThreadError> {
        panic!(
            "FirstMissBackendErrorThreadService::replay_accepted_inbound_message should not be reached"
        )
    }

    async fn mark_message_submitted(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
        _turn_id: String,
        _turn_run_id: String,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::mark_message_submitted should not be reached")
    }

    async fn mark_message_rejected_busy(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!(
            "FirstMissBackendErrorThreadService::mark_message_rejected_busy should not be reached"
        )
    }

    async fn append_assistant_draft(
        &self,
        _request: AppendAssistantDraftRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::append_assistant_draft should not be reached")
    }

    async fn append_tool_result_reference(
        &self,
        _request: AppendToolResultReferenceRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!(
            "FirstMissBackendErrorThreadService::append_tool_result_reference should not be reached"
        )
    }

    async fn append_capability_display_preview(
        &self,
        _request: AppendCapabilityDisplayPreviewRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!(
            "FirstMissBackendErrorThreadService::append_capability_display_preview should not be reached"
        )
    }

    async fn update_tool_result_reference(
        &self,
        _request: UpdateToolResultReferenceRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!(
            "FirstMissBackendErrorThreadService::update_tool_result_reference should not be reached"
        )
    }

    async fn update_assistant_draft(
        &self,
        _request: UpdateAssistantDraftRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::update_assistant_draft should not be reached")
    }

    async fn finalize_assistant_message(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
        _content: MessageContent,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!(
            "FirstMissBackendErrorThreadService::finalize_assistant_message should not be reached"
        )
    }

    async fn redact_message(
        &self,
        _request: RedactMessageRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::redact_message should not be reached")
    }

    async fn load_context_window(
        &self,
        _request: LoadContextWindowRequest,
    ) -> Result<ContextWindow, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::load_context_window should not be reached")
    }

    async fn load_context_messages(
        &self,
        _request: LoadContextMessagesRequest,
    ) -> Result<ContextMessages, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::load_context_messages should not be reached")
    }

    async fn list_threads_for_scope(
        &self,
        _request: ListThreadsForScopeRequest,
    ) -> Result<ListThreadsForScopeResponse, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::list_threads_for_scope should not be reached")
    }

    async fn create_summary_artifact(
        &self,
        _request: CreateSummaryArtifactRequest,
    ) -> Result<SummaryArtifact, SessionThreadError> {
        panic!("FirstMissBackendErrorThreadService::create_summary_artifact should not be reached")
    }
}

// Contract: when the caller-scoped probe misses (UnknownThread → automation
// fallback fires) and `resolve_run_thread_scope` authorizes access, but the
// second `list_thread_history` call for the trigger-owned scope returns a
// backend error, the result must be Unavailable (503) — NOT the 404 NotFound
// that would have been returned had the automation service also denied access.
// A backend outage must never be surfaced as an authorization miss.
#[tokio::test]
async fn get_timeline_surfaces_backend_error_from_unscoped_trigger_history_reload() {
    let caller = caller();
    let trigger_thread_id =
        ThreadId::new("thread-trigger-reload-error").expect("valid trigger thread id");

    // Thread service: first call (caller-scoped probe) → UnknownThread,
    // second call (trigger-owned scope reload) → Backend error.
    let thread_service = Arc::new(FirstMissBackendErrorThreadService::new());

    // Automation service authorizes: the service resolves a scope for the
    // thread, so the service proceeds to the trigger-owned reload.
    let automation_service = Arc::new(
        StaticAutomationService::new(Vec::new()).with_resolve_scope_for_thread(
            trigger_thread_id.clone(),
            trigger_run_thread_scope_for(&caller),
        ),
    );

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service);

    let err = services
        .get_timeline(
            caller,
            RebornTimelineRequest::new(trigger_thread_id.as_str().to_string()),
        )
        .await
        .expect_err("backend error on trigger-owned reload must surface as 503, not 404");

    // Must be Unavailable, not NotFound: the backend error on the reload
    // must not be mistaken for an authorization miss.
    assert_eq!(
        err.code,
        ProductSurfaceErrorCode::Unavailable,
        "trigger-owned reload backend error must map to Unavailable, not NotFound"
    );
    assert_eq!(err.status_code, 503);
    assert!(err.retryable, "backend outage must be retryable");
}

// Contract: when `TriggerRunThreadScope.agent_id` is `None` the fallback must
// substitute the bound caller's `agent_id` so the reconstructed `TurnScope`
// can locate the thread in storage.
#[tokio::test]
async fn get_timeline_uses_caller_agent_when_trigger_scope_omits_agent_id() {
    // `TriggerRunThreadScope.agent_id` is `Option<AgentId>`.  When it is
    // `None` (e.g. the trigger record was stored without an explicit agent),
    // `check_automation_trigger_access` falls back to `bound_caller.agent_id`.
    // This test seeds the thread under the scope that fallback should produce
    // (caller's agent, trigger's project, creator's owner) and verifies that
    // the timeline resolves — proving the fallback actually runs.
    let caller = caller();
    let trigger_thread_id =
        ThreadId::new("thread-trigger-no-agent").expect("valid trigger thread id");

    // The thread is stored under the scope the fallback reconstructs:
    //   agent_id    = bound_caller.agent_id  (falls back from None)
    //   project_id  = trigger_scope.project_id
    //   owner_user_id = Some(creator_user_id)
    let fallback_scope = ThreadScope {
        tenant_id: caller.tenant_id.clone(),
        agent_id: caller.agent_id.clone().expect("test caller has agent"),
        project_id: caller.project_id.clone(),
        owner_user_id: Some(
            UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid trigger creator user id"),
        ),
        mission_id: None,
    };
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: fallback_scope,
            thread_id: Some(trigger_thread_id.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Agent-omitted trigger run".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-no-agent-automation",
            )),
        })
        .await
        .expect("trigger thread stored");

    // The trigger scope has agent_id = None, exercising the fallback branch.
    let scope_with_no_agent = TriggerRunThreadScope {
        agent_id: None,
        project_id: caller.project_id.clone(),
        creator_user_id: UserId::new(TRIGGER_CREATOR_USER_ID)
            .expect("valid trigger creator user id"),
    };
    let automation_service = Arc::new(
        StaticAutomationService::new(vec![])
            .with_resolve_scope_for_thread(trigger_thread_id.clone(), scope_with_no_agent),
    );

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service);

    let response = services
        .get_timeline(
            caller,
            RebornTimelineRequest::new(trigger_thread_id.as_str().to_string()),
        )
        .await
        .expect("timeline must resolve when agent_id is None via caller fallback");

    assert_eq!(
        response.thread.thread_id, trigger_thread_id,
        "fallback to caller agent_id must locate the trigger-owned thread"
    );
}

#[tokio::test]
async fn resolve_gate_approval_succeeds_for_own_automation_trigger_thread() {
    // The caller owns the automation that produced the trigger thread. Approval
    // of a gate on that thread must succeed via the automation fallback.
    //
    // Post-#4754 ("Part A") verification: `check_automation_trigger_access`
    // must forward the trigger-owned `TurnScope` (with
    // `owner_user_id = Some(TRIGGER_CREATOR_USER_ID)`) — not the WebUI
    // caller's user_id — to the turn coordinator's `get_run_state` call.
    // The fake coordinator is configured to return `BlockedApproval` only
    // for any scope it receives; this assertion proves the coordinator
    // actually gets the trigger-owned scope, not the caller's session scope.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-trigger-gate-alpha").await;

    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    // Program coordinator to report BlockedApproval with an approval gate.
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate ref");
    coordinator.set_parked_approval_gate(gate_ref.clone());
    coordinator.set_run_state_actor(Some(turn_actor_for_user(TRIGGER_CREATOR_USER_ID)));

    let services = session_services(thread_service, coordinator.clone())
        .with_automation_product_service(automation_service_with_trigger_thread(
            trigger_thread_id.clone(),
            &caller,
        ))
        .with_approval_interactions(approval_interactions.clone());

    let response = services
        .resolve_gate(
            caller.clone(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "approval-trigger-1",
                "thread_id": trigger_thread_id.as_str(),
                "run_id": run_id_string(),
                "gate_ref": gate_ref.as_str(),
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect("automation owner should be able to approve gate on trigger thread");

    assert!(
        matches!(response, RebornResolveGateResponse::Resumed(_)),
        "expected Resumed, got {response:?}"
    );
    assert_eq!(
        approval_interactions.resolution_count(),
        1,
        "approval interaction should have been called"
    );

    // Part A scope assertion: the coordinator must receive the trigger-owned
    // scope (owner = TRIGGER_CREATOR_USER_ID), not the WebUI caller's scope
    // (owner = "user-alpha"). This confirms `check_automation_trigger_access`
    // reconstructs the scope from `TriggerRunThreadScope.creator_user_id` and
    // that the reconstructed scope flows through to the turn coordinator.
    let expected_trigger_scope = TurnScope::new_with_owner(
        caller.tenant_id.clone(),
        caller.agent_id.clone(),
        caller.project_id.clone(),
        trigger_thread_id.clone(),
        Some(UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid creator user id")),
    );
    assert_eq!(
        coordinator.last_run_state_scope(),
        Some(expected_trigger_scope),
        "get_run_state must receive the trigger-owned scope (owner = TRIGGER_CREATOR_USER_ID), \
         not the WebUI caller's session scope (owner = user-alpha)"
    );
    assert_eq!(
        approval_interactions
            .last_resolution()
            .expect("approval resolution")
            .actor
            .user_id,
        UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid creator user id"),
        "approval resolution must resume the run as the trigger creator, not the WebUI caller"
    );
}

#[tokio::test]
async fn cancel_run_succeeds_for_own_automation_trigger_thread() {
    // The caller owns the automation, but the run itself belongs to the trigger
    // creator. cancel_run must forward both the trigger-owned scope and the run
    // actor to the turn coordinator.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-trigger-cancel-alpha").await;
    let coordinator = Arc::new(FakeTurnCoordinator::default());

    let services =
        session_services(thread_service, coordinator.clone()).with_automation_product_service(
            automation_service_with_trigger_thread(trigger_thread_id.clone(), &caller),
        );

    let response = services
        .cancel_run(
            caller.clone(),
            serde_json::from_value::<ProductCancelRunRequest>(json!({
                "client_action_id": "cancel-trigger-1",
                "thread_id": trigger_thread_id.as_str(),
                "run_id": run_id_string(),
                "reason": "user_requested"
            }))
            .expect("request"),
        )
        .await
        .expect("automation owner should be able to cancel trigger thread run");

    assert_eq!(response.status, TurnStatus::Cancelled);
    let expected_trigger_scope = TurnScope::new_with_owner(
        caller.tenant_id.clone(),
        caller.agent_id.clone(),
        caller.project_id.clone(),
        trigger_thread_id,
        Some(UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid creator user id")),
    );
    assert_eq!(
        coordinator.last_cancellation_scope(),
        Some(expected_trigger_scope),
        "cancel_run must receive the trigger-owned scope"
    );
    assert_eq!(
        coordinator
            .last_cancellation_actor()
            .expect("cancel actor")
            .user_id,
        UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid creator user id"),
        "cancel_run must use the trigger creator actor, not the WebUI caller"
    );
}

#[tokio::test]
async fn get_run_state_succeeds_for_own_automation_trigger_thread() {
    // get_run_state is read-only, but it still must resolve the browser thread
    // id to the trigger-owned scope before querying the coordinator.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-trigger-state-alpha").await;
    let coordinator = Arc::new(FakeTurnCoordinator::default());

    let services =
        session_services(thread_service, coordinator.clone()).with_automation_product_service(
            automation_service_with_trigger_thread(trigger_thread_id.clone(), &caller),
        );

    let response = services
        .get_run_state(
            caller.clone(),
            RebornGetRunStateRequest {
                thread_id: trigger_thread_id.as_str().to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect("automation owner should be able to read trigger run state");

    assert_eq!(response.status, TurnStatus::Queued);
    let expected_trigger_scope = TurnScope::new_with_owner(
        caller.tenant_id.clone(),
        caller.agent_id.clone(),
        caller.project_id.clone(),
        trigger_thread_id,
        Some(UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid creator user id")),
    );
    assert_eq!(
        coordinator.last_run_state_scope(),
        Some(expected_trigger_scope),
        "get_run_state must query the trigger-owned scope"
    );
}

#[tokio::test]
async fn resolve_gate_rejects_other_users_automation_trigger_thread() {
    // Alice owns the trigger thread. Bob should get 404, not a gate resolution.
    let alice = caller_for_user("user-alice");
    let bob = caller_for_user("user-bob");
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id =
        setup_trigger_thread(&thread_service, &alice, "thread-trigger-gate-beta").await;

    // Bob has no automations — resolve_run_thread_scope returns None, fallback denies him.
    let bob_automation_service = Arc::new(StaticAutomationService::new(Vec::new()));
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate ref");

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(bob_automation_service)
        .with_approval_interactions(approval_interactions.clone());

    let err = services
        .resolve_gate(
            bob,
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "approval-trigger-rejected",
                "thread_id": trigger_thread_id.as_str(),
                "run_id": run_id_string(),
                "gate_ref": gate_ref.as_str(),
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect_err("non-owner must not resolve gate on another user's trigger thread");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        approval_interactions.resolution_count(),
        0,
        "approval interaction must not be called for unauthorized caller"
    );
}

// Regression: stream_events used the WebUI caller's user_id as the projection
// identity even after resolve_thread_access_for_caller succeeded via the
// automation fallback. For a trigger-fired thread the run events are keyed
// under the trigger creator's user_id, not the WebUI caller's; passing the
// caller's id caused the turn-event replay filter (owner_user_id) and the
// runtime event stream key (EventStreamKey) to select the wrong bucket —
// approval-gate events were invisible to the chat page.
//
// The fix: after authorization succeeds, derive the projection identity from
// scope.explicit_owner_user_id() (the creator for trigger threads; falls back
// to caller for normal session threads where thread_owner = ActorFallback).
#[tokio::test]
async fn stream_events_uses_trigger_creator_as_projection_identity() {
    // The caller ("user-alpha") owns the automation. The trigger thread was
    // stored under the external creator's scope ("user-trigger-creator").
    // stream_events must pass the CREATOR's identity to the projection drain,
    // not the caller's, so the correct event stream bucket is selected.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-trigger-stream-alpha").await;

    let event_stream = Arc::new(RecordingProjectionStream::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service_with_trigger_thread(
            trigger_thread_id.clone(),
            &caller,
        ))
        .with_event_stream(event_stream.clone());

    services
        .stream_events(
            caller.clone(),
            RebornStreamEventsRequest {
                thread_id: trigger_thread_id.as_str().to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect("automation owner should be able to stream trigger thread events");

    // The projection drain must have been called with the trigger CREATOR's
    // user_id, not the WebUI caller's user_id. Events are owned by the
    // run's submitting identity (the creator); using the caller's id
    // filters to the wrong stream/event bucket.
    let requests = event_stream.requests();
    assert_eq!(
        requests.len(),
        1,
        "projection drain must be called exactly once"
    );
    assert_eq!(
        requests[0].actor.user_id,
        UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid creator user id"),
        "projection actor must be the trigger creator (owner of the run events), \
         not the WebUI caller (who proved visibility via automation ownership)"
    );
    // The scope must still carry the thread_id correctly.
    assert_eq!(
        requests[0].scope.thread_id, trigger_thread_id,
        "projection scope thread_id must match the trigger thread"
    );
}

#[tokio::test]
async fn stream_events_revalidates_service_on_every_poll() {
    // Every stream_events poll must call resolve_run_thread_scope — there is no
    // authorization cache. This ensures a caller that loses automation
    // visibility between polls cannot keep draining the trigger-owned stream.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id = setup_trigger_thread(
        &thread_service,
        &caller,
        "thread-trigger-stream-revalidate-alpha",
    )
    .await;

    let automation_service =
        automation_service_with_trigger_thread(trigger_thread_id.clone(), &caller);
    let event_stream = Arc::new(RecordingProjectionStream::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service.clone())
        .with_event_stream(event_stream.clone());

    for _ in 0..3 {
        services
            .stream_events(
                caller.clone(),
                RebornStreamEventsRequest {
                    thread_id: trigger_thread_id.as_str().to_string(),
                    after_cursor: None,
                },
            )
            .await
            .expect("automation owner should be able to repeatedly stream trigger events");
    }

    assert_eq!(
        automation_service.resolve_calls(),
        vec![
            trigger_thread_id.clone(),
            trigger_thread_id.clone(),
            trigger_thread_id.clone()
        ],
        "every stream_events poll must call resolve_run_thread_scope (no authz caching)"
    );
    assert_eq!(
        event_stream.requests().len(),
        3,
        "event polling must not be suppressed"
    );
}

#[tokio::test]
async fn stream_events_fails_when_visibility_revoked_between_polls() {
    // If the caller's automation visibility is revoked between polls,
    // the next poll must fail with not_found — the authz result is not cached.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id = setup_trigger_thread(
        &thread_service,
        &caller,
        "thread-trigger-stream-revoke-alpha",
    )
    .await;

    // A service that starts with the scope available but can revoke it.
    let revocable_service = Arc::new(RevocableAutomationService::new(
        trigger_thread_id.clone(),
        &caller,
    ));
    let event_stream = Arc::new(RecordingProjectionStream::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(revocable_service.clone())
        .with_event_stream(event_stream.clone());

    // First poll succeeds — caller still has automation visibility.
    services
        .stream_events(
            caller.clone(),
            RebornStreamEventsRequest {
                thread_id: trigger_thread_id.as_str().to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect("first poll must succeed while scope is visible");

    // Revoke visibility.
    revocable_service.revoke();

    // Second poll must fail — visibility was revoked and there is no cached authz.
    let err = services
        .stream_events(
            caller.clone(),
            RebornStreamEventsRequest {
                thread_id: trigger_thread_id.as_str().to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("second poll must fail after visibility is revoked");

    assert_eq!(
        err.code,
        ProductSurfaceErrorCode::NotFound,
        "revoked visibility must surface as not_found, not a stale cached grant"
    );
    assert_eq!(err.status_code, 404);
}

#[tokio::test]
async fn get_timeline_rejects_thread_id_absent_from_callers_automations() {
    // The thread_id does not appear in the caller's automation run history at
    // all — `resolve_run_thread_scope` returns `None`.  The service must return
    // 404 and must NOT fall back to guessing the thread scope.
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    // No threads stored anywhere.

    // Automation service knows about a DIFFERENT thread, not the requested one.
    let unrelated_thread_id =
        ThreadId::new("thread-unrelated-xyz").expect("valid unrelated thread id");
    let automation_service = Arc::new(
        StaticAutomationService::new(vec![RebornAutomationInfo {
            automation_id: "trigger-other".to_string(),
            name: "Other automation".to_string(),
            source: RebornAutomationSource::Schedule {
                cron: "0 12 * * *".to_string(),
                timezone: "UTC".to_string(),
            },
            state: RebornAutomationState::Active,
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            recent_runs: vec![RebornAutomationRecentRunInfo {
                run_id: Some(automation_run_id()),
                thread_id: Some(unrelated_thread_id),
                fire_slot: None,
                status: RebornAutomationRecentRunStatus::Ok,
                submitted_at: "2026-06-10T12:00:00Z".parse().expect("submitted_at"),
                completed_at: Some("2026-06-10T12:01:00Z".parse().expect("completed_at")),
            }],
            is_active: true,
            created_at: None,
            active_hold: None,
        }]), // resolve_scope is None — the service does not recognise the requested thread.
    );

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service);

    let err = services
        .get_timeline(
            caller,
            RebornTimelineRequest::new("thread-absent-from-automations".to_string()),
        )
        .await
        .expect_err("unknown thread_id must return 404");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
}
