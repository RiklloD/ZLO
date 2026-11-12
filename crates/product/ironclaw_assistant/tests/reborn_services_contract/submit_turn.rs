//! Turn submission, duplicate-submit replay, and session-channel admission.

use super::*;

fn legacy_webui_source_binding_id_for(caller: &ProductSurfaceCaller, thread_id: &str) -> String {
    format!(
        "{}{}{}{}{}",
        segment("surface", "webui"),
        segment("tenant", caller.tenant_id.as_str()),
        segment(
            "agent",
            caller.agent_id.as_ref().map(AgentId::as_str).unwrap_or("")
        ),
        segment("thread", thread_id),
        segment("actor", caller.user_id.as_str())
    )
}

#[tokio::test]
async fn submit_turn_uses_service_and_thread_history_without_route_store_access() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-1",
                "thread_id": "thread-alpha",
                "content": "hello from webui"
            }))
            .expect("request"),
        )
        .await
        .expect("submit succeeds");

    let RebornSubmitTurnResponse::Submitted {
        thread_id,
        status,
        event_cursor,
        ..
    } = response
    else {
        panic!("expected submitted response");
    };
    assert_eq!(thread_id.as_str(), "thread-alpha");
    assert_eq!(status, TurnStatus::Queued);
    assert_eq!(event_cursor, EventCursor(7));
    assert_eq!(coordinator.submission_count(), 1);

    let timeline = services
        .get_timeline(caller(), RebornTimelineRequest::new("thread-alpha"))
        .await
        .expect("timeline");
    assert_eq!(timeline.messages.len(), 1);
    assert_eq!(timeline.messages[0].status, MessageStatus::Submitted);
    assert_eq!(
        timeline.messages[0].content.as_deref(),
        Some("hello from webui")
    );
    let submission_scope = coordinator
        .last_submission_scope()
        .expect("submission scope");
    assert_eq!(submission_scope.thread_id.as_str(), "thread-alpha");
    assert_eq!(submission_scope.tenant_id.as_str(), "tenant-alpha");
    assert_eq!(
        submission_scope.agent_id.expect("agent").as_str(),
        "agent-alpha"
    );
    assert_eq!(
        submission_scope.project_id.expect("project").as_str(),
        "project-alpha"
    );
    assert_eq!(
        coordinator.last_submission_origin_kind(),
        Some(TurnOriginKind::WebUi),
        "WebUI submit must produce WebUi origin"
    );
}

#[tokio::test]
async fn submit_turn_resolves_model_policy_before_persisting_or_submitting() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let policy = Arc::new(SetupRecordingLlmConfigService::default());
    policy.resolve_next_model_as(Ok(Some("model-approved".to_string())));
    let services = session_services(threads, coordinator.clone()).with_llm_config_service(policy);
    create_thread_for(&services, caller(), "thread-alpha").await;

    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-model-approved",
                "thread_id": "thread-alpha",
                "content": "hello from webui",
                "model": "model-user-requested"
            }))
            .expect("request"),
        )
        .await
        .expect("submit succeeds");

    assert_eq!(
        coordinator.last_requested_model().as_deref(),
        Some("model-approved"),
    );
}

#[tokio::test]
async fn submit_turn_replay_preserves_model_resolved_at_session_acceptance() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::Unavailable {
            reason: "injected first submission failure".to_string(),
        },
    ));
    let policy = Arc::new(SetupRecordingLlmConfigService::default());
    policy.resolve_next_model_as(Ok(Some("model-a".to_string())));
    let services =
        session_services(threads, coordinator.clone()).with_llm_config_service(policy.clone());
    create_thread_for(&services, caller(), "thread-model-replay").await;
    let request = || {
        session_submit_request(json!({
            "client_action_id": "send-model-replay",
            "thread_id": "thread-model-replay",
            "content": "hello from webui"
        }))
        .expect("request")
    };

    services
        .submit_turn(caller(), request())
        .await
        .expect_err("the injected first coordinator submission must fail");

    policy.resolve_next_model_as(Ok(Some("model-b".to_string())));
    services
        .submit_turn(caller(), request())
        .await
        .expect("idempotent session replay succeeds");

    assert_eq!(
        coordinator.last_requested_model().as_deref(),
        Some("model-a")
    );
}

#[tokio::test]
async fn submit_turn_model_policy_backend_fault_stays_retryable() {
    // Regression: `LlmConfigServiceError::Internal` mapped to a PERMANENT
    // policy failure, which the workflow settles durably as
    // Rejected(PolicyDenied) in the session idempotency ledger — the same
    // client_action_id kept failing even after the backend recovered. A
    // backend fault is not a policy verdict: it surfaces transient, settles
    // nothing, and the SAME action id succeeds on retry.
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let policy = Arc::new(SetupRecordingLlmConfigService::default());
    policy.resolve_next_model_as(Err(LlmConfigServiceError::Internal));
    let services =
        session_services(threads, coordinator.clone()).with_llm_config_service(policy.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let request = || {
        session_submit_request(json!({
            "client_action_id": "send-through-backend-fault",
            "thread_id": "thread-alpha",
            "content": "hello from webui"
        }))
        .expect("request")
    };
    services
        .submit_turn(caller(), request())
        .await
        .expect_err("a model-policy backend fault must surface");
    assert_eq!(
        coordinator.submission_count(),
        0,
        "nothing may be submitted while the policy backend is down"
    );

    // Backend recovers; the SAME client_action_id must now succeed — a
    // durable permanent settle would replay the rejection here instead.
    let response = services
        .submit_turn(caller(), request())
        .await
        .expect("the same action id succeeds after the backend recovers");
    assert!(matches!(
        response,
        RebornSubmitTurnResponse::Submitted { .. }
    ));
    assert_eq!(coordinator.submission_count(), 1);
}

#[tokio::test]
async fn submit_turn_rejects_disallowed_model_before_message_side_effects() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let policy = Arc::new(SetupRecordingLlmConfigService::default());
    policy.resolve_next_model_as(Err(LlmConfigServiceError::InvalidRequest {
        field: Some("model".to_string()),
        reason: "model is not allowed".to_string(),
    }));
    let services = session_services(threads, coordinator.clone()).with_llm_config_service(policy);
    create_thread_for(&services, caller(), "thread-alpha").await;

    let error = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-model-denied",
                "thread_id": "thread-alpha",
                "content": "must not persist",
                "model": "model-denied"
            }))
            .expect("request"),
        )
        .await
        .expect_err("model must be denied");

    assert_eq!(error.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(coordinator.submission_count(), 0);
    let timeline = services
        .get_timeline(caller(), RebornTimelineRequest::new("thread-alpha"))
        .await
        .expect("timeline");
    assert!(timeline.messages.is_empty());
}

#[tokio::test]
async fn submit_turn_replays_an_accepted_message_before_rechecking_model_policy() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let policy = Arc::new(SetupRecordingLlmConfigService::default());
    policy.resolve_next_model_as(Ok(Some("model-approved".to_string())));
    let first = session_services(Arc::clone(&threads), coordinator.clone())
        .with_llm_config_service(policy.clone());
    create_thread_for(&first, caller(), "thread-alpha").await;

    let request = || {
        session_submit_request(json!({
            "client_action_id": "send-model-replay",
            "thread_id": "thread-alpha",
            "content": "hello from webui",
            "model": "model-user-requested"
        }))
        .expect("request")
    };
    first
        .submit_turn(caller(), request())
        .await
        .expect("first submit succeeds");

    policy.resolve_next_model_as(Err(LlmConfigServiceError::InvalidRequest {
        field: Some("model".to_string()),
        reason: "model is no longer allowed".to_string(),
    }));
    let replay = session_services(threads, coordinator.clone())
        .with_llm_config_service(policy)
        .submit_turn(caller(), request())
        .await
        .expect("accepted replay remains authoritative");

    assert!(matches!(
        replay,
        RebornSubmitTurnResponse::AlreadySubmitted { .. }
    ));
    assert_eq!(coordinator.submission_count(), 1);
}

#[tokio::test]
async fn submit_turn_records_skill_activation_message_before_turn_wake() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorded_for_hook = Arc::clone(&recorded);
    let services = session_services(threads, coordinator.clone()).with_skill_activation_hooks(
        move |scope, accepted_message_ref, message| {
            recorded_for_hook.lock().expect("lock").push((
                scope.thread_id.as_str().to_string(),
                accepted_message_ref.as_str().to_string(),
                message.to_string(),
            ));
            Ok(())
        },
        |_, _| Ok(()),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let submitted = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-skill-activation",
                "thread_id": "thread-alpha",
                "content": "/code-review inspect this"
            }))
            .expect("request"),
        )
        .await
        .expect("submit succeeds");
    let RebornSubmitTurnResponse::Submitted {
        accepted_message_ref,
        ..
    } = submitted
    else {
        panic!("first submit should be accepted")
    };

    assert_eq!(coordinator.submission_count(), 1);
    assert_eq!(
        recorded.lock().expect("lock").as_slice(),
        &[(
            "thread-alpha".to_string(),
            accepted_message_ref.as_str().to_string(),
            "/code-review inspect this".to_string()
        )]
    );
}

#[tokio::test]
async fn busy_submit_clears_skill_activation_message() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let active_run_id = TurnRunId::new();
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id,
            status: TurnStatus::Running,
            event_cursor: EventCursor(17),
        }),
    ));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let cleared = Arc::new(Mutex::new(Vec::new()));
    let recorded_for_hook = Arc::clone(&recorded);
    let cleared_for_hook = Arc::clone(&cleared);
    let services = session_services(threads, coordinator.clone()).with_skill_activation_hooks(
        move |scope, accepted_message_ref, message| {
            recorded_for_hook.lock().expect("lock").push((
                scope.thread_id.as_str().to_string(),
                accepted_message_ref.as_str().to_string(),
                message.to_string(),
            ));
            Ok(())
        },
        move |scope, accepted_message_ref| {
            cleared_for_hook.lock().expect("lock").push((
                scope.thread_id.as_str().to_string(),
                accepted_message_ref.as_str().to_string(),
            ));
            Ok(())
        },
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let rejected = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-skill-activation-busy",
                "thread_id": "thread-alpha",
                "content": "/code-review inspect this"
            }))
            .expect("request"),
        )
        .await
        .expect("busy submit is rejected");

    assert!(matches!(
        rejected,
        RebornSubmitTurnResponse::RejectedBusy {
            active_run_id: Some(id),
            ..
        } if id == active_run_id
    ));
    assert_eq!(coordinator.submission_count(), 0);
    let recorded = recorded.lock().expect("lock");
    let cleared = cleared.lock().expect("lock");
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        cleared.as_slice(),
        &[(recorded[0].0.clone(), recorded[0].1.clone())],
        "rejected submissions must clear their activation input before returning"
    );
}

#[tokio::test]
async fn submit_turn_returns_internal_when_skill_activation_recorder_fails() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone()).with_skill_activation_hooks(
        |_, _, _| {
            Err(ironclaw_product_contracts::surface::ProductSurfaceError {
                code: ProductSurfaceErrorCode::Internal,
                kind: ProductSurfaceErrorKind::Internal,
                status_code: 500,
                retryable: false,
                field: None,
                validation_code: None,
            })
        },
        |_, _| Ok(()),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-recorder-fails",
                "thread_id": "thread-alpha",
                "content": "/code-review inspect this"
            }))
            .expect("request"),
        )
        .await
        .expect_err("recorder failure is surfaced");

    assert_eq!(err.code, ProductSurfaceErrorCode::Internal);
    assert_eq!(coordinator.submission_count(), 0);
    let timeline = services
        .get_timeline(caller(), RebornTimelineRequest::new("thread-alpha"))
        .await
        .expect("timeline");
    assert_eq!(timeline.messages.len(), 1);
    assert_eq!(timeline.messages[0].status, MessageStatus::Accepted);
}

#[tokio::test]
async fn duplicate_submit_replays_prior_handoff_without_second_submission() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let request = || {
        session_submit_request(json!({
            "client_action_id": "send-duplicate",
            "thread_id": "thread-alpha",
            "content": "hello once"
        }))
        .expect("request")
    };

    services
        .submit_turn(caller(), request())
        .await
        .expect("first submit succeeds");
    let replayed = services
        .submit_turn(caller(), request())
        .await
        .expect("duplicate submit replays");

    assert!(matches!(
        replayed,
        RebornSubmitTurnResponse::AlreadySubmitted { .. }
    ));
    assert_eq!(coordinator.submission_count(), 1);
}

#[tokio::test]
async fn submitted_replay_with_missing_or_invalid_run_id_maps_to_replay_unavailable() {
    for turn_run_id in [None, Some("not-a-uuid".to_string())] {
        let coordinator = Arc::new(FakeTurnCoordinator::default());
        let services = session_services(
            Arc::new(ScriptedThreadService::submitted_replay(turn_run_id)),
            coordinator.clone(),
        );

        let err = services
            .submit_turn(
                caller(),
                session_submit_request(json!({
                    "client_action_id": "send-replay-corrupt",
                    "thread_id": "thread-alpha",
                    "content": "hello from webui"
                }))
                .expect("request"),
            )
            .await
            .expect_err("corrupt submitted replay cannot be reconstructed");

        assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
        assert_eq!(err.kind, ProductSurfaceErrorKind::ReplayUnavailable);
        assert_eq!(err.status_code, 409);
        assert!(!err.retryable);
        assert_eq!(coordinator.submission_count(), 0);
    }
}

#[tokio::test]
async fn submit_turn_rejects_missing_thread_before_turn_submission() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-missing",
                "thread_id": "thread-missing",
                "content": "this thread was never created"
            }))
            .expect("request"),
        )
        .await
        .expect_err("missing thread must reject");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(coordinator.submission_count(), 0);
}

#[tokio::test]
async fn submit_turn_maps_capacity_exceeded_to_non_retryable_rate_limit() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::capacity_exceeded(TurnCapacityResource::SubmitTurn, 1),
    ));
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-capacity",
                "thread_id": "thread-alpha",
                "content": "capacity denied"
            }))
            .expect("request"),
        )
        .await
        .expect_err("capacity error must map through service");

    assert_eq!(err.code, ProductSurfaceErrorCode::RateLimited);
    assert_eq!(err.status_code, 429);
    assert!(!err.retryable);
    assert_eq!(coordinator.submission_count(), 0);
}

#[tokio::test]
async fn submit_turn_rejects_non_owner_before_turn_submission() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .submit_turn(
            caller_for_user("user-beta"),
            session_submit_request(json!({
                "client_action_id": "send-denied",
                "thread_id": "thread-alpha",
                "content": "wrong participant"
            }))
            .expect("request"),
        )
        .await
        .expect_err("non-owner must reject");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(coordinator.submission_count(), 0);
}

#[tokio::test]
async fn same_thread_retry_replays_legacy_submitted_message_after_binding_key_change() {
    let caller = caller();
    let thread_scope = thread_scope_for(&caller);
    let thread_id = ThreadId::new("thread-alpha").expect("valid thread");
    let legacy_binding_id = legacy_webui_source_binding_id_for(&caller, thread_id.as_str());
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    let accepted = thread_service
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: thread_scope.clone(),
            thread_id: thread_id.clone(),
            actor_id: caller.user_id.as_str().to_string(),
            source_binding_id: Some(legacy_binding_id.clone()),
            reply_target_binding_id: Some(legacy_binding_id),
            external_event_id: Some("send-legacy-submitted".to_string()),
            content: MessageContent::text("hello once"),
        })
        .await
        .expect("accepted");
    let run_id = TurnRunId::new();
    thread_service
        .mark_message_submitted(
            &thread_scope,
            &thread_id,
            accepted.message_id,
            "turn-legacy".to_string(),
            run_id.to_string(),
        )
        .await
        .expect("submitted");

    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(thread_service.clone(), coordinator.clone());

    let replayed = services
        .submit_turn(
            caller,
            session_submit_request(json!({
                "client_action_id": "send-legacy-submitted",
                "thread_id": "thread-alpha",
                "content": "hello once"
            }))
            .expect("request"),
        )
        .await
        .expect("legacy submit replays");

    let RebornSubmitTurnResponse::AlreadySubmitted {
        thread_id: replayed_thread_id,
        run_id: replayed_run_id,
        ..
    } = replayed
    else {
        panic!("expected already submitted replay");
    };
    assert_eq!(replayed_thread_id, thread_id);
    assert_eq!(replayed_run_id, run_id);
    assert_eq!(coordinator.submission_count(), 0);
}

#[tokio::test]
async fn same_thread_retry_reuses_legacy_accepted_message_without_creating_duplicate() {
    let caller = caller();
    let thread_scope = thread_scope_for(&caller);
    let thread_id = ThreadId::new("thread-alpha").expect("valid thread");
    let legacy_binding_id = legacy_webui_source_binding_id_for(&caller, thread_id.as_str());
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    let accepted = thread_service
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: thread_scope.clone(),
            thread_id: thread_id.clone(),
            actor_id: caller.user_id.as_str().to_string(),
            source_binding_id: Some(legacy_binding_id.clone()),
            reply_target_binding_id: Some(legacy_binding_id),
            external_event_id: Some("send-legacy-accepted".to_string()),
            content: MessageContent::text("hello once"),
        })
        .await
        .expect("accepted");

    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(thread_service.clone(), coordinator.clone());

    let response = services
        .submit_turn(
            caller.clone(),
            session_submit_request(json!({
                "client_action_id": "send-legacy-accepted",
                "thread_id": "thread-alpha",
                "content": "hello once"
            }))
            .expect("request"),
        )
        .await
        .expect("legacy accepted submit");

    assert!(matches!(
        response,
        RebornSubmitTurnResponse::Submitted { .. }
    ));
    assert_eq!(coordinator.submission_count(), 1);

    let timeline = services
        .get_timeline(
            caller,
            RebornTimelineRequest::new(thread_id.as_str().to_string()),
        )
        .await
        .expect("timeline");
    assert_eq!(timeline.messages.len(), 1);
    assert_eq!(timeline.messages[0].message_id, accepted.message_id);
    assert_eq!(timeline.messages[0].status, MessageStatus::Submitted);
}

#[tokio::test]
async fn duplicate_submit_rejects_cross_thread_reuse_maps_to_duplicate_kind() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    create_thread_for(&services, caller(), "thread-beta").await;

    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-cross-thread",
                "thread_id": "thread-alpha",
                "content": "hello once"
            }))
            .expect("request"),
        )
        .await
        .expect("first submit succeeds");

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-cross-thread",
                "thread_id": "thread-beta",
                "content": "hello twice"
            }))
            .expect("request"),
        )
        .await
        .expect_err("cross-thread duplicate is rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Duplicate);
    assert_eq!(err.status_code, 409);
    assert_eq!(coordinator.submission_count(), 1);

    let alpha_timeline = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("alpha timeline");
    assert_eq!(alpha_timeline.messages.len(), 1);

    let beta_timeline = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-beta".to_string()),
        )
        .await
        .expect("beta timeline");
    assert!(beta_timeline.messages.is_empty());
}

#[tokio::test]
async fn concurrent_duplicate_submit_creates_one_message_and_replays_outcome() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(DefaultTurnCoordinator::new(Arc::new(
        in_memory_agent_turn_runtime(),
    )));
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-alpha").await;
    let services = Arc::new(services);

    let request = || {
        session_submit_request(json!({
            "client_action_id": "send-concurrent",
            "thread_id": "thread-alpha",
            "content": "hello once"
        }))
        .expect("request")
    };

    let first = {
        let services = services.clone();
        tokio::spawn(async move { services.submit_turn(caller(), request()).await })
    };
    let second = {
        let services = services.clone();
        tokio::spawn(async move { services.submit_turn(caller(), request()).await })
    };

    let first = first.await.expect("first task join").expect("first submit");
    let second = second
        .await
        .expect("second task join")
        .expect("second submit");

    let first_run_id = match &first {
        RebornSubmitTurnResponse::Submitted { run_id, .. }
        | RebornSubmitTurnResponse::AlreadySubmitted { run_id, .. } => *run_id,
        RebornSubmitTurnResponse::RejectedBusy { .. }
        | RebornSubmitTurnResponse::DeferredBusy { .. } => {
            panic!("duplicate submit must not defer while deduping")
        }
    };
    let second_run_id = match &second {
        RebornSubmitTurnResponse::Submitted { run_id, .. }
        | RebornSubmitTurnResponse::AlreadySubmitted { run_id, .. } => *run_id,
        RebornSubmitTurnResponse::RejectedBusy { .. }
        | RebornSubmitTurnResponse::DeferredBusy { .. } => {
            panic!("duplicate submit must not defer while deduping")
        }
    };
    assert_eq!(first_run_id, second_run_id);

    let timeline = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("timeline");
    assert_eq!(timeline.messages.len(), 1);
    assert_eq!(timeline.messages[0].status, MessageStatus::Submitted);
    assert_eq!(timeline.messages[0].content.as_deref(), Some("hello once"));
}

#[tokio::test]
async fn refresh_reresolves_thread_to_same_canonical_scope() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-alpha").await;

    let first = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("first resolve");
    let refreshed = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("refresh resolve");

    assert_eq!(first.thread, refreshed.thread);
    assert_eq!(refreshed.thread.thread_id.as_str(), "thread-alpha");
    assert_eq!(refreshed.thread.scope.tenant_id.as_str(), "tenant-alpha");
    assert_eq!(refreshed.thread.scope.agent_id.as_str(), "agent-alpha");
    assert_eq!(
        refreshed
            .thread
            .scope
            .owner_user_id
            .expect("owner")
            .as_str(),
        "user-alpha"
    );
}

#[tokio::test]
async fn duplicate_submit_without_project_id_still_rejects_cross_thread_reuse() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    let caller = caller_with_project(None);
    create_thread_for(&services, caller.clone(), "thread-alpha").await;
    create_thread_for(&services, caller.clone(), "thread-beta").await;

    services
        .submit_turn(
            caller.clone(),
            session_submit_request(json!({
                "client_action_id": "send-no-project",
                "thread_id": "thread-alpha",
                "content": "hello once"
            }))
            .expect("request"),
        )
        .await
        .expect("first submit succeeds");

    let err = services
        .submit_turn(
            caller,
            session_submit_request(json!({
                "client_action_id": "send-no-project",
                "thread_id": "thread-beta",
                "content": "hello twice"
            }))
            .expect("request"),
        )
        .await
        .expect_err("cross-thread duplicate is rejected without a project binding");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Duplicate);
    assert_eq!(err.status_code, 409);
    assert_eq!(coordinator.submission_count(), 1);
}

#[tokio::test]
async fn duplicate_submit_is_isolated_by_project_scope() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(
        &services,
        caller_with_project(Some("project-alpha")),
        "thread-alpha",
    )
    .await;
    create_thread_for(
        &services,
        caller_with_project(Some("project-beta")),
        "thread-beta",
    )
    .await;

    let first = services
        .submit_turn(
            caller_with_project(Some("project-alpha")),
            session_submit_request(json!({
                "client_action_id": "send-project-scoped",
                "thread_id": "thread-alpha",
                "content": "hello alpha"
            }))
            .expect("request"),
        )
        .await
        .expect("project alpha submit");
    let second = services
        .submit_turn(
            caller_with_project(Some("project-beta")),
            session_submit_request(json!({
                "client_action_id": "send-project-scoped",
                "thread_id": "thread-beta",
                "content": "hello beta"
            }))
            .expect("request"),
        )
        .await
        .expect("project beta submit");

    assert!(matches!(first, RebornSubmitTurnResponse::Submitted { .. }));
    assert!(matches!(second, RebornSubmitTurnResponse::Submitted { .. }));
    assert_eq!(coordinator.submission_count(), 2);
}

fn submit_request_for_extension(
    extension_id: Option<&str>,
    thread_id: &str,
    action: &str,
) -> ProductSubmitTurnRequest {
    let mut body = json!({
        "client_action_id": action,
        "thread_id": thread_id,
        "content": "hello channel"
    });
    if let Some(extension_id) = extension_id {
        body["extension_id"] = json!(extension_id);
    }
    serde_json::from_value::<ProductSubmitTurnRequest>(body).expect("request")
}

#[tokio::test]
async fn submit_turn_with_extension_id_requires_a_session_channel_directory() {
    let services = ironclaw_assistant::RebornServices::new(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let error = services
        .submit_turn(
            caller(),
            submit_request_for_extension(Some("web-app"), "thread-alpha", "send-no-directory"),
        )
        .await
        .expect_err("no directory wired must fail closed");
    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
}

#[test]
fn builtin_session_surface_id_matches_the_kernel_transport_constant() {
    // The contracts crate cannot import the kernel (layer order points the
    // other way), so the two spellings of the host transport identity are
    // pinned equal here instead. Both are persisted coordinates; neither may
    // move independently.
    assert_eq!(
        ironclaw_product_contracts::session_ingress::BUILTIN_SESSION_SURFACE_ID,
        ironclaw_turns::product_context::WEBUI_SOURCE_CHANNEL,
    );
}

#[tokio::test]
async fn submit_turn_without_extension_id_admits_under_the_legacy_api_surface() {
    // The unparameterized lane is the OpenAI-compatible API's documented wire
    // shape (`ProductSubmitTurnRequest::extension_id`): headless SDK clients
    // cannot learn a channel id from `GET /session`, so `None` submits under
    // the built-in surface identity — even on a deployment with no session
    // channel directory at all. Regression: 97274d5c9a removed this arm and
    // hard-404'd every `/v1/chat/completions` submission.
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    let response = services
        .submit_turn(
            caller(),
            submit_request_for_extension(None, "thread-alpha", "send-without-channel"),
        )
        .await
        .expect("API transports submit under the legacy surface without a channel parameter");
    assert!(matches!(
        response,
        RebornSubmitTurnResponse::Submitted { .. }
    ));
    assert_eq!(coordinator.submission_count(), 1);
}

#[tokio::test]
async fn submit_turn_rejects_an_unknown_or_non_session_extension_as_not_found() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone()).with_session_channel_directory(
        Arc::new(StaticSessionChannelDirectory {
            session_channels: vec!["web-app"],
        }),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let error = services
        .submit_turn(
            caller(),
            submit_request_for_extension(Some("slack"), "thread-alpha", "send-wrong-channel"),
        )
        .await
        .expect_err("a webhook channel must never admit session submissions");
    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);

    // The built-in surface id is not route-addressable either: naming it
    // through the parameterized route must 404 unless a manifest channel
    // actually claims it. Only the unparameterized API lane reaches it.
    let error = services
        .submit_turn(
            caller(),
            submit_request_for_extension(Some("webui"), "thread-alpha", "send-builtin-by-name"),
        )
        .await
        .expect_err("the built-in surface id is not a route-addressable channel");
    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(
        coordinator.submission_count(),
        0,
        "nothing may be submitted under a rejected channel identity"
    );
}

#[tokio::test]
async fn submit_turn_admits_a_declared_session_channel() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone()).with_session_channel_directory(
        Arc::new(StaticSessionChannelDirectory {
            session_channels: vec!["web-app"],
        }),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .submit_turn(
            caller(),
            submit_request_for_extension(Some("web-app"), "thread-alpha", "send-on-channel"),
        )
        .await
        .expect("session channel submission admits");
    assert!(matches!(
        response,
        RebornSubmitTurnResponse::Submitted { .. }
    ));
    assert_eq!(coordinator.submission_count(), 1);
}
