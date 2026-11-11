//! Run and thread artifact exports.

use super::*;

#[tokio::test]
async fn run_artifact_selects_one_owned_run_and_queries_only_its_scoped_logs() {
    let owner = caller();
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-artifact").expect("thread id");
    let run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    let other_run_id = TurnRunId::new();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    seed_submitted_message(
        &thread_service,
        &thread_scope,
        &thread_id,
        &run_id,
        "selected run",
    )
    .await;
    seed_submitted_message(
        &thread_service,
        &thread_scope,
        &thread_id,
        &other_run_id,
        "other run",
    )
    .await;
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(operator_logs.clone());

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: RUN_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornRunArtifactRequest {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("owned run artifact");
    let artifact: RebornRunArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert_eq!(artifact.messages.len(), 1);
    assert_eq!(artifact.messages[0].content, "selected run");
    assert!(!artifact.logs.complete);
    let requests = operator_logs.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].thread_id.as_deref(), Some("thread-artifact"));
    assert_eq!(
        requests[0].run_id.as_deref(),
        Some(run_id.to_string().as_str())
    );
    assert_eq!(requests[0].limit, Some(500));

    // The diagnostic store was never populated for this run: the export must
    // still succeed, say so honestly, and keep the durable timestamp floor.
    assert!(!artifact.timings.available);
    assert_eq!(
        artifact.timings.unavailable_reason.as_deref(),
        Some("run_not_resident")
    );
    assert!(artifact.timings.iterations.is_empty());
    assert!(
        artifact
            .messages
            .iter()
            .any(|message| message.created_at.is_some()),
        "durable message timestamps must survive an absent diagnostic store"
    );
    assert_eq!(artifact.schema, RUN_ARTIFACT_SCHEMA);
}

/// A diagnostic store whose every method fails, standing in for a backend
/// outage. The single guarantee under test: a user filing a bug report must
/// always get a file, so a diagnostic-store failure must never fail the
/// artifact export.
#[derive(Default)]
struct FailingDiagnosticStore;

impl DiagnosticStorePort for FailingDiagnosticStore {
    fn record_activity(
        &self,
        _scope: DiagnosticScope,
        _event: DiagnosticActivityEvent,
    ) -> Result<DiagnosticCursor, DiagnosticStoreError> {
        Err(DiagnosticStoreError::StateUnavailable)
    }

    fn snapshot(
        &self,
        _scope: &DiagnosticScope,
    ) -> Result<Option<DiagnosticSnapshot>, DiagnosticStoreError> {
        Err(DiagnosticStoreError::StateUnavailable)
    }

    fn prompt(
        &self,
        _scope: &DiagnosticScope,
    ) -> Result<Option<PromptDiagnostic>, DiagnosticStoreError> {
        Err(DiagnosticStoreError::StateUnavailable)
    }

    fn tool_execution(
        &self,
        _scope: &DiagnosticScope,
        _activity_id: CapabilityActivityId,
    ) -> Result<Option<ToolExecutionDiagnostic>, DiagnosticStoreError> {
        Err(DiagnosticStoreError::StateUnavailable)
    }

    fn updates_after(
        &self,
        _scope: &DiagnosticScope,
        _after: Option<DiagnosticCursor>,
    ) -> Result<DiagnosticUpdateBatch, DiagnosticStoreError> {
        Err(DiagnosticStoreError::StateUnavailable)
    }
}

#[tokio::test]
async fn run_artifact_reports_diagnostic_store_failure_without_failing_the_export() {
    let owner = caller();
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-artifact-store-failure").expect("thread id");
    let run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    seed_submitted_message(
        &thread_service,
        &thread_scope,
        &thread_id,
        &run_id,
        "diagnostic store is down",
    )
    .await;
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_diagnostic_store(Arc::new(FailingDiagnosticStore));

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: RUN_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornRunArtifactRequest {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("artifact export must succeed even when the diagnostic store errors");
    let artifact: RebornRunArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert!(!artifact.timings.available);
    assert_eq!(
        artifact.timings.unavailable_reason.as_deref(),
        Some("diagnostic_store_unavailable")
    );
}

#[tokio::test]
async fn run_artifact_rejects_another_user_before_querying_logs() {
    let owner = caller_for_user("user-bob");
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-bob-artifact").expect("thread id");
    let run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    seed_submitted_message(
        &thread_service,
        &thread_scope,
        &thread_id,
        &run_id,
        "private trajectory",
    )
    .await;
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(operator_logs.clone());

    let error = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: RUN_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornRunArtifactRequest {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect_err("foreign run must not be exported");

    assert_eq!(error.status_code, 404);
    assert_eq!(error.kind, ProductSurfaceErrorKind::NotFound);
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn thread_artifact_includes_all_owned_runs_and_queries_thread_scoped_logs() {
    let owner = caller();
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-full-artifact").expect("thread id");
    let first_run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    let second_run_id = TurnRunId::new();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    seed_submitted_message(
        &thread_service,
        &thread_scope,
        &thread_id,
        &first_run_id,
        "first run",
    )
    .await;
    seed_submitted_message(
        &thread_service,
        &thread_scope,
        &thread_id,
        &second_run_id,
        "second run",
    )
    .await;
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(operator_logs.clone());

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: thread_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("owned thread artifact");
    let artifact: RebornThreadArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert_eq!(artifact.messages.len(), 2);
    assert_eq!(artifact.messages[0].content, "first run");
    assert_eq!(
        artifact.messages[0].run_id.as_deref(),
        Some(first_run_id.to_string().as_str())
    );
    assert_eq!(artifact.messages[1].content, "second run");
    assert_eq!(
        artifact.messages[1].run_id.as_deref(),
        Some(second_run_id.to_string().as_str())
    );
    let requests = operator_logs.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].thread_id.as_deref(),
        Some("thread-full-artifact")
    );
    assert_eq!(requests[0].run_id, None);
    assert_eq!(requests[0].limit, Some(500));
}

fn diagnostic_model_call(status: InspectorModelCallStatus) -> ModelCallDiagnostic {
    ModelCallDiagnostic {
        call_id: DiagnosticModelCallId::new(),
        iteration: 1,
        requested_model: BoundedDiagnosticText::label("test-model"),
        effective_model: None,
        started_at: Utc::now(),
        completed_at: Some(Utc::now()),
        duration_ms: Some(1),
        status,
        usage: None,
        failure_summary: None,
    }
}

/// A thread with two runs must expose one timing entry per run. The exact
/// per-run wall-clock projection is pinned with deterministic timestamps in
/// the `thread_artifact` unit test; this route test proves both entries survive
/// the caller-owned export path.
#[tokio::test]
async fn thread_artifact_per_run_timings_do_not_reach_into_another_runs_activity() {
    let owner = caller();
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-multi-run-timings").expect("thread id");
    let run_a = TurnRunId::parse(&run_id_string()).expect("run id");
    let run_b = TurnRunId::new();
    let run_c = TurnRunId::new();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");

    seed_submitted_message(&thread_service, &thread_scope, &thread_id, &run_a, "run a").await;
    seed_submitted_message(&thread_service, &thread_scope, &thread_id, &run_b, "run b").await;
    seed_submitted_message(&thread_service, &thread_scope, &thread_id, &run_c, "run c").await;

    let diagnostic_store =
        InMemoryDiagnosticStore::new(DiagnosticStoreLimits::default()).expect("diagnostic store");
    diagnostic_store
        .record_model_call(
            DiagnosticScope::new(
                owner.tenant_id.clone(),
                owner.user_id.clone(),
                thread_id.clone(),
                run_a,
            ),
            diagnostic_model_call(InspectorModelCallStatus::Succeeded),
        )
        .expect("run a model call recorded");
    diagnostic_store
        .record_model_call(
            DiagnosticScope::new(
                owner.tenant_id.clone(),
                owner.user_id.clone(),
                thread_id.clone(),
                run_b,
            ),
            diagnostic_model_call(InspectorModelCallStatus::Succeeded),
        )
        .expect("run b model call recorded");

    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_diagnostic_store(Arc::new(diagnostic_store));

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: thread_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("thread artifact");
    let artifact: RebornThreadArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert_eq!(artifact.timings_by_run.len(), 3);
    let run_a_timing = artifact
        .timings_by_run
        .iter()
        .find(|entry| entry.run_id == run_a.to_string())
        .expect("run a timing entry");
    assert!(run_a_timing.timings.available);
    assert!(run_a_timing.timings.totals.wall_clock_ms.is_some());
    let run_b_timing = artifact
        .timings_by_run
        .iter()
        .find(|entry| entry.run_id == run_b.to_string())
        .expect("run b timing entry");
    assert!(run_b_timing.timings.available);
    let run_c_timing = artifact
        .timings_by_run
        .iter()
        .find(|entry| entry.run_id == run_c.to_string())
        .expect("run c timing entry");
    assert!(!run_c_timing.timings.available);
    assert_eq!(
        run_c_timing.timings.unavailable_reason.as_deref(),
        Some("run_not_resident")
    );
}

#[tokio::test]
async fn thread_artifact_projects_messages_from_the_bounded_snapshot() {
    let owner = caller();
    let history = fake_thread_history(&owner, "thread-bounded-artifact");
    let message = history.messages[0].clone();
    let snapshot = BoundedThreadMessageSnapshot {
        history: ThreadMessageRange {
            thread: history.thread.clone(),
            messages: vec![message.clone()],
        },
        context: ContextMessages {
            thread_id: history.thread.thread_id.clone(),
            messages: vec![ContextMessage {
                message_id: Some(message.message_id),
                summary_id: None,
                sequence: message.sequence,
                kind: message.kind,
                tool_result_provider_call: None,
                content: "content from bounded snapshot".to_string(),
                image_attachments: Vec::new(),
            }],
        },
    };
    let thread_service = Arc::new(ScriptedThreadService::thread_artifact(history, snapshot));
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_logs_service(Arc::new(RecordingOperatorLogsService::default()));

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: "thread-bounded-artifact".to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("thread artifact");
    let artifact: RebornThreadArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert_eq!(artifact.messages.len(), 1);
    assert_eq!(
        artifact.messages[0].content,
        "content from bounded snapshot"
    );
    let bounded_requests = thread_service.bounded_requests();
    assert_eq!(bounded_requests.len(), 1);
    assert_eq!(
        bounded_requests[0].thread_id.as_str(),
        "thread-bounded-artifact"
    );
    assert_eq!(
        bounded_requests[0].max_messages,
        THREAD_ARTIFACT_MAX_MESSAGES
    );
    assert_eq!(bounded_requests[0].max_bytes, 64 * 1024 * 1024);
}

#[tokio::test]
async fn thread_artifact_accepts_more_than_one_thousand_small_messages() {
    let owner = caller();
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-artifact-many-messages").expect("thread id");
    let run_id = TurnRunId::new();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    for sequence in 0..=1_000 {
        seed_submitted_message(
            &thread_service,
            &thread_scope,
            &thread_id,
            &run_id,
            &format!("message {sequence}"),
        )
        .await;
    }
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(Arc::new(RecordingOperatorLogsService::default()));

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: thread_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("a small trajectory must not fail only because it has many messages");
    let artifact: RebornThreadArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert_eq!(artifact.messages.len(), 1_001);
}

#[tokio::test]
async fn thread_artifact_accepts_tool_heavy_trajectory_over_old_byte_limits() {
    let owner = caller();
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-artifact-tool-heavy").expect("thread id");
    let run_id = TurnRunId::new();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    let tool_payload = "x".repeat(44 * 1024);
    for sequence in 0..573 {
        seed_submitted_message(
            &thread_service,
            &thread_scope,
            &thread_id,
            &run_id,
            &format!("tool result {sequence}: {tool_payload}"),
        )
        .await;
    }
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(Arc::new(RecordingOperatorLogsService::default()));

    let page = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: thread_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect("a tool-heavy trajectory within the raised byte limits must export");
    let artifact: RebornThreadArtifact =
        serde_json::from_value(page.payload).expect("artifact payload");

    assert_eq!(artifact.messages.len(), 573);
}

#[tokio::test]
async fn thread_artifact_still_maps_bounded_read_limit_to_413() {
    let owner = caller();
    let history = fake_thread_history(&owner, "thread-artifact-over-byte-budget");
    let snapshot = BoundedThreadMessageSnapshot {
        history: ThreadMessageRange {
            thread: history.thread.clone(),
            messages: Vec::new(),
        },
        context: ContextMessages {
            thread_id: history.thread.thread_id.clone(),
            messages: Vec::new(),
        },
    };
    let thread_service = Arc::new(ScriptedThreadService::thread_artifact_limit_exceeded(
        history, snapshot,
    ));
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(operator_logs.clone());

    let error = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: "thread-artifact-over-byte-budget".to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect_err("a snapshot over the unchanged bounded-read budgets must fail closed");

    assert_eq!(error.status_code, 413);
    assert_eq!(error.kind, ProductSurfaceErrorKind::Validation);
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn thread_artifact_reports_bounded_backend_failure_as_unavailable() {
    let owner = caller();
    let history = fake_thread_history(&owner, "thread-artifact-backend-error");
    let snapshot = BoundedThreadMessageSnapshot {
        history: ThreadMessageRange {
            thread: history.thread.clone(),
            messages: Vec::new(),
        },
        context: ContextMessages {
            thread_id: history.thread.thread_id.clone(),
            messages: Vec::new(),
        },
    };
    let thread_service = Arc::new(ScriptedThreadService::thread_artifact_backend_error(
        history, snapshot,
    ));
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(operator_logs.clone());

    let error = services
        .query(
            owner,
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: "thread-artifact-backend-error".to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect_err("backend capability failure must not look like an oversized thread");

    assert_eq!(error.status_code, 503);
    assert_eq!(error.kind, ProductSurfaceErrorKind::TimelineUnavailable);
    assert!(error.retryable);
    assert!(operator_logs.requests().is_empty());
}

#[tokio::test]
async fn thread_artifact_rejects_another_user_before_querying_logs() {
    let owner = caller_for_user("user-bob");
    let thread_scope = thread_scope_for(&owner);
    let thread_id = ThreadId::new("thread-bob-full-artifact").expect("thread id");
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope,
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .expect("thread");
    let operator_logs = Arc::new(RecordingOperatorLogsService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_operator_logs_service(operator_logs.clone());

    let error = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: THREAD_ARTIFACT_VIEW.id.to_string(),
                params: serde_json::to_value(RebornThreadArtifactRequest {
                    thread_id: thread_id.to_string(),
                })
                .expect("artifact params"),
                cursor: None,
            },
        )
        .await
        .expect_err("foreign thread must not be exported");

    assert_eq!(error.status_code, 404);
    assert_eq!(error.kind, ProductSurfaceErrorKind::NotFound);
    assert!(operator_logs.requests().is_empty());
}
