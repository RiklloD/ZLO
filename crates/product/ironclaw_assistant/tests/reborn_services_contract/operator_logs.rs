//! Operator and thread-scoped log views, and the operator service lifecycle port.

use super::*;

struct CrateRootLifecycleBackend;

async fn query_logs_view(
    services: &RebornServices,
    caller: ProductSurfaceCaller,
    mut request: RebornLogQueryRequest,
) -> Result<RebornLogQueryResponse, ProductSurfaceError> {
    let cursor = request.cursor.take();
    let page = services
        .query(
            caller,
            RebornViewQuery {
                view_id: LOGS_VIEW.id.to_string(),
                params: serde_json::to_value(request)
                    .map_err(ProductSurfaceError::internal_from)?,
                cursor,
            },
        )
        .await?;
    serde_json::from_value(page.payload).map_err(ProductSurfaceError::internal_from)
}

async fn query_operator_logs_view(
    services: &RebornServices,
    caller: ProductSurfaceCaller,
    mut request: RebornOperatorLogsQuery,
) -> Result<RebornOperatorCommandPlaneResponse, ProductSurfaceError> {
    let cursor = request.cursor.take();
    let page = services
        .query(
            caller,
            RebornViewQuery {
                view_id: OPERATOR_LOGS_VIEW.id.to_string(),
                params: serde_json::to_value(request)
                    .map_err(ProductSurfaceError::internal_from)?,
                cursor,
            },
        )
        .await?;
    serde_json::from_value(page.payload).map_err(ProductSurfaceError::internal_from)
}

#[async_trait]
impl OperatorServiceLifecycleService for CrateRootLifecycleBackend {
    async fn control_service(
        &self,
        _caller: ProductSurfaceCaller,
        request: RebornServiceLifecycleRequest,
    ) -> Result<RebornServiceLifecycleResponse, ProductSurfaceError> {
        Ok(RebornServiceLifecycleResponse {
            action: request.action,
            state: RebornServiceLifecycleState::Unsupported,
            message: "not wired".to_string(),
            remediation: None,
        })
    }
}

#[tokio::test]
async fn query_operator_logs_bounds_query_before_logs_service() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    let oversized_cursor = format!("  {}  ", "c".repeat(2048));
    let oversized_target = format!("{}é", "t".repeat(512));
    let oversized_thread_id = format!("{}é", "thread-".repeat(80));
    let oversized_run_id = format!("{}é", "run-".repeat(100));
    let boundary_source = format!("{}é", "s".repeat(254));
    let response = query_operator_logs_view(
        &services,
        caller().with_operator_config(true),
        RebornOperatorLogsQuery {
            limit: Some(u32::MAX),
            cursor: Some(oversized_cursor),
            level: Some(RebornLogLevel::Warn),
            target: Some(oversized_target),
            thread_id: Some(oversized_thread_id),
            run_id: Some(oversized_run_id),
            turn_id: Some("turn-1".to_string()),
            tool_call_id: Some("tool-call-1".to_string()),
            tool_name: Some("shell".to_string()),
            source: Some(boundary_source),
            tail: true,
            follow: false,
        },
    )
    .await
    .expect("operator logs query");

    assert_eq!(response.status, RebornOperatorSurfaceStatus::Available);
    let requests = operator_logs.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].limit, Some(500));
    assert_eq!(requests[0].cursor.as_ref().map(String::len), Some(512));
    assert_eq!(requests[0].target.as_ref().map(String::len), Some(256));
    assert_eq!(requests[0].thread_id.as_ref().map(String::len), Some(256));
    assert_eq!(requests[0].run_id.as_ref().map(String::len), Some(256));
    assert_eq!(requests[0].turn_id.as_deref(), Some("turn-1"));
    assert_eq!(requests[0].tool_call_id.as_deref(), Some("tool-call-1"));
    assert_eq!(requests[0].tool_name.as_deref(), Some("shell"));
    let source = requests[0].source.as_deref().expect("bounded source");
    assert_eq!(source.len(), 256);
    assert!(source.ends_with('é'));
    assert!(source.is_char_boundary(source.len()));
    let run_id = requests[0].run_id.as_deref().expect("bounded run id");
    assert_eq!(run_id.len(), 256);
    assert!(run_id.ends_with(" ... [truncated]"));
    assert!(run_id.is_char_boundary(run_id.len()));
    assert_eq!(requests[0].level, Some(RebornLogLevel::Warn));
    assert!(requests[0].tail);
    assert!(!requests[0].follow);
}

#[tokio::test]
async fn query_operator_logs_forwards_follow_mode_to_logs_service() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    query_operator_logs_view(
        &services,
        caller().with_operator_config(true),
        RebornOperatorLogsQuery {
            limit: Some(25),
            cursor: Some("after:7".to_string()),
            level: Some(RebornLogLevel::Info),
            target: Some("ironclaw".to_string()),
            thread_id: None,
            run_id: None,
            turn_id: None,
            tool_call_id: None,
            tool_name: None,
            source: None,
            tail: false,
            follow: true,
        },
    )
    .await
    .expect("operator logs follow query");

    let requests = operator_logs.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].limit, Some(25));
    assert_eq!(requests[0].cursor.as_deref(), Some("after:7"));
    assert_eq!(requests[0].level, Some(RebornLogLevel::Info));
    assert_eq!(requests[0].target.as_deref(), Some("ironclaw"));
    assert!(!requests[0].tail);
    assert!(requests[0].follow);
}

#[tokio::test]
async fn query_operator_logs_rejects_ambiguous_tail_follow_modes() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    let err = query_operator_logs_view(
        &services,
        caller().with_operator_config(true),
        RebornOperatorLogsQuery {
            limit: None,
            cursor: None,
            level: None,
            target: None,
            thread_id: None,
            run_id: None,
            turn_id: None,
            tool_call_id: None,
            tool_name: None,
            source: None,
            tail: true,
            follow: true,
        },
    )
    .await
    .expect_err("tail and follow cannot be combined");

    assert_eq!(err.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("follow"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::InvalidValue)
    );
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn query_logs_requires_thread_scope() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    let err = query_logs_view(&services, caller(), RebornLogQueryRequest::default())
        .await
        .expect_err("public logs require a thread scope");

    assert_eq!(err.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("thread_id"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::MissingField)
    );
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn query_logs_rejects_ambiguous_tail_follow_modes() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    let err = query_logs_view(
        &services,
        caller(),
        RebornLogQueryRequest::default()
            .set_thread_id("thread-alpha")
            .set_tail(true)
            .set_follow(true),
    )
    .await
    .expect_err("tail and follow cannot be combined");

    assert_eq!(err.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some("follow"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::InvalidValue)
    );
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn query_logs_forwards_owned_thread_scope_to_logs_service() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    setup_owned_thread(&services, caller(), "thread-alpha").await;

    query_logs_view(
        &services,
        caller(),
        RebornLogQueryRequest::default()
            .set_limit(25)
            .set_cursor("after:7")
            .set_level(RebornLogLevel::Info)
            .set_target("ironclaw")
            .set_thread_id("thread-alpha")
            .set_follow(true),
    )
    .await
    .expect("owned thread logs query");

    let requests = operator_logs.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].limit, Some(25));
    assert_eq!(requests[0].cursor.as_deref(), Some("after:7"));
    assert_eq!(requests[0].level, Some(RebornLogLevel::Info));
    assert_eq!(requests[0].target.as_deref(), Some("ironclaw"));
    assert_eq!(requests[0].thread_id.as_deref(), Some("thread-alpha"));
    assert!(!requests[0].tail);
    assert!(requests[0].follow);
}

#[tokio::test]
async fn query_logs_rejects_thread_owned_by_another_caller() {
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(operator_logs.clone());

    setup_owned_thread(&services, caller_for_user("user-bob"), "thread-bob").await;

    let err = query_logs_view(
        &services,
        caller(),
        RebornLogQueryRequest::default()
            .set_limit(25)
            .set_thread_id("thread-bob"),
    )
    .await
    .expect_err("foreign thread logs are not caller-visible");

    assert_eq!(err.status_code, 404);
    assert_eq!(err.kind, ProductSurfaceErrorKind::NotFound);
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn operator_service_lifecycle_contract_is_implementable_from_crate_root() {
    let backend = CrateRootLifecycleBackend;
    let response = backend
        .control_service(
            caller(),
            RebornServiceLifecycleRequest {
                action: RebornServiceLifecycleAction::Status,
            },
        )
        .await
        .expect("crate-root lifecycle service implementation");

    assert_eq!(response.action, RebornServiceLifecycleAction::Status);
    assert_eq!(response.state, RebornServiceLifecycleState::Unsupported);
}
