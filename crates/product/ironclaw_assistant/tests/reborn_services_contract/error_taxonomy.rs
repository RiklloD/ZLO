//! Turn, adapter, projection, and surface error mapping onto the service taxonomy.

use super::*;

struct AuthFailureProjectionStream;

#[async_trait]
impl ProjectionStream for AuthFailureProjectionStream {
    async fn drain(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError> {
        Err(ProductAdapterError::Authentication(
            ProtocolAuthFailure::SignatureMismatch,
        ))
    }
}

struct StaticErrorProjectionStream {
    error: ProductAdapterError,
}

#[async_trait]
impl ProjectionStream for StaticErrorProjectionStream {
    async fn drain(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError> {
        Err(self.error.clone())
    }
}

#[tokio::test]
async fn validation_errors_are_stable_and_sanitized() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-1",
                "thread_id": "thread-alpha"
            }))
            .expect("request"),
        )
        .await
        .expect_err("missing content rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("content"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::MissingField)
    );
    let rendered = serde_json::to_string(&err).expect("json");
    assert!(!rendered.contains("backend"));
    assert!(!rendered.contains("TurnCoordinator"));
}

#[tokio::test]
async fn turn_admission_rejected_maps_to_busy_taxonomy() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::with_submit_error(
            TurnError::AdmissionRejected(AdmissionRejection::new(
                AdmissionRejectionReason::TenantLimit,
            )),
        )),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-rate-limited",
                "thread_id": "thread-alpha",
                "content": "hello from webui"
            }))
            .expect("request"),
        )
        .await
        .expect_err("admission rejection is a stable busy/rate-limited error");

    assert_eq!(err.code, ProductSurfaceErrorCode::RateLimited);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Busy);
    assert_eq!(err.status_code, 429);
    assert!(err.retryable);
}

#[tokio::test]
async fn turn_unauthorized_maps_to_forbidden() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::with_submit_error(
            TurnError::Unauthorized,
        )),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-forbidden",
                "thread_id": "thread-alpha",
                "content": "hello from webui"
            }))
            .expect("request"),
        )
        .await
        .expect_err("turn unauthorized is forbidden");

    assert_eq!(err.code, ProductSurfaceErrorCode::Forbidden);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ParticipantDenied);
    assert_eq!(err.status_code, 403);
}

#[tokio::test]
async fn turn_error_categories_map_to_service_taxonomy() {
    let cases = [
        (
            "conflict",
            TurnError::Conflict {
                reason: "active run changed".to_string(),
            },
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::Conflict,
            409,
            false,
        ),
        (
            "scope-not-found",
            TurnError::ScopeNotFound,
            ProductSurfaceErrorCode::NotFound,
            ProductSurfaceErrorKind::NotFound,
            404,
            false,
        ),
        (
            "invalid-request",
            TurnError::InvalidRequest {
                reason: "invalid run profile".to_string(),
            },
            ProductSurfaceErrorCode::InvalidRequest,
            ProductSurfaceErrorKind::Validation,
            400,
            false,
        ),
        (
            "unavailable",
            TurnError::Unavailable {
                reason: "turn store unavailable".to_string(),
            },
            ProductSurfaceErrorCode::Unavailable,
            ProductSurfaceErrorKind::ServiceUnavailable,
            503,
            true,
        ),
    ];

    for (name, turn_error, expected_code, expected_kind, expected_status, expected_retryable) in
        cases
    {
        let services = session_services(
            Arc::new(InMemorySessionThreadService::default()),
            Arc::new(FakeTurnCoordinator::with_submit_error(turn_error)),
        );
        let thread_id = format!("thread-{name}");
        create_thread_for(&services, caller(), &thread_id).await;

        let err = services
            .submit_turn(
                caller(),
                session_submit_request(json!({
                    "client_action_id": format!("send-{name}"),
                    "thread_id": thread_id,
                    "content": "hello from webui"
                }))
                .expect("request"),
            )
            .await
            .expect_err("turn error maps to stable service taxonomy");

        assert_eq!(err.code, expected_code, "{name}");
        assert_eq!(err.kind, expected_kind, "{name}");
        assert_eq!(err.status_code, expected_status, "{name}");
        assert_eq!(err.retryable, expected_retryable, "{name}");
    }
}

#[tokio::test]
async fn stream_events_without_projection_stream_maps_to_replay_unavailable_taxonomy() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .stream_events(
            caller(),
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("missing projection stream is replay unavailable");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ReplayUnavailable);
    assert_eq!(err.status_code, 503);
    assert!(!err.retryable);
}

#[tokio::test]
async fn adapter_authentication_maps_to_unauthenticated() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_event_stream(Arc::new(AuthFailureProjectionStream));
    // stream_events now ownership-probes the caller before draining; seed the
    // thread under the caller so the probe passes and the adapter auth error
    // is what the test observes.
    setup_owned_thread(&services, caller(), "thread-alpha").await;

    let err = services
        .stream_events(
            caller(),
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("adapter auth failure is unauthenticated");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unauthenticated);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ParticipantDenied);
    assert_eq!(err.status_code, 401);
}

#[tokio::test]
async fn projection_transient_maps_to_replay_unavailable_taxonomy() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_event_stream(Arc::new(StaticErrorProjectionStream {
        error: ProductAdapterError::SurfaceTransient {
            reason: RedactedString::new("provider stack trace with /host/path and secret-token"),
        },
    }));
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .stream_events(
            caller(),
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("projection transient is replay unavailable");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ReplayUnavailable);
    assert_eq!(err.status_code, 503);
    assert!(err.retryable);
    let rendered = format!("{err:?} {}", serde_json::to_string(&err).expect("json"));
    assert!(!rendered.contains("secret-token"));
    assert!(!rendered.contains("/host/path"));
    assert!(!rendered.contains("provider stack trace"));
}

#[tokio::test]
async fn projection_egress_denied_maps_to_blocked_resource_taxonomy() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_event_stream(Arc::new(StaticErrorProjectionStream {
        error: ProductAdapterError::EgressDenied {
            reason: RedactedString::new("denied api key secret-token"),
        },
    }));
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .stream_events(
            caller(),
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("blocked resource is stable taxonomy");

    assert_eq!(err.code, ProductSurfaceErrorCode::Forbidden);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedResource);
    assert_eq!(err.status_code, 403);
    let rendered = format!("{err:?} {}", serde_json::to_string(&err).expect("json"));
    assert!(!rendered.contains("secret-token"));
}

#[tokio::test]
async fn surface_rejection_kinds_map_to_service_taxonomy() {
    let cases = [
        (
            ProductSurfaceRejectionKind::ThreadBusy,
            409,
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::Busy,
        ),
        (
            ProductSurfaceRejectionKind::AdmissionRejected,
            429,
            ProductSurfaceErrorCode::RateLimited,
            ProductSurfaceErrorKind::Busy,
        ),
        (
            ProductSurfaceRejectionKind::ScopeNotFound,
            404,
            ProductSurfaceErrorCode::NotFound,
            ProductSurfaceErrorKind::NotFound,
        ),
        (
            ProductSurfaceRejectionKind::Unauthorized,
            403,
            ProductSurfaceErrorCode::Forbidden,
            ProductSurfaceErrorKind::ParticipantDenied,
        ),
        (
            ProductSurfaceRejectionKind::InvalidRequest,
            400,
            ProductSurfaceErrorCode::InvalidRequest,
            ProductSurfaceErrorKind::Validation,
        ),
        (
            ProductSurfaceRejectionKind::Unavailable,
            503,
            ProductSurfaceErrorCode::Unavailable,
            ProductSurfaceErrorKind::ReplayUnavailable,
        ),
        (
            ProductSurfaceRejectionKind::Conflict,
            409,
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorKind::Conflict,
        ),
    ];

    for (workflow_kind, status_code, expected_code, expected_kind) in cases {
        let services = session_services(
            Arc::new(InMemorySessionThreadService::default()),
            Arc::new(FakeTurnCoordinator::default()),
        )
        .with_event_stream(Arc::new(StaticErrorProjectionStream {
            error: ProductAdapterError::SurfaceRejected {
                kind: workflow_kind,
                status_code,
                retryable: false,
                reason: RedactedString::new("internal workflow detail secret-token"),
            },
        }));
        create_thread_for(&services, caller(), "thread-alpha").await;

        let err = services
            .stream_events(
                caller(),
                RebornStreamEventsRequest {
                    thread_id: "thread-alpha".to_string(),
                    after_cursor: None,
                },
            )
            .await
            .expect_err("workflow rejection maps to stable service taxonomy");

        assert_eq!(err.code, expected_code);
        assert_eq!(err.kind, expected_kind);
        assert_eq!(err.status_code, status_code);
        assert!(
            !serde_json::to_string(&err)
                .expect("json")
                .contains("secret-token")
        );
    }
}

#[tokio::test]
async fn timeline_backend_failure_maps_to_timeline_unavailable_taxonomy() {
    let services = session_services(
        Arc::new(ScriptedThreadService::backend_history()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let err = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect_err("timeline backend failure is stable unavailable taxonomy");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::TimelineUnavailable);
    assert_eq!(err.status_code, 503);
    assert!(err.retryable);
    let rendered = format!("{err:?} {}", serde_json::to_string(&err).expect("json"));
    assert!(!rendered.contains("secret-token"));
    assert!(!rendered.contains("/host/path"));
    assert!(!rendered.contains("backend detail"));
}
