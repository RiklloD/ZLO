//! Approval, auth, and native-question gate resolution.

use super::*;

#[derive(Default)]
struct RecordingAuthInteractionService {
    resolutions: Mutex<Vec<ResolveAuthInteractionRequest>>,
}

impl RecordingAuthInteractionService {
    fn resolution_count(&self) -> usize {
        self.resolutions.lock().expect("lock").len()
    }

    fn last_resolution(&self) -> Option<ResolveAuthInteractionRequest> {
        self.resolutions.lock().expect("lock").last().cloned()
    }
}

#[async_trait]
impl AuthInteractionService for RecordingAuthInteractionService {
    async fn list_pending(
        &self,
        _request: ListPendingAuthInteractionsRequest,
    ) -> Result<ListPendingAuthInteractionsResponse, ironclaw_assistant::ProductSurfaceFailure>
    {
        Ok(ListPendingAuthInteractionsResponse {
            auth_interactions: vec![],
        })
    }

    async fn resolve(
        &self,
        request: ResolveAuthInteractionRequest,
    ) -> Result<ResolveAuthInteractionResponse, ironclaw_assistant::ProductSurfaceFailure> {
        let run_id = request.run_id_hint.expect("webui passes run_id");
        let decision = request.decision.clone();
        self.resolutions.lock().expect("lock").push(request);
        Ok(match decision {
            AuthInteractionDecision::CredentialProvided { .. }
            | AuthInteractionDecision::CallbackCompleted { .. } => {
                ResolveAuthInteractionResponse::Resumed(ResumeTurnResponse {
                    run_id,
                    status: TurnStatus::Queued,
                    event_cursor: EventCursor(29),
                })
            }
            AuthInteractionDecision::Deny => {
                ResolveAuthInteractionResponse::Canceled(CancelRunResponse {
                    run_id,
                    status: TurnStatus::Cancelled,
                    event_cursor: EventCursor(31),
                    already_terminal: false,
                    actor: None,
                })
            }
        })
    }
}

#[tokio::test]
async fn approved_gate_resolution_resumes_turn() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-1",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect("gate resolution succeeds");

    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(coordinator.resumption_count(), 1);
    assert_eq!(
        coordinator.last_resumption_precondition(),
        Some(ResumeTurnPrecondition::AnyBlockedGate)
    );
}

/// A WebUI run parked on the native `ask_user` call, for the gate tests.
async fn parked_native_question(
    allow_free_text: bool,
) -> (
    RebornServices,
    Arc<FakeTurnCoordinator>,
    Arc<ironclaw_turns::InMemoryExternalToolCatalog>,
    TurnRunId,
) {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let catalog = Arc::new(ironclaw_turns::InMemoryExternalToolCatalog::new());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_external_tool_catalog(catalog.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_gate(TurnGateRef::new("gate:external_tool-q1").expect("gate"));
    coordinator.set_run_state_status(TurnStatus::BlockedExternalTool);
    let run = TurnRunId::parse(&run_id_string()).expect("run id");
    let arguments = json!({"questions": [{
        "question": "Which database?",
        "options": [{"label": "Postgres"}, {"label": "SQLite"}],
        "allow_free_text": allow_free_text
    }]});
    ironclaw_turns::ExternalToolCatalog::record_pending_call(
        catalog.as_ref(),
        run,
        ironclaw_turns::PendingExternalCall::new(
            "call_1",
            ironclaw_assistant::native_questions::ASK_USER_TOOL_NAME,
            arguments,
        ),
    )
    .await
    .expect("pending call");
    (services, coordinator, catalog, run)
}

fn question_resolution(
    gate: &str,
    resolution: &str,
    answer: serde_json::Value,
) -> ProductResolveGateRequest {
    serde_json::from_value(json!({
        "client_action_id": format!("q-{resolution}"),
        "thread_id": "thread-alpha",
        "run_id": run_id_string(),
        "gate_ref": gate,
        "resolution": resolution,
        "answer": answer
    }))
    .expect("request")
}

#[tokio::test]
async fn native_ask_user_answer_is_submitted_to_the_parked_call_and_resumes() {
    let (services, coordinator, catalog, run) = parked_native_question(false).await;
    let output = |catalog: Arc<ironclaw_turns::InMemoryExternalToolCatalog>| async move {
        ironclaw_turns::ExternalToolCatalog::output(catalog.as_ref(), run, "call_1")
            .await
            .expect("catalog read")
    };

    // An answer outside the options (no free text allowed) is refused and
    // the run stays parked with nothing submitted.
    let err = services
        .resolve_gate(
            caller(),
            question_resolution(
                "gate:external_tool-q1",
                "answered",
                json!({"values": {"q1": ["MySQL"]}}),
            ),
        )
        .await
        .expect_err("a misfit answer is refused");
    assert_eq!(err.status_code, 400);
    assert_eq!(coordinator.resumption_count(), 0);
    assert!(output(catalog.clone()).await.is_none());

    let response = services
        .resolve_gate(
            caller(),
            question_resolution(
                "gate:external_tool-q1",
                "answered",
                json!({"values": {"q1": ["SQLite"]}}),
            ),
        )
        .await
        .expect("the answer resumes the run");
    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(
        coordinator.last_resumption_precondition(),
        Some(ResumeTurnPrecondition::BlockedExternalToolGate)
    );
    let submitted = output(catalog.clone())
        .await
        .expect("the answer is the call's output");
    assert_eq!(submitted["answered"], true);
    assert_eq!(submitted["answers"]["Which database?"], json!(["SQLite"]));
}

#[tokio::test]
async fn native_ask_user_dismissal_resumes_with_a_decline_and_answers_need_a_question() {
    let (services, coordinator, catalog, run) = parked_native_question(true).await;
    let response = services
        .resolve_gate(
            caller(),
            question_resolution("gate:external_tool-q1", "declined", json!(null)),
        )
        .await
        .expect("a dismissal resumes the run");
    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(
        coordinator.cancellation_count(),
        0,
        "dismissing is not cancelling"
    );
    let submitted = ironclaw_turns::ExternalToolCatalog::output(catalog.as_ref(), run, "call_1")
        .await
        .expect("catalog read")
        .expect("the decline is the call's output");
    assert_eq!(submitted["answered"], false);

    // "answered" for a gate that is not a waiting question is refused.
    let (services, coordinator, _catalog, _run) = parked_native_question(true).await;
    coordinator.set_parked_approval_gate(TurnGateRef::new("gate:approval-x").expect("gate"));
    coordinator.set_run_state_status(TurnStatus::BlockedApproval);
    let err = services
        .resolve_gate(
            caller(),
            question_resolution(
                "gate:approval-x",
                "answered",
                json!({"values": {"q1": ["SQLite"]}}),
            ),
        )
        .await
        .expect_err("an answer cannot resolve an approval");
    assert_eq!(err.status_code, 400);
    assert_eq!(coordinator.resumption_count(), 0);
}

#[tokio::test]
async fn resolve_gate_rejects_missing_run_state_actor() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_gate(TurnGateRef::new("gate-alpha").expect("gate"));
    coordinator.set_run_state_actor(None);

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-missing-actor",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect_err("missing run-state actor must fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::Forbidden);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ParticipantDenied);
    assert_eq!(err.status_code, 403);
    assert_eq!(coordinator.cancellation_count(), 0);
}

#[tokio::test]
async fn resolve_gate_rejects_mismatched_run_state_actor() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_gate(TurnGateRef::new("gate-alpha").expect("gate"));
    coordinator.set_run_state_actor(Some(turn_actor_for_user("user-beta")));

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-mismatched-actor",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect_err("mismatched run-state actor must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::Forbidden);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ParticipantDenied);
    assert_eq!(err.status_code, 403);
    assert_eq!(coordinator.cancellation_count(), 0);
}

#[tokio::test]
async fn generic_gate_resolution_rejects_blocked_auth_run() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_auth_gate(TurnGateRef::new("custom-auth-gate").expect("gate"));

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-auth-fallback",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "custom-auth-gate",
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect_err("generic resolver must not resume auth gate");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedAuthentication);
    assert_eq!(coordinator.resumption_count(), 0);
}

#[tokio::test]
async fn blocked_auth_run_routes_non_prefixed_gate_to_auth_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let auth_interactions = Arc::new(RecordingAuthInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_auth_interactions(auth_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_auth_gate(TurnGateRef::new("custom-auth-gate").expect("gate"));

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-auth-state-routed",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "custom-auth-gate",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect("blocked auth status routes to auth interaction service");

    assert!(matches!(response, RebornResolveGateResponse::Cancelled(_)));
    assert_eq!(auth_interactions.resolution_count(), 1);
    let resolution = auth_interactions.last_resolution().expect("resolution");
    assert_eq!(resolution.gate_ref.as_str(), "custom-auth-gate");
    assert_eq!(resolution.decision, AuthInteractionDecision::Deny);
    assert_eq!(coordinator.cancellation_count(), 0);
}

#[tokio::test]
async fn blocked_auth_run_with_stale_gate_ref_returns_conflict() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let auth_interactions = Arc::new(RecordingAuthInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_auth_interactions(auth_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_auth_gate(TurnGateRef::new("gate-current").expect("gate"));

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-auth-stale",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-stale",
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect_err("stale auth gate_ref must produce Conflict");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedAuthentication);
    assert_eq!(err.status_code, 409);
    assert_eq!(coordinator.resumption_count(), 0);
    assert_eq!(coordinator.cancellation_count(), 0);
    assert_eq!(auth_interactions.resolution_count(), 0);
}

#[tokio::test]
async fn blocked_approval_run_routes_non_prefixed_gate_to_approval_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_approval_interactions(approval_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_approval_gate(TurnGateRef::new("custom-approval-gate").expect("gate"));

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-approval-state-routed",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "custom-approval-gate",
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect("blocked approval status routes to approval interaction service");

    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(approval_interactions.resolution_count(), 1);
    let resolution = approval_interactions.last_resolution().expect("resolution");
    assert_eq!(resolution.gate_ref.as_str(), "custom-approval-gate");
    assert_eq!(
        resolution.decision,
        ApprovalInteractionDecision::ApproveOnce
    );
    assert_eq!(coordinator.resumption_count(), 0);
}

#[tokio::test]
async fn blocked_approval_run_with_stale_gate_ref_returns_conflict() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_approval_interactions(approval_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_approval_gate(TurnGateRef::new("gate-current").expect("gate"));

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-approval-stale",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-stale",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect_err("stale approval gate_ref must produce Conflict");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedApproval);
    assert_eq!(err.status_code, 409);
    assert_eq!(coordinator.resumption_count(), 0);
    assert_eq!(coordinator.cancellation_count(), 0);
    assert_eq!(approval_interactions.resolution_count(), 0);
}

#[tokio::test]
async fn terminal_run_state_rejects_gate_resolution_before_shape_fallback() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_approval_interactions(approval_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_run_state_status(TurnStatus::Completed);
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate ref");

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-terminal",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": gate_ref.as_str(),
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect_err("terminal run must fail closed before shape fallback");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Conflict);
    assert_eq!(err.status_code, 409);
    assert_eq!(coordinator.resumption_count(), 0);
    assert_eq!(coordinator.cancellation_count(), 0);
    assert_eq!(approval_interactions.resolution_count(), 0);
}

#[tokio::test]
async fn approval_gate_resolution_uses_approval_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_approval_interactions(approval_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate ref");

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "approval-gate-1",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": gate_ref.as_str(),
                "resolution": "approved"
            }))
            .expect("request"),
        )
        .await
        .expect("approval gate resolution succeeds");

    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(approval_interactions.resolution_count(), 1);
    assert_eq!(coordinator.resumption_count(), 0);
    assert_eq!(
        approval_interactions
            .last_resolution()
            .expect("resolution")
            .gate_ref,
        gate_ref
    );
}

#[tokio::test]
async fn approval_gate_denial_uses_approval_interaction_service_and_returns_cancelled() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_approval_interactions(approval_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate ref");

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "approval-gate-deny",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": gate_ref.as_str(),
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect("approval gate denial succeeds");

    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(approval_interactions.resolution_count(), 1);
    assert_eq!(coordinator.cancellation_count(), 0);
    assert_eq!(
        approval_interactions
            .last_resolution()
            .expect("resolution")
            .decision,
        ApprovalInteractionDecision::Deny { feedback: None }
    );
}

#[tokio::test]
async fn credential_gate_resolution_returns_sanitized_stable_error_until_gate_port_exists() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;
    let credential_ref = CredentialAccountId::new();

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-credential",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "credential_provided",
                "credential_ref": credential_ref.to_string()
            }))
            .expect("request"),
        )
        .await
        .expect_err("credential resolution is not wired yet");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedAuthentication);
    assert_eq!(err.status_code, 503);
    assert_eq!(coordinator.resumption_count(), 0);
    let rendered = format!("{err:?} {}", serde_json::to_string(&err).expect("json"));
    assert!(!rendered.contains(credential_ref.to_string().as_str()));
}

#[tokio::test]
async fn auth_gate_credential_resolution_uses_auth_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let auth_interactions = Arc::new(RecordingAuthInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_auth_interactions(auth_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    let credential_ref = CredentialAccountId::new();

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-credential",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate:auth-alpha",
                "resolution": "credential_provided",
                "credential_ref": credential_ref.to_string()
            }))
            .expect("request"),
        )
        .await
        .expect("credential resolution routes through auth interaction service");

    assert!(matches!(response, RebornResolveGateResponse::Resumed(_)));
    assert_eq!(auth_interactions.resolution_count(), 1);
    let resolution = auth_interactions.last_resolution().expect("resolution");
    assert_eq!(resolution.gate_ref.as_str(), "gate:auth-alpha");
    assert_eq!(
        resolution.decision,
        AuthInteractionDecision::CredentialProvided { credential_ref }
    );
    assert_eq!(coordinator.resumption_count(), 0);
}

#[tokio::test]
async fn hook_auth_gate_denial_uses_auth_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let auth_interactions = Arc::new(RecordingAuthInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_auth_interactions(auth_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-auth-deny",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate:hook-auth-alpha",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect("auth denial routes through auth interaction service");

    assert!(matches!(response, RebornResolveGateResponse::Cancelled(_)));
    assert_eq!(auth_interactions.resolution_count(), 1);
    let resolution = auth_interactions.last_resolution().expect("resolution");
    assert_eq!(resolution.gate_ref.as_str(), "gate:hook-auth-alpha");
    assert_eq!(resolution.decision, AuthInteractionDecision::Deny);
    assert_eq!(coordinator.cancellation_count(), 0);
}

/// A minimal auth-interaction stub that returns `Resumed` for every
/// Deny decision, mirroring the production path where the model is resumed
/// so it can surface the denial to the user.
struct DeniedResumedAuthInteractionService;

#[async_trait]
impl AuthInteractionService for DeniedResumedAuthInteractionService {
    async fn list_pending(
        &self,
        _request: ListPendingAuthInteractionsRequest,
    ) -> Result<ListPendingAuthInteractionsResponse, ProductSurfaceFailure> {
        Ok(ListPendingAuthInteractionsResponse {
            auth_interactions: vec![],
        })
    }

    async fn resolve(
        &self,
        request: ResolveAuthInteractionRequest,
    ) -> Result<ResolveAuthInteractionResponse, ProductSurfaceFailure> {
        let run_id = request.run_id_hint.expect("webui passes run_id");
        Ok(ResolveAuthInteractionResponse::Resumed(
            ResumeTurnResponse {
                run_id,
                status: TurnStatus::Queued,
                event_cursor: EventCursor(37),
            },
        ))
    }
}

#[tokio::test]
async fn hook_auth_gate_denial_maps_to_reborn_resumed() {
    // Verifies that a Deny decision (which produces `Resumed` from
    // `resume_denied_auth`) maps to `RebornResolveGateResponse::Resumed`
    // through the service.
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let auth_interactions = Arc::new(DeniedResumedAuthInteractionService);
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_auth_interactions(auth_interactions);
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-auth-denial-resumed",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate:hook-auth-denial-resumed",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect(
            "Resumed from auth-interaction service must map to RebornResolveGateResponse::Resumed",
        );

    assert!(
        matches!(response, RebornResolveGateResponse::Resumed(_)),
        "expected Resumed, got: {response:?}"
    );
    assert_eq!(coordinator.cancellation_count(), 0);
}

#[tokio::test]
async fn missing_run_state_for_auth_gate_still_routes_to_auth_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::with_run_state_error(
        TurnError::ScopeNotFound,
    ));
    let auth_interactions = Arc::new(RecordingAuthInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_auth_interactions(auth_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-auth-missing-run",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate:hook-auth-missing",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect("typed auth gate routes to auth interaction service when run-state is gone");

    assert!(matches!(response, RebornResolveGateResponse::Cancelled(_)));
    assert_eq!(auth_interactions.resolution_count(), 1);
    assert_eq!(
        auth_interactions
            .last_resolution()
            .expect("resolution")
            .gate_ref
            .as_str(),
        "gate:hook-auth-missing"
    );
    assert_eq!(coordinator.cancellation_count(), 0);
}

#[tokio::test]
async fn denied_gate_resolution_cancels_run() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;
    coordinator.set_parked_gate(TurnGateRef::new("gate-alpha").expect("gate"));

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-2",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect("gate denial succeeds");

    assert!(matches!(response, RebornResolveGateResponse::Cancelled(_)));
    assert_eq!(coordinator.cancellation_count(), 1);
}

// Regression: cancel_run must reject when the authenticated user does not own
// the thread. TurnScope only carries (tenant, agent, project, thread_id), so
// without this check any caller sharing an agent scope could cancel another
// user's run by guessing the run_id.
#[tokio::test]
async fn cancel_run_rejects_cross_user_access() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    let alice = caller();
    create_thread_for(&services, alice.clone(), "thread-alice").await;

    // Bob shares Alice's (tenant, agent, project) scope and guesses her thread.
    let bob = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("user-bob").expect("user"),
        alice.agent_id.clone(),
        alice.project_id.clone(),
    );

    let err = services
        .cancel_run(
            bob,
            serde_json::from_value::<ProductCancelRunRequest>(json!({
                "client_action_id": "cancel-cross",
                "thread_id": "thread-alice",
                "run_id": run_id_string(),
                "reason": "user_requested"
            }))
            .expect("request"),
        )
        .await
        .expect_err("cross-user cancel must be rejected");

    // 404 rather than 403 so the existence of Alice's thread is not leaked.
    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        coordinator.cancellation_count(),
        0,
        "turn coordinator must NOT be called for cross-user cancel"
    );
}

// Regression: the ownership probe must collapse `ThreadScopeMismatch` and
// `UnknownThread` into the same NotFound response. Current backends return
// `UnknownThread` for `list_thread_history` scope mismatches, but the contract
// also permits `ThreadScopeMismatch`; if a future backend change starts
// emitting it, the default `map_thread_error` path would surface 409 Conflict
// instead, signalling to attackers that the thread exists under a different
// owner. Lock in the explicit remap by driving cancel_run through a stub that
// always returns `ThreadScopeMismatch`.
#[tokio::test]
async fn cancel_run_remaps_thread_scope_mismatch_to_not_found() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(Arc::new(ScopeMismatchThreadStub), coordinator.clone());

    let err = services
        .cancel_run(
            caller(),
            serde_json::from_value::<ProductCancelRunRequest>(json!({
                "client_action_id": "cancel-scope-mismatch",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "reason": "user_requested"
            }))
            .expect("request"),
        )
        .await
        .expect_err("scope mismatch must surface as NotFound");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        coordinator.cancellation_count(),
        0,
        "turn coordinator must NOT be called when ownership probe fails"
    );
}

// Regression: resolve_gate must reject when the authenticated user does not
// own the thread, for both the approve→resume path and the deny/cancel path.
#[tokio::test]
async fn resolve_gate_rejects_cross_user_access() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    let alice = caller();
    create_thread_for(&services, alice.clone(), "thread-alice").await;
    coordinator.set_parked_gate(TurnGateRef::new("gate-alpha").expect("gate"));

    let bob = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("user-bob").expect("user"),
        alice.agent_id.clone(),
        alice.project_id.clone(),
    );

    let err = services
        .resolve_gate(
            bob,
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-cross",
                "thread_id": "thread-alice",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect_err("cross-user gate resolution must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        coordinator.cancellation_count(),
        0,
        "turn coordinator must NOT be called for cross-user resolve"
    );
}

// Regression: stream_events shares the TurnScope shape with cancel_run /
// resolve_gate / get_run_state — none of which carry owner_user_id — so the
// projection drain must be gated by the same ownership probe. Without it, a
// caller who shares the (tenant, agent, project) scope could read another
// user's projection feed by guessing thread_id.
#[tokio::test]
async fn stream_events_rejects_cross_user_access() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let event_stream = Arc::new(RecordingProjectionStream::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_event_stream(event_stream.clone());
    let alice = caller();
    setup_owned_thread(&services, alice.clone(), "thread-alice").await;

    let bob = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("user-bob").expect("user"),
        alice.agent_id.clone(),
        alice.project_id.clone(),
    );

    let err = services
        .stream_events(
            bob,
            RebornStreamEventsRequest {
                thread_id: "thread-alice".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("cross-user stream_events must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        event_stream.drain_count(),
        0,
        "projection stream must NOT be drained when ownership probe fails"
    );
}

// Regression: when create_thread is given an explicit `requested_thread_id`,
// a thread that already exists under a different owner would surface as
// `ThreadScopeMismatch` → `409 Conflict` via `map_thread_error`. That gives
// any caller sharing the (tenant, agent, project) scope an existence oracle
// for thread ids they did not create. Explicit-id collisions must redact to
// the same `NotFound` outcome as the cancel_run / resolve_gate / stream_events
// ownership probe. The auto-generated path keeps `map_thread_error` since the
// caller cannot usefully probe deterministically-derived UUIDv5 ids.
#[tokio::test]
async fn create_thread_explicit_id_collision_remaps_to_not_found() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let alice = caller();
    setup_owned_thread(&services, alice.clone(), "thread-alice").await;

    let bob = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("user-bob").expect("user"),
        alice.agent_id.clone(),
        alice.project_id.clone(),
    );

    let err = services
        .create_thread(
            bob,
            serde_json::from_value::<ProductCreateThreadRequest>(json!({
                "client_action_id": "create-cross",
                "requested_thread_id": "thread-alice",
            }))
            .expect("request"),
        )
        .await
        .expect_err("cross-user create_thread with explicit id must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
}

// Regression: cancel_run is not gate-aware, so without a parked-on-gate check
// a denied/cancelled resolution carrying a stale or attacker-supplied gate_ref
// would cancel any non-terminal run with the matching run_id. Mismatched gate
// must produce Conflict and cancel_run must never be invoked.
#[tokio::test]
async fn denied_gate_resolution_with_stale_gate_ref_returns_conflict() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;
    // The run is parked on `gate-current`, but the browser supplies `gate-stale`.
    coordinator.set_parked_gate(TurnGateRef::new("gate-current").expect("gate"));

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-stale",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-stale",
                "resolution": "declined"
            }))
            .expect("request"),
        )
        .await
        .expect_err("stale gate_ref must produce Conflict, not silent cancel");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedApproval);
    assert_eq!(err.status_code, 409);
    assert_eq!(
        coordinator.cancellation_count(),
        0,
        "cancel_run must NOT be called for stale gate_ref"
    );
}

#[tokio::test]
async fn generic_gate_resolution_with_persistent_flag_is_rejected() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "gate-always",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": "gate-alpha",
                "resolution": "approved",
                "always": true,
            }))
            .expect("request"),
        )
        .await
        .expect_err("generic persistent gate resolution must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedApproval);
    assert_eq!(err.status_code, 503);
    assert_eq!(
        coordinator.resumption_count(),
        0,
        "resume_turn must NOT be called for unsupported generic persistent gate"
    );
}

#[tokio::test]
async fn approval_gate_resolution_with_persistent_flag_uses_approval_interaction_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let approval_interactions = Arc::new(RecordingApprovalInteractionService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_approval_interactions(approval_interactions.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate ref");

    let response = services
        .resolve_gate(
            caller(),
            serde_json::from_value::<ProductResolveGateRequest>(json!({
                "client_action_id": "approval-gate-always",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "gate_ref": gate_ref.as_str(),
                "resolution": "approved",
                "always": true,
            }))
            .expect("request"),
        )
        .await
        .expect("persistent approval resolution succeeds");

    assert!(matches!(
        response,
        RebornResolveGateResponse::Resumed(response) if response.status == TurnStatus::Queued
    ));
    assert_eq!(approval_interactions.resolution_count(), 1);
    assert_eq!(
        approval_interactions
            .last_resolution()
            .expect("resolution")
            .decision,
        ApprovalInteractionDecision::AlwaysAllow
    );
    assert_eq!(coordinator.resumption_count(), 0);
}
