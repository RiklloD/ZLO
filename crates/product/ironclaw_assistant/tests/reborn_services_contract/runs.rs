//! Run control: cancel, retry, and run-state reads.

use super::*;

#[tokio::test]
async fn cancel_run_uses_turn_service_and_stable_response() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .cancel_run(
            caller(),
            serde_json::from_value::<ProductCancelRunRequest>(json!({
                "client_action_id": "cancel-1",
                "thread_id": "thread-alpha",
                "run_id": run_id_string(),
                "reason": "user_requested"
            }))
            .expect("request"),
        )
        .await
        .expect("cancel succeeds");

    assert_eq!(response.status, TurnStatus::Cancelled);
    assert_eq!(response.event_cursor, EventCursor(13));
    assert!(!response.already_terminal);
    assert_eq!(coordinator.cancellation_count(), 1);
}

#[tokio::test]
async fn retry_run_uses_turn_service_and_stable_response() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .retry_run(
            caller(),
            serde_json::from_value::<ProductRetryRunRequest>(json!({
                "client_action_id": "retry-1",
                "thread_id": "thread-alpha",
                "run_id": run_id_string()
            }))
            .expect("request"),
        )
        .await
        .expect("retry succeeds");

    assert_eq!(response.status, TurnStatus::Queued);
    assert_eq!(response.event_cursor, EventCursor(19));
    assert_eq!(coordinator.retry_count(), 1);
    let retry = coordinator.last_retry().expect("retry request");
    assert_eq!(
        retry.run_id,
        TurnRunId::parse(&run_id_string()).expect("run id")
    );
    assert_eq!(retry.actor, caller().actor());
    assert_eq!(
        retry.scope,
        caller().turn_scope(ThreadId::new("thread-alpha").expect("thread"))
    );
    assert_eq!(retry.idempotency_key.as_str(), "retry-1");
}

#[tokio::test]
async fn retry_run_rejects_invalid_run_id_without_turn_service() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );

    let err = services
        .retry_run(
            caller(),
            serde_json::from_value::<ProductRetryRunRequest>(json!({
                "client_action_id": "retry-invalid-run",
                "thread_id": "thread-alpha",
                "run_id": "not-a-run-uuid"
            }))
            .expect("request"),
        )
        .await
        .expect_err("invalid run id should fail validation");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("run_id"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::InvalidId)
    );
    assert_eq!(
        coordinator.retry_attempt_count(),
        0,
        "validation must fail before TurnCoordinator::retry_turn"
    );
    assert_eq!(coordinator.retry_count(), 0);
}

#[tokio::test]
async fn retry_run_maps_not_retryable_to_non_retryable_conflict() {
    let run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    let coordinator = Arc::new(FakeTurnCoordinator::with_retry_error(
        TurnError::RunNotRetryable { run_id },
    ));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .retry_run(
            caller(),
            serde_json::from_value::<ProductRetryRunRequest>(json!({
                "client_action_id": "retry-not-retryable",
                "thread_id": "thread-alpha",
                "run_id": run_id_string()
            }))
            .expect("request"),
        )
        .await
        .expect_err("not retryable maps to conflict");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Conflict);
    assert_eq!(err.status_code, 409);
    assert!(!err.retryable);
    assert_eq!(coordinator.retry_attempt_count(), 1);
    assert_eq!(coordinator.retry_count(), 0);
}

#[tokio::test]
async fn retry_run_rejects_cross_user_access() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    let alice = caller();
    create_thread_for(&services, alice.clone(), "thread-alice").await;

    let bob = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("user-bob").expect("user"),
        alice.agent_id.clone(),
        alice.project_id.clone(),
    );

    let err = services
        .retry_run(
            bob,
            serde_json::from_value::<ProductRetryRunRequest>(json!({
                "client_action_id": "retry-cross",
                "thread_id": "thread-alice",
                "run_id": run_id_string()
            }))
            .expect("request"),
        )
        .await
        .expect_err("cross-user retry must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        coordinator.retry_count(),
        0,
        "turn coordinator must NOT be called for cross-user retry"
    );
}

#[tokio::test]
async fn get_run_state_returns_stable_dto_without_m3_internal_fields() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    // A failed run carries a model-visible `detail` (free-form backend cause
    // text, scrubbed only for secret VALUES). The public run-state DTO must
    // keep the user-facing `category` but strip `detail` so internal
    // diagnostics never reach the browser (see
    // `SanitizedFailure::public_projection`).
    coordinator.set_run_state_failure(
        SanitizedFailure::new("model_unavailable")
            .expect("valid category")
            .with_detail("HTTP 500 from provider at /internal/models/route-xyz"),
    );
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    setup_owned_thread(&services, caller(), "thread-alpha").await;

    let response = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: "thread-alpha".to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect("get_run_state succeeds");

    assert_eq!(response.run_id.as_uuid().to_string(), run_id_string());
    assert_eq!(response.status, TurnStatus::Queued);
    assert_eq!(response.event_cursor, EventCursor(17));
    assert_eq!(response.accepted_message_ref.as_str(), "msg:replayed");
    assert_eq!(response.resolved_run_profile_version, 1);
    assert_eq!(
        response.resolved_run_profile_id,
        RunProfileId::default_profile().as_str()
    );
    assert!(response.gate_ref.is_none());
    // The user-facing category survives; the model-visible detail is stripped.
    let failure = response.failure.as_ref().expect("failure present");
    assert_eq!(failure.category(), "model_unavailable");
    assert_eq!(
        failure.detail(),
        None,
        "public run-state DTO must not expose the model-visible failure detail"
    );
    assert!(response.checkpoint_id.is_none());
    assert_eq!(coordinator.run_state_request_count(), 1);

    // Stable DTO must not surface M3-internal binding refs, model route, raw
    // turn scope, or the internal failure detail to WebUI consumers.
    let rendered = serde_json::to_string(&response).expect("json");
    assert!(!rendered.contains("source_binding_ref"));
    assert!(!rendered.contains("reply_target_binding_ref"));
    assert!(!rendered.contains("resolved_model_route"));
    assert!(!rendered.contains("webui-src:replayed"));
    assert!(!rendered.contains("webui-reply:replayed"));
    assert!(!rendered.contains("\"scope\""));
    assert!(!rendered.contains("\"detail\""));
    assert!(!rendered.contains("/internal/models/route-xyz"));
    // With no reported usage, the token/cost fields are omitted entirely.
    assert!(!rendered.contains("\"usage\""));
    assert!(!rendered.contains("\"cost\""));
}

#[tokio::test]
async fn get_run_state_surfaces_token_usage_and_priced_cost() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    // A run that reported usage against a concrete (gpt-4o) model surfaces both
    // the token counts and a USD cost priced from the shared cost table.
    coordinator.set_run_state_usage(
        LoopModelUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        LoopModelRouteSnapshot::new("openai", "gpt-4o", "config:v1", "auth:v1"),
    );
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    setup_owned_thread(&services, caller(), "thread-alpha").await;

    let response = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: "thread-alpha".to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect("get_run_state succeeds");

    let usage = response.usage.expect("token usage surfaced");
    assert_eq!(usage.input_tokens, 1_000);
    assert_eq!(usage.output_tokens, 500);

    let cost = response
        .cost
        .as_ref()
        .expect("cost priced from resolved model");
    // gpt-4o: input 0.0000025/tok → 1000 * 0.0000025 = 0.0025; output 0.00001/tok
    // → 500 * 0.00001 = 0.005; total 0.0075.
    assert_eq!(cost.input_cost_usd, "0.0025");
    assert_eq!(cost.output_cost_usd, "0.005");
    assert_eq!(cost.total_cost_usd, "0.0075");
    assert_eq!(cost.currency, "USD");

    // The resolved route stays internal even though its model id fed pricing.
    let rendered = serde_json::to_string(&response).expect("json");
    assert!(!rendered.contains("resolved_model_route"));
    assert!(rendered.contains("\"usage\""));
    assert!(rendered.contains("\"cost\""));
}

/// Stub [`ActiveModelReader`] returning a fixed active/default model id.
struct FixedActiveModelReader(Option<String>);

impl ActiveModelReader for FixedActiveModelReader {
    fn active_model_id(&self) -> Option<String> {
        self.0.clone()
    }
}

#[tokio::test]
async fn get_run_state_prices_default_model_run_against_active_model() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    // A default-model run: usage is reported but no route was resolved (the
    // caller sent no `model`). Pricing falls back to the runtime's live active
    // model so the run is still priced instead of reporting `usage` with no
    // `cost`.
    coordinator.set_run_state_usage_default_model(LoopModelUsage {
        input_tokens: 1_000,
        output_tokens: 500,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    });
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_active_model_reader(Arc::new(FixedActiveModelReader(Some("gpt-4o".to_string()))));
    setup_owned_thread(&services, caller(), "thread-alpha").await;

    let response = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: "thread-alpha".to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect("get_run_state succeeds");

    let usage = response.usage.expect("token usage surfaced");
    assert_eq!(usage.input_tokens, 1_000);
    assert_eq!(usage.output_tokens, 500);

    // gpt-4o pricing, identical to the explicit-route case: input 1000 *
    // 0.0000025 = 0.0025; output 500 * 0.00001 = 0.005; total 0.0075.
    let cost = response
        .cost
        .as_ref()
        .expect("default-model run priced against the active model");
    assert_eq!(cost.input_cost_usd, "0.0025");
    assert_eq!(cost.output_cost_usd, "0.005");
    assert_eq!(cost.total_cost_usd, "0.0075");

    // The active model was only used to price; it is not leaked as a route.
    let rendered = serde_json::to_string(&response).expect("json");
    assert!(!rendered.contains("resolved_model_route"));
    assert!(!rendered.contains("gpt-4o"));
}

#[tokio::test]
async fn get_run_state_default_model_run_omits_cost_without_active_model() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    coordinator.set_run_state_usage_default_model(LoopModelUsage {
        input_tokens: 1_000,
        output_tokens: 500,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    });
    // No active-model reader wired (and a reader that reports no concrete model
    // behaves the same): the run reports token usage but omits cost rather than
    // mispricing against a sentinel.
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    )
    .with_active_model_reader(Arc::new(FixedActiveModelReader(None)));
    setup_owned_thread(&services, caller(), "thread-alpha").await;

    let response = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: "thread-alpha".to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect("get_run_state succeeds");

    assert!(response.usage.is_some(), "token usage still surfaced");
    assert!(
        response.cost.is_none(),
        "cost omitted when no concrete model is available"
    );
    let rendered = serde_json::to_string(&response).expect("json");
    assert!(rendered.contains("\"usage\""));
    assert!(!rendered.contains("\"cost\""));
}

#[tokio::test]
async fn get_run_state_rejects_invalid_thread_id() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );

    let err = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: String::new(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect_err("blank thread_id must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("thread_id"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::InvalidId)
    );
    // Errors must be sanitized — no internal type names leak through.
    let rendered = serde_json::to_string(&err).expect("json");
    assert!(!rendered.contains("TurnCoordinator"));
    assert!(!rendered.contains("HostRuntime"));
    assert_eq!(coordinator.run_state_request_count(), 0);
}

#[tokio::test]
async fn get_run_state_rejects_non_uuid_run_id() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );

    let err = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: "thread-alpha".to_string(),
                run_id: "not-a-uuid".to_string(),
            },
        )
        .await
        .expect_err("non-uuid run_id must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("run_id"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::InvalidId)
    );
    assert_eq!(coordinator.run_state_request_count(), 0);
}

#[tokio::test]
async fn get_run_state_maps_scope_not_found_to_not_found() {
    let coordinator = Arc::new(FakeTurnCoordinator::with_run_state_error(
        TurnError::ScopeNotFound,
    ));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    setup_owned_thread(&services, caller(), "thread-alpha").await;

    let err = services
        .get_run_state(
            caller(),
            RebornGetRunStateRequest {
                thread_id: "thread-alpha".to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect_err("missing run must surface NotFound");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert!(!err.retryable);
}

// Regression: get_run_state must reject when the authenticated user does not
// own the thread. TurnScope only carries (tenant, agent, project, thread_id),
// so without this check any caller sharing an agent scope could read another
// user's run state by guessing thread_id and run_id.
#[tokio::test]
async fn get_run_state_rejects_cross_user_access() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
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
        .get_run_state(
            bob,
            RebornGetRunStateRequest {
                thread_id: "thread-alice".to_string(),
                run_id: run_id_string(),
            },
        )
        .await
        .expect_err("cross-user run-state read must be rejected");

    // 404 rather than 403 so the existence of Alice's thread is not leaked.
    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(
        coordinator.run_state_request_count(),
        0,
        "turn coordinator must NOT be called for cross-user run-state read"
    );
}
