//! Scheduler liveness and run-cancellation races: mid-register surface change,
//! deferred busy messages, and stopped-worker rejection.

use super::*;

/// Multi-call model response with a mid-register surface change must not kill the run.
///
/// Scenario: the scripted gateway (a) registers tool call #1, (b) activates an extension
/// (deterministic surface-content change), (c) registers tool call #2, then returns both
/// candidates together.  Before the fix, register #2 rebuilt the inner port, wiping the
/// snapshot that candidate #1 referred to; the executor hit StaleSurface on the first
/// candidate and collapsed to a terminal HostUnavailable failure.  After the fix, both
/// candidates carry the same (prompt-stage) surface version and the run completes.
#[tokio::test]
async fn multi_tool_call_response_survives_surface_change_mid_register() {
    use ironclaw_assistant::LifecycleProductAction;
    use ironclaw_product_contracts::lifecycle_service::{
        LifecycleProductContext, LifecycleProductService, LifecycleProductSurfaceContext,
    };
    use std::sync::OnceLock;

    // This scenario performs a complete libSQL-backed runtime bootstrap and an
    // extension lifecycle transition while the crate's other runtime tests run
    // in parallel. Keep the timeout large enough to measure the behavior under
    // test rather than host scheduling contention; the generic 10-second poll
    // budget is intentionally tighter for smaller runtime scenarios.
    const SURFACE_CHANGE_TEST_TIMEOUT: Duration = Duration::from_secs(30);

    // Gateway state seeded after runtime build.
    struct LifecycleServiceHandle {
        service: ironclaw_extension_manager::ExtensionHostLifecycleProductService,
    }

    impl std::fmt::Debug for LifecycleServiceHandle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LifecycleServiceHandle").finish()
        }
    }

    struct MultiToolCallGateway {
        calls: StdMutex<usize>,
        service_slot: Arc<OnceLock<LifecycleServiceHandle>>,
    }

    #[async_trait]
    impl HostManagedModelGateway for MultiToolCallGateway {
        async fn stream_model(
            &self,
            _request: HostManagedModelRequest,
        ) -> Result<HostManagedModelResponse, HostManagedModelError> {
            Err(HostManagedModelError::safe(
                HostManagedModelErrorKind::InvalidRequest,
                "expected capability-aware model path",
            ))
        }

        async fn stream_model_with_capabilities(
            &self,
            _request: HostManagedModelRequest,
            capabilities: Arc<dyn ironclaw_loop_contracts::LoopCapabilityPort>,
        ) -> Result<HostManagedModelResponse, HostManagedModelError> {
            let call_index = {
                let mut calls = self.calls.lock().expect("multi-tool gateway lock poisoned");
                let idx = *calls;
                *calls += 1;
                idx
            };

            if call_index > 0 {
                // Second model call: capability results have been fed back — finish the run.
                return Ok(HostManagedModelResponse::assistant_reply(
                    "multi-tool surface-change ok",
                ));
            }

            // ── First model call ──────────────────────────────────────────────────
            // Trigger prompt-stage surface snapshot (establishes V1).
            capabilities
                .visible_capabilities(VisibleCapabilityRequest)
                .await
                .map_err(model_capability_error)?;

            // Find the builtin echo tool.
            let echo_id =
                ironclaw_host_api::ids::CapabilityId::new("builtin.echo").expect("echo id");
            let echo_tool = capabilities
                .tool_definitions()
                .map_err(model_capability_error)?
                .into_iter()
                .find(|def| def.capability_id == echo_id)
                .expect("echo provider tool definition");

            // Register call #1 — candidate carries surface version V1.
            let mut call1 = ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("provider-turn-multi".to_string()),
                id: "call-multi-1".to_string(),
                name: echo_tool.name.clone(),
                arguments: serde_json::json!({"message": "hello from call 1"}),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            };
            let candidate1 = capabilities
                .register_provider_tool_call(RegisterProviderToolCallRequest::new(call1.clone()))
                .await
                .map_err(model_capability_error)?;

            // Activate the github extension — deterministic surface-content change.
            // Pre-fix: this rebuilds the inner port, wiping candidate1's snapshot.
            let service_handle = self
                .service_slot
                .get()
                .expect("lifecycle service must be seeded before send_user_message");
            let package_ref = LifecyclePackageRef::new(LifecyclePackageKind::Extension, "github")
                .expect("valid github ref");
            // #5459 P1: act as the runtime owner (the tenant operator) so
            // the install is tenant-shared and visible to the run's
            // surface user — a non-operator install would now be private.
            let ctx = LifecycleProductContext::Surface(LifecycleProductSurfaceContext {
                tenant_id: TenantId::new("tenant-multi-tool-surface").expect("tenant id"),
                user_id: UserId::new("runtime-multi-tool-surface-owner").expect("user id"),
                agent_id: None,
                project_id: None,
            });
            service_handle
                .service
                .execute(
                    ctx.clone(),
                    LifecycleProductAction::ExtensionInstall {
                        package_ref: package_ref.clone(),
                    },
                )
                .await
                .expect("install github extension");
            service_handle
                .service
                .execute(
                    ctx,
                    LifecycleProductAction::ExtensionActivate { package_ref },
                )
                .await
                .expect("activate github extension");

            // Register call #2 — after surface change.
            // Post-fix: reuses current port, so both candidates carry the same surface version.
            call1.id = "call-multi-2".to_string();
            call1.arguments = serde_json::json!({"message": "hello from call 2"});
            let candidate2 = capabilities
                .register_provider_tool_call(RegisterProviderToolCallRequest::new(call1))
                .await
                .map_err(model_capability_error)?;

            // Both candidates must carry the same surface version after the fix.
            // (We cannot assert this here without breaking the pre-fix path,
            //  so we rely on the run-completion assertion in the test body.)
            Ok(HostManagedModelResponse::capability_calls(
                vec![candidate1, candidate2],
                "",
            ))
        }
    }

    // ── Test body ──────────────────────────────────────────────────────────────
    let root = tempfile::tempdir().expect("tempdir");
    let service_slot: Arc<OnceLock<LifecycleServiceHandle>> = Arc::new(OnceLock::new());
    let gateway = Arc::new(MultiToolCallGateway {
        calls: StdMutex::new(0),
        service_slot: Arc::clone(&service_slot),
    });
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway;

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-multi-tool-surface-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-multi-tool-surface-tenant".to_string(),
        agent_id: "runtime-multi-tool-surface-agent".to_string(),
        source_binding_id: "runtime-multi-tool-surface-source".to_string(),
        reply_target_binding_id: "runtime-multi-tool-surface-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: SURFACE_CHANGE_TEST_TIMEOUT,
    })
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");

    // Seed the lifecycle service before the model gateway runs.
    let extension_management = runtime.extension_management.clone();
    let service = ironclaw_extension_manager::ExtensionHostLifecycleProductService::new(
        Arc::clone(&runtime.skill_management),
    )
    .with_extension_management(extension_management)
    .with_runtime_credential_accounts(Arc::new(MultiToolConfiguredCredentials));
    service_slot
        .set(LifecycleServiceHandle { service })
        .expect("service slot should be empty before seeding");

    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        SURFACE_CHANGE_TEST_TIMEOUT,
        runtime.send_user_message(&conversation, "use echo tool twice"),
    )
    .await
    .expect("runtime send should finish within timeout")
    .expect("runtime send should succeed");

    assert_eq!(
        reply.status,
        TurnStatus::Completed,
        "multi-tool response with mid-register surface change must not produce terminal failure; status={:?} text={:?}",
        reply.status,
        reply.text,
    );
    assert_eq!(reply.text.as_deref(), Some("multi-tool surface-change ok"));

    runtime.shutdown().await.expect("runtime shutdown");
}

/// Regression guard: a message that arrives while the thread is busy is queued
/// as steering input for the active run (`Queued` status, `DeferredBusy`
/// response) and must NOT be auto-resubmitted as a separate run when the
/// blocking run reaches a terminal state.
///
/// Scenario:
///  A – submitted via `turn_coordinator.submit_turn`; worker is stopped so it stays
///      Queued and holds the active-lock.
///  B – submitted via the WebUI path; thread is busy → stored as `Queued`,
///      bound to run A; response is `DeferredBusy` with a non-empty `notice`.
///  Cancel A → B flips `Queued` → `RejectedBusy` (the cancel-time steering
///      reconciler claims the undrained input; resend affordance, never an
///      auto-resubmission as a separate run).
///  C – submitted after A is cancelled; thread is free → `Submitted`.
///
// arch-exempt: large_file, requires `build_reborn_runtime` + full turn-runner control that only this runtime test harness provides — moving it would duplicate the harness, plan #4471
#[tokio::test]
async fn deferred_busy_message_not_auto_submitted_after_run_cancellation() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "busy-drain ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(with_test_authenticated_session_channel(
        crate::deployment::local_filesystem_build_input(
            "runtime-rejected-busy-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    ))
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-rejected-busy-tenant".to_string(),
        agent_id: "runtime-rejected-busy-agent".to_string(),
        source_binding_id: "runtime-rejected-busy-source".to_string(),
        reply_target_binding_id: "runtime-rejected-busy-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    // Stop the worker so run A stays Queued and holds the thread active-lock.
    stop_turn_runner_worker_for_manual_state_test(&runtime).await;

    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-rejected-busy-tenant").unwrap(),
        UserId::new("runtime-rejected-busy-owner").unwrap(),
        Some(AgentId::new("runtime-rejected-busy-agent").unwrap()),
        None,
    );

    // Create the thread via WebUI so the thread record exists.
    let created = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        CREATE_THREAD_COMMAND,
        ProductCreateThreadRequest {
            client_action_id: Some("create-rejected-busy-thread".to_string()),
            requested_thread_id: None,
            project_id: None,
        },
    )
    .await
    .expect("create thread");
    let thread_id = created.thread.thread_id.clone();

    // Submit message A directly so we hold the active-lock (worker is stopped,
    // so the run stays Queued indefinitely).
    let scope = caller.turn_scope(thread_id.clone());
    let actor = caller.actor();
    let submitted_a = runtime
        .turn_coordinator
        .submit_turn(SubmitTurnRequest {
            subagent_activation_provenance: None,
            requested_model: None,
            output_contract: None,
            scope: scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: AcceptedMessageRef::new("msg:rejected-busy-a").unwrap(),
            requested_run_profile: None,
            idempotency_key: IdempotencyKey::new("rejected-busy-a").unwrap(),
            received_at: Utc::now(),
            requested_run_id: None,
            parent_run_id: None,
            subagent_depth: 0,
            spawn_tree_root_run_id: None,
            product_context: None,
        })
        .await
        .expect("message A submitted");
    let SubmitTurnResponse::Accepted {
        run_id: run_id_a, ..
    } = submitted_a;

    // Submit message B through the WebUI path — thread is busy, must get RejectedBusy.
    let response_b = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        SUBMIT_TURN_COMMAND,
        ProductSubmitTurnRequest {
            extension_id: Some(TEST_SESSION_EXTENSION_ID.to_string()),
            client_action_id: Some("send-rejected-busy-b".to_string()),
            thread_id: Some(thread_id.to_string()),
            content: Some("message B while thread is busy".to_string()),
            attachments: Vec::new(),
            model: None,
            mention_roots: Vec::new(),
        },
    )
    .await
    .expect("message B submit should not error");

    let RebornSubmitTurnResponse::DeferredBusy {
        notice: notice_b,
        active_run_id: busy_run_id,
        status: status_b,
        ..
    } = response_b
    else {
        panic!("expected DeferredBusy for message B, got {response_b:?}");
    };
    assert_eq!(
        busy_run_id, run_id_a,
        "DeferredBusy should report run A as the active run"
    );
    assert_eq!(
        status_b,
        TurnStatus::Queued,
        "run A holds the lock in Queued state"
    );
    assert!(
        !notice_b.is_empty(),
        "DeferredBusy response must carry a non-empty notice"
    );

    // Verify message B is stored with Queued status, bound to run A.
    let history = runtime
        .thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: runtime.thread_scope.clone(),
            thread_id: thread_id.clone(),
        })
        .await
        .expect("thread history after B");
    let queued_messages: Vec<_> = history
        .messages
        .iter()
        .filter(|m| matches!(m.status, MessageStatus::Queued))
        .collect();
    assert_eq!(
        queued_messages.len(),
        1,
        "exactly one message should be stored as Queued after thread-busy submit"
    );
    assert_eq!(
        queued_messages[0].kind,
        MessageKind::User,
        "the Queued message must be of kind User"
    );
    assert_eq!(
        queued_messages[0].turn_run_id.as_deref(),
        Some(run_id_a.to_string().as_str()),
        "the Queued message must be bound to run A"
    );

    // Cancel run A — this is the terminal event that (must NOT) auto-resubmit B.
    runtime
        .cancel_run(
            &scope,
            run_id_a,
            SanitizedCancelReason::UserRequested,
            "rejected-busy-cancel-a",
        )
        .await
        .expect("run A cancellation succeeds");

    // B must remain RejectedBusy — no auto-resubmission should have fired.
    let history_after_cancel = runtime
        .thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: runtime.thread_scope.clone(),
            thread_id: thread_id.clone(),
        })
        .await
        .expect("thread history after cancel");
    // Identify message B by the message_id we captured from the pre-cancel history.
    // Using the stable message_id (rather than a simple Queued count) ensures
    // a regression that leaves the Queued row AND adds a Submitted row for the
    // same message cannot slip past as "still one Queued".
    let msg_b_id = queued_messages[0].message_id;

    let msg_b_after_cancel: Vec<_> = history_after_cancel
        .messages
        .iter()
        .filter(|m| m.message_id == msg_b_id)
        .collect();
    assert_eq!(
        msg_b_after_cancel.len(),
        1,
        "message B must appear exactly once in history after run A is cancelled"
    );
    assert_eq!(
        msg_b_after_cancel[0].status,
        MessageStatus::RejectedBusy,
        "message B must flip to RejectedBusy after run A is cancelled — resend affordance, no auto-resubmission"
    );
    // Guard: no additional Submitted row must have been created for message B's message_id.
    let submitted_for_b: Vec<_> = history_after_cancel
        .messages
        .iter()
        .filter(|m| matches!(m.status, MessageStatus::Submitted) && m.message_id == msg_b_id)
        .collect();
    assert!(
        submitted_for_b.is_empty(),
        "no Submitted row must exist for message B after run A is cancelled — got {submitted_for_b:?}"
    );

    // Submit message C — thread is free again, must be Submitted.
    let response_c = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        SUBMIT_TURN_COMMAND,
        ProductSubmitTurnRequest {
            extension_id: Some(TEST_SESSION_EXTENSION_ID.to_string()),
            client_action_id: Some("send-rejected-busy-c".to_string()),
            thread_id: Some(thread_id.to_string()),
            content: Some("message C after thread is free".to_string()),
            attachments: Vec::new(),
            model: None,
            mention_roots: Vec::new(),
        },
    )
    .await
    .expect("message C submit should not error");

    assert!(
        matches!(response_c, RebornSubmitTurnResponse::Submitted { .. }),
        "message C must be accepted after run A is cancelled, got {response_c:?}"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

struct MultiToolConfiguredCredentials;

#[async_trait]
impl ironclaw_auth::RuntimeCredentialAccountSelectionService for MultiToolConfiguredCredentials {
    async fn select_configured_account_for_binding(
        &self,
        _lookup: ironclaw_auth::CredentialAccountSelectionRequest,
        _runtime_scope: ironclaw_auth::AuthProductScope,
    ) -> Result<ironclaw_auth::CredentialAccount, ironclaw_auth::AuthProductError> {
        Err(ironclaw_auth::AuthProductError::CredentialMissing)
    }

    async fn select_unique_configured_runtime_account(
        &self,
        _request: ironclaw_auth::RuntimeCredentialAccountSelectionRequest,
    ) -> Result<ironclaw_auth::CredentialAccount, ironclaw_auth::AuthProductError> {
        let now = chrono::Utc::now();
        Ok(ironclaw_auth::CredentialAccount {
            id: ironclaw_auth::CredentialAccountId::new(),
            scope: ironclaw_auth::AuthProductScope::new(
                ironclaw_host_api::resource::ResourceScope::local_default(
                    UserId::new("multi-tool-credential-user").expect("user id"),
                    ironclaw_host_api::ids::InvocationId::new(),
                )
                .expect("resource scope"),
                ironclaw_auth::AuthSurface::Api,
            ),
            provider: ironclaw_auth::AuthProviderId::new("test-provider").expect("provider id"),
            label: ironclaw_auth::CredentialAccountLabel::new("test-provider")
                .expect("account label"),
            status: ironclaw_auth::CredentialAccountStatus::Configured,
            ownership: ironclaw_auth::CredentialOwnership::UserReusable,
            owner_extension: None,
            granted_extensions: Vec::new(),
            access_secret: Some(
                ironclaw_host_api::ids::SecretHandle::new("test-secret").expect("secret handle"),
            ),
            refresh_secret: None,
            scopes: Vec::new(),
            provider_identity: None,
            link_revision: 0,
            created_at: now,
            updated_at: now,
        })
    }
}

// ── Regression: scheduler liveness must not treat mutex contention as stopped ──

/// Verify three invariants of the scheduler liveness check introduced to fix the
/// `try_lock()` contention bug:
///
/// 1. Before shutdown: liveness check says NOT stopped (atomic flag = false).
/// 2. While mutex is momentarily held by another task: atomic flag is still false,
///    so the guard correctly treats that as "alive".
/// 3. After graceful `shutdown()`: liveness check says stopped (atomic flag = true).
///
/// The `stopped` atomic flag is the authoritative signal; `try_lock`
/// failure now means "alive" rather than "stopped".
#[tokio::test]
async fn scheduler_liveness_not_stopped_under_contention() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "liveness-test-reply".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "scheduler-liveness-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "scheduler-liveness-tenant".to_string(),
        agent_id: "scheduler-liveness-agent".to_string(),
        source_binding_id: "scheduler-liveness-source".to_string(),
        reply_target_binding_id: "scheduler-liveness-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_SEND_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input)
        .await
        .expect("runtime builds for liveness test");

    let conversation = runtime.new_conversation().await.expect("conversation");

    // Invariant 1: Before shutdown, the atomic stopped flag must be false.
    assert!(
        !runtime.turn_scheduler.atomic_stopped(),
        "scheduler_stopped must be false on a freshly built runtime"
    );

    // Invariant 2: While the scheduler handle mutex is held (simulating
    // shutdown/scheduler contention), the public submit path must NOT
    // return `WorkerStopped` — and must complete within a bounded timeout.
    //
    // `is_stopped()` uses `try_lock()` (non-blocking) on the handle mutex,
    // not `lock().await`, so holding the lock here cannot deadlock. Tokio's
    // Mutex is non-re-entrant: `try_lock()` inside `is_stopped()` will
    // fail (returning `Err`) because the current task already holds the guard.
    // The guard falls through to "alive" because the `stopped` flag is false.
    //
    // `notify()` sends through the notifier (not the handle mutex), so the
    // worker processes the turn while the test holds the handle. The
    // RecordingGateway resolves the model call synchronously, so the turn
    // reaches Completed. We assert the full Ok result to catch both the
    // liveness regression (WorkerStopped) and any other scheduler breakage.
    //
    // The surrounding `tokio::time::timeout` is the deadlock-regression
    // guard: if `is_stopped()` ever regresses from `try_lock()` to
    // `lock().await`, this test will panic with a clear message instead of
    // hanging CI indefinitely.
    {
        // Hold the tokio Mutex for the duration of the submit call.
        let _guard = runtime.turn_scheduler.handle_mutex().lock().await;

        let result = tokio::time::timeout(
            RUNTIME_SEND_TIMEOUT,
            runtime.send_user_message(&conversation, "liveness-probe"),
        )
        .await
        .expect(
            "send_user_message timed out while handle mutex was held — \
                 liveness guard likely regressed from try_lock() to lock().await, \
                 causing a deadlock",
        );

        assert!(
            result.is_ok(),
            "send_user_message must succeed (RecordingGateway completes the turn) \
                 while scheduler handle is merely contended (stopped=false); \
                 got: {result:?}"
        );
    } // guard released here — handle mutex is free again

    // Invariant 3: After the worker is stopped (flag = true), the public
    // submit path MUST return `WorkerStopped`.
    //
    // We use `stop_turn_runner_worker_for_manual_state_test` instead of
    // `shutdown()` because `shutdown()` consumes `self`, which would prevent
    // us from calling `send_user_message` afterward to exercise the guard.
    stop_turn_runner_worker_for_manual_state_test(&runtime).await;

    assert!(
        runtime.turn_scheduler.atomic_stopped(),
        "scheduler_stopped must be true after stop helper"
    );

    let result_after_stop = runtime
        .send_user_message(&conversation, "post-stop-probe")
        .await;
    assert!(
        matches!(
            result_after_stop,
            Err(crate::runtime::RebornRuntimeError::WorkerStopped)
        ),
        "send_user_message must return WorkerStopped after scheduler is stopped; \
             got: {result_after_stop:?}"
    );

    // shutdown() handles the already-taken scheduler handle gracefully.
    runtime.shutdown().await.expect("runtime shutdown");
}

/// Companion test: `stop_turn_runner_worker_for_manual_state_test` (the test-only
/// helper used by many existing tests) must also set `scheduler_stopped = true`
/// so the liveness guard correctly reports stopped after it is called.
#[tokio::test]
async fn scheduler_liveness_stopped_after_test_helper_stops_worker() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "liveness-helper-test-reply".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "scheduler-liveness-helper-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "scheduler-liveness-helper-tenant".to_string(),
        agent_id: "scheduler-liveness-helper-agent".to_string(),
        source_binding_id: "scheduler-liveness-helper-source".to_string(),
        reply_target_binding_id: "scheduler-liveness-helper-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input)
        .await
        .expect("runtime builds for helper-stopped test");

    // Before stopping: not stopped.
    assert!(
        !runtime.turn_scheduler.atomic_stopped(),
        "scheduler_stopped must be false before stop helper runs"
    );

    stop_turn_runner_worker_for_manual_state_test(&runtime).await;

    // After the test helper stops the worker: flag must be true.
    assert!(
        runtime.turn_scheduler.atomic_stopped(),
        "scheduler_stopped must be true after stop_turn_runner_worker_for_manual_state_test"
    );

    // shutdown() handles the already-taken scheduler handle gracefully
    // via the `if let Some` guard — safe to call after the test helper.
    runtime.shutdown().await.expect("runtime shutdown");
}

/// After `stop_turn_runner_worker_for_manual_state_test` sets
/// `scheduler_stopped = true`, `send_user_message` must immediately return
/// `Err(RebornRuntimeError::WorkerStopped)` without submitting the turn.
#[tokio::test]
async fn scheduler_stopped_rejects_send_user_message() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "stopped-reject-reply".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "scheduler-stopped-reject-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "scheduler-stopped-reject-tenant".to_string(),
        agent_id: "scheduler-stopped-reject-agent".to_string(),
        source_binding_id: "scheduler-stopped-reject-source".to_string(),
        reply_target_binding_id: "scheduler-stopped-reject-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input)
        .await
        .expect("runtime builds for stopped-reject test");

    let conversation = runtime.new_conversation().await.expect("conversation");

    // Capture thread history before the stopped-send to verify no side effects.
    let thread_service = runtime.session_thread_service();
    let thread_scope = runtime.thread_scope.clone();
    let history_before = thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: thread_scope.clone(),
            thread_id: conversation.0.clone(),
        })
        .await
        .expect("list history before stopped send");

    stop_turn_runner_worker_for_manual_state_test(&runtime).await;

    let result = runtime.send_user_message(&conversation, "hi").await;
    assert!(
        matches!(result, Err(RebornRuntimeError::WorkerStopped)),
        "send_user_message must return WorkerStopped when scheduler is stopped, got: {result:?}"
    );

    // Assert no side effects: history must not grow after the rejected send.
    let history_after = thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: thread_scope,
            thread_id: conversation.0.clone(),
        })
        .await
        .expect("list history after stopped send");
    assert_eq!(
        history_before.messages.len(),
        history_after.messages.len(),
        "send_user_message must not write any messages when WorkerStopped is returned"
    );

    // shutdown() handles the already-taken scheduler handle gracefully.
    runtime.shutdown().await.expect("runtime shutdown");
}

#[async_trait]
impl HostManagedModelGateway for SkillShellGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "expected capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let call_index = {
            let mut calls = self.calls.lock().expect("skill shell gateway lock");
            let index = *calls;
            *calls += 1;
            index
        };

        if call_index > 0 {
            // Second call: the shell has run, so capture what it returned for the assertions.
            if let Some(tool_result) = request
                .messages
                .iter()
                .find(|message| message.role == HostManagedModelMessageRole::ToolResult)
            {
                *self.shell_output.lock().expect("shell output lock") =
                    Some(tool_result.content.clone());
            }
            return Ok(HostManagedModelResponse::assistant_reply("done"));
        }

        // First call: find the staged-files note in whatever the model was actually given.
        let corpus = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let workdir = staged_workdir_from_body(&corpus).unwrap_or_else(|| {
            panic!("the activated skill body must advertise a staged workdir; got:\n{corpus}")
        });
        *self.advertised_workdir.lock().expect("workdir lock") = Some(workdir.clone());

        let shell_id = CapabilityId::new("builtin.shell").expect("shell id");
        let shell_tool = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?
            .into_iter()
            .find(|definition| definition.capability_id == shell_id)
            .expect("builtin.shell must be offered under a policy with a process backend");
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("skill-shell-turn".to_string()),
                id: "call-skill-shell".to_string(),
                name: shell_tool.name,
                // Exactly the shape the note tells the model to use.
                arguments: serde_json::json!({
                    "command": "python3 scripts/egfr.py --scr 1.3 --age 62 --female",
                    "workdir": workdir,
                }),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

/// Pull the advertised directory out of the staged-files note, the way a model reading it would.
///
/// Parsed rather than reconstructed: reconstructing it here would assert the fixture's own idea of
/// the path instead of the one the body carries.
fn staged_workdir_from_body(body: &str) -> Option<String> {
    let marker = "This skill's files are staged at `";
    let start = body.find(marker)? + marker.len();
    let rest = &body[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}
