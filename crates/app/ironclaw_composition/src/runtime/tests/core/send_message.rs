//! `send_user_message` through the standalone runtime: replies, model outages,
//! trace enrollment, ownership, origin rendering, hosted MCP activation, and
//! cancellation propagation.

use super::*;

#[tokio::test]
async fn standalone_yolo_message_flow_ignores_model_budget_gate() {
    let root = tempfile::tempdir().expect("tempdir");
    let host_home = root.path().join("host-home");
    std::fs::create_dir_all(&host_home).expect("host home");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "yolo budget bypass reply".to_string(),
        requests: Arc::clone(&requests),
    });
    let cost_table = ironclaw_loop_host::StaticModelCostTable::new().with_entry(
        ModelProfileId::new("interactive_model").expect("model profile id"),
        ModelCost {
            input_per_token: dec!(1.00),
            output_per_token: dec!(1.00),
            max_output_tokens: 8_192,
        },
    );

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input_with_profile(
            crate::RebornCompositionProfile::StandaloneUnrestricted,
            "runtime-yolo-budget-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(
            crate::standalone_unrestricted_runtime_policy(true)
                .expect("local-yolo policy resolves"),
        )
        .with_local_runtime_confirmed_host_home_root(host_home),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-yolo-budget-tenant".to_string(),
        agent_id: "runtime-yolo-budget-agent".to_string(),
        source_binding_id: "runtime-yolo-budget-source".to_string(),
        reply_target_binding_id: "runtime-yolo-budget-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway)
    .with_model_cost_table_override(Arc::new(cost_table));

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "ping"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);
    assert_eq!(reply.text.as_deref(), Some("yolo budget bypass reply"));
    assert_eq!(
        recorded_request_count(&requests),
        1,
        "standalone-unrestricted must reach the model gateway even when a paid cost table is present"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn send_user_message_returns_completed_assistant_text_with_recording_gateway() {
    let root = tempfile::tempdir().expect("tempdir");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "recorded runtime reply".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-success-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-success-tenant".to_string(),
        agent_id: "runtime-success-agent".to_string(),
        source_binding_id: "runtime-success-source".to_string(),
        reply_target_binding_id: "runtime-success-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "ping"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);
    assert_eq!(reply.text.as_deref(), Some("recorded runtime reply"));
    assert_eq!(recorded_request_count(&requests), 1);

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn send_user_message_preserves_model_unavailable_after_retry_budget() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(ModelOutageGateway::default());
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-model-outage-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-model-outage-tenant".to_string(),
        agent_id: "runtime-model-outage-agent".to_string(),
        source_binding_id: "runtime-model-outage-source".to_string(),
        reply_target_binding_id: "runtime-model-outage-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway.clone())
    // Keep >= 2 retries (the test pins retry-then-fail) but well under
    // the production budget so the deliberate outage fails in seconds.
    .with_model_availability_retry_attempts(std::num::NonZeroU32::new(2).expect("nonzero"));

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "please write a long report"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Failed);
    assert_eq!(reply.failure_category.as_deref(), Some("model_unavailable"));
    assert_eq!(reply.text, None);
    assert!(
        gateway.calls.load(Ordering::SeqCst) >= 3,
        "model outage should be retried before the run fails"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

/// End-to-end Trace Commons auto-capture: a real runtime turn through
/// `send_user_message` must, for an enrolled owner scope, land a redacted
/// envelope in that scope's submission queue without any manual trace
/// command. This drives the full chain: turn completion → lifecycle bus →
/// best-effort capture sink → thread-history read → redact/score →
/// eligibility → queue (+ immediate flush attempt, which fails locally
/// against the closed loopback endpoint and must leave the entry queued).
#[tokio::test]
async fn send_user_message_auto_queues_trace_for_enrolled_scope() {
    use ironclaw_trace_commons::contribution as trace_contribution;

    let owner = format!("runtime-trace-capture-owner-{}", uuid::Uuid::new_v4());
    // Trace state is keyed by the tenant-scoped composite, so enroll (and
    // later read the queue) under `trace_scope_key(tenant, owner)`, not the
    // bare owner id.
    let scope = trace_contribution::trace_scope_key("runtime-trace-capture-tenant", &owner);
    // Closed loopback port: the immediate flush fails fast and locally; no
    // traffic leaves the machine.
    let policy = trace_contribution::StandingTraceContributionPolicy::default()
        .set_enabled(true)
        .set_ingestion_endpoint("https://127.0.0.1:1/v1/traces")
        .set_min_submission_score(0.0)
        .set_require_manual_approval_when_pii_detected(false)
        .set_auto_submit_high_value_traces(true);
    trace_contribution::write_trace_policy_for_scope(Some(&scope), &policy)
        .expect("write trace policy");

    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "auto capture reply".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(&owner, root.path().join("standalone"))
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trace-capture-tenant".to_string(),
        agent_id: "runtime-trace-capture-agent".to_string(),
        source_binding_id: "runtime-trace-capture-source".to_string(),
        reply_target_binding_id: "runtime-trace-capture-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "capture this turn"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");
    assert_eq!(reply.status, TurnStatus::Completed);

    // The capture task is detached from the lifecycle path; poll briefly.
    let queue_dir =
        trace_contribution::trace_contribution_dir_for_scope(Some(&scope)).join("queue");
    let queued = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|entry| {
                    // Fail loud on a per-entry IO error too, so the test
                    // can't silently drop a broken entry and still claim the
                    // queue holds exactly one envelope.
                    entry
                        .unwrap_or_else(|error| {
                            panic!(
                                "failed to read a trace queue entry in {}: {error}",
                                dir.display()
                            )
                        })
                        .path()
                })
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.ends_with(".json") && !name.ends_with(".held.json")
                        })
                })
                .collect(),
            // The queue dir not existing yet is the expected pre-capture
            // state; any other IO error is a real failure the test must not
            // mask as "no queued traces".
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("failed to read trace queue dir {}: {error}", dir.display()),
        }
    };
    let mut entries = Vec::new();
    for _ in 0..150 {
        entries = queued(&queue_dir);
        if !entries.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        entries.len(),
        1,
        "a completed turn for an enrolled scope must auto-queue one trace envelope"
    );
    let body = std::fs::read_to_string(&entries[0]).expect("queued envelope readable");
    let envelope: serde_json::Value = serde_json::from_str(&body).expect("envelope is JSON");
    assert_eq!(envelope["outcome"]["task_success"], "success");

    runtime.shutdown().await.expect("runtime shutdown");
    #[allow(clippy::let_underscore_must_use)] // best-effort per-test scope dir cleanup
    let _ = std::fs::remove_dir_all(trace_contribution::trace_contribution_dir_for_scope(Some(
        &scope,
    )));
}

/// Regression guard: `send_user_message` must persist a
/// `TurnOwner::Personal` (the bound actor user) in `product_context`,
/// not a `TurnOwner::SharedAgent`.  Before the fix, `turn_scope_for`
/// built an ownerless scope whose `product_owner` resolved to
/// `SharedAgent` because `agent_id` was set and no explicit owner was
/// carried.
#[tokio::test(flavor = "multi_thread")]
async fn send_user_message_persists_personal_owner_for_webui() {
    use ironclaw_host_api::turn::TurnOwner;

    let root = tempfile::tempdir().expect("tempdir");
    let actor_owner_id = "runtime-personal-owner-user";
    let gateway = Arc::new(RecordingGateway {
        reply: "owner-check reply".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            actor_owner_id,
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-personal-owner-tenant".to_string(),
        agent_id: "runtime-personal-owner-agent".to_string(),
        source_binding_id: "runtime-personal-owner-source".to_string(),
        reply_target_binding_id: "runtime-personal-owner-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "ping"),
    )
    .await
    .expect("runtime send should finish within timeout")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);

    // Verify the persisted product_context carries Personal{user: actor_user_id},
    // not SharedAgent.
    let scope = runtime.turn_scope_for(&conversation.0);
    let run_state = runtime
        .turn_coordinator
        .get_run_state(GetRunStateRequest {
            scope,
            run_id: reply.run_id,
        })
        .await
        .expect("get_run_state should succeed");

    let product_context = run_state
        .product_context
        .expect("product_context must be set by send_user_message");
    let expected_user_id = UserId::new(actor_owner_id).expect("actor user id should be valid");
    assert!(
        matches!(
            &product_context.owner,
            TurnOwner::Personal { user } if user == &expected_user_id
        ),
        "send_user_message must persist TurnOwner::Personal{{user: actor_user_id}}, \
             got {:?}",
        product_context.owner
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

/// Regression guard: `send_user_message` resolves product context via
/// `resolve_web_ui`, which sets `TurnOriginKind::WebUi`.  The runtime
/// context section rendered into the model request must therefore contain
/// the WebUI origin line produced by
/// `LoopRuntimeContext::render_model_content`.  Previously, only the
/// persisted `product_context` owner was asserted; this test closes the
/// gap by asserting the *rendered* origin appears in the captured model
/// request.
#[tokio::test]
async fn send_user_message_renders_cli_origin_in_model_request() {
    let root = tempfile::tempdir().expect("tempdir");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "webui-origin-check reply".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-origin-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-origin-tenant".to_string(),
        agent_id: "runtime-webui-origin-agent".to_string(),
        source_binding_id: "runtime-webui-origin-source".to_string(),
        reply_target_binding_id: "runtime-webui-origin-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "ping"),
    )
    .await
    .expect("runtime send should finish within timeout")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);

    // The runtime-context system message carries the rendered
    // `LoopRuntimeContext` — its content_ref uses the "runtime" section
    // prefix stamped by `push_runtime_context`.
    let runtime_context_content = {
        let requests = requests
            .lock()
            .expect("recording gateway requests lock poisoned");
        requests[0]
            .messages
            .iter()
            .find(|message| {
                message.role == HostManagedModelMessageRole::System
                    && message
                        .content_ref
                        .as_str()
                        .starts_with("msg:runtime.loop-start.")
            })
            .expect(
                "model request must include a runtime-context system message \
                     (content_ref starts with msg:runtime.loop-start.)",
            )
            .content
            .clone()
    };

    // Exact string produced by LoopRuntimeContext::render_model_content for
    // local runtime chat, which stamps the first-party source channel as CLI.
    assert!(
        runtime_context_content.contains("Run origin: CLI chat; replies render in this session."),
        "runtime-context system message must contain the CLI origin line, \
             got: {runtime_context_content:?}"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn hosted_mcp_activation_stays_pending_until_preparation_completes() {
    let root = tempfile::tempdir().expect("tempdir");
    let host_home = root.path().join("host-home");
    std::fs::create_dir_all(&host_home).expect("host home");
    let gateway = Arc::new(AuthGateToolCallingGateway::default());
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway.clone();
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input_with_profile(
            RebornCompositionProfile::StandaloneUnrestricted,
            "runtime-auth-gate-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(
            crate::standalone_unrestricted_runtime_policy(true)
                .expect("local-yolo policy resolves"),
        )
        .with_local_runtime_confirmed_host_home_root(host_home),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-auth-gate-tenant".to_string(),
        agent_id: "runtime-auth-gate-agent".to_string(),
        source_binding_id: "runtime-auth-gate-source".to_string(),
        reply_target_binding_id: "runtime-auth-gate-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let extension_management = &runtime.extension_management;
    let notion_ref = LifecyclePackageRef::new(LifecyclePackageKind::Extension, "notion")
        .expect("valid notion ref");
    extension_management
        .install(
            notion_ref.clone(),
            extension_management.tenant_operator_user_id_for_test(),
        )
        .await
        .expect("install Notion MCP");
    // Hosted-MCP discovery belongs to pending preparation, not activation
    // mode selection. The prechecked helper bypasses that product seam, so
    // activation remains visibly pending instead of publishing a guessed tool
    // catalog.
    let activation = extension_management
        .activate_with_prechecked_credentials_for_test(notion_ref)
        .await
        .expect("pending hosted-MCP activation returns a lifecycle response");
    assert_eq!(activation.phase, InstallationState::Installed);

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn cancel_run_propagates_to_children_when_event_sink_is_unavailable() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-cancel-child-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-cancel-child-tenant".to_string(),
        agent_id: "runtime-cancel-child-agent".to_string(),
        source_binding_id: "runtime-cancel-child-source".to_string(),
        reply_target_binding_id: "runtime-cancel-child-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let mut runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let overloaded_event_sink = Arc::new(OverloadedEventSink::default());
    let event_sink: Arc<dyn NonBlockingEventSink> = overloaded_event_sink.clone();
    runtime.runtime_event_sink = event_sink;
    stop_turn_runner_worker_for_manual_state_test(&runtime).await;
    let conversation = runtime.new_conversation().await.expect("conversation");
    let parent_scope = runtime.turn_scope_for(&conversation.0);
    let actor = TurnActor::new(runtime.actor_user_id.clone());
    let parent = runtime
        .turn_coordinator
        .submit_turn(SubmitTurnRequest {
            subagent_activation_provenance: None,
            requested_model: None,
            output_contract: None,
            scope: parent_scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: AcceptedMessageRef::new("msg:cancel-parent").unwrap(),
            requested_run_profile: None,
            idempotency_key: IdempotencyKey::new("cancel-parent").unwrap(),
            received_at: Utc::now(),
            requested_run_id: None,
            parent_run_id: None,
            subagent_depth: 0,
            spawn_tree_root_run_id: None,
            product_context: None,
        })
        .await
        .expect("parent submitted");
    let SubmitTurnResponse::Accepted {
        run_id: parent_run_id,
        ..
    } = parent;
    let child_scope = TurnScope::new_with_owner(
        parent_scope.tenant_id.clone(),
        parent_scope.agent_id.clone(),
        parent_scope.project_id.clone(),
        ThreadId::new("runtime-cancel-child-thread").unwrap(),
        parent_scope.explicit_owner_user_id().cloned(),
    );
    let child = runtime
        .turn_tree_store
        .submit_child_turn(
            SubmitChildRunRequest {
                parent_scope: parent_scope.clone(),
                parent_run_id,
                child_scope: child_scope.clone(),
                actor,
                accepted_message_ref: AcceptedMessageRef::new("msg:cancel-child").unwrap(),
                requested_run_profile: None,
                output_contract: None,
                idempotency_key: IdempotencyKey::new("cancel-child").unwrap(),
                received_at: Utc::now(),
                requested_run_id: None,
                spawn_tree_descendant_cap: 4,
                process_dependency: None,
                process_input: None,
            },
            &AllowAllTurnAdmissionPolicy,
            &InMemoryRunProfileResolver::default(),
        )
        .await
        .expect("child submitted");
    let SubmitTurnResponse::Accepted {
        run_id: child_run_id,
        ..
    } = child;

    runtime
        .cancel_run(
            &parent_scope,
            parent_run_id,
            SanitizedCancelReason::UserRequested,
            "test-parent-cancel",
        )
        .await
        .expect("parent cancellation succeeds");

    let result_ref = LoopResultRef::new("result:runtime-cancel-child").unwrap();
    let parent_resolved_run_profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .expect("resolve run profile");
    let parent_run_context = LoopRunContext::new(
        parent_scope.clone(),
        TurnId::new(),
        parent_run_id,
        parent_resolved_run_profile,
    );
    runtime
        .thread_service
        .append_tool_result_reference(AppendToolResultReferenceRequest {
            intrinsic_outcome: None,
            scope: runtime.thread_scope.clone(),
            thread_id: parent_scope.thread_id.clone(),
            turn_run_id: parent_run_id.to_string(),
            result_ref: result_ref.as_str().to_string(),
            safe_summary: ToolResultSafeSummary::new("subagent spawned").unwrap(),
            provider_call: None,
            model_observation: None,
        })
        .await
        .expect("parent result reference seeded");
    let child_thread_scope = ThreadScope {
        tenant_id: child_scope.tenant_id.clone(),
        agent_id: child_scope.agent_id.clone().unwrap(),
        project_id: child_scope.project_id.clone(),
        owner_user_id: Some(runtime.actor_user_id.clone()),
        mission_id: None,
    };
    runtime
        .thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: child_thread_scope,
            thread_id: Some(child_scope.thread_id.clone()),
            created_by_actor_id: "test".to_string(),
            title: Some("Subagent".to_string()),
            metadata_json: Some(
                serde_json::to_string(&SubagentThreadMetadata {
                    kind: SubagentThreadKind::Subagent,
                    parent_run_id,
                    parent_thread_id: parent_scope.thread_id.clone(),
                    tree_root_run_id: parent_run_id,
                    child_run_id,
                    subagent_kind: SubagentKindId::new("general").unwrap(),
                    mode: SpawnSubagentMode::Blocking,
                    result_ref,
                    spawn_provider_call_id: None,
                    handoff: None,
                    parent_run_context: parent_run_context.clone(),
                    gate_ref: ironclaw_host_api::turn::TurnGateRef::new(
                        "gate:runtime-cancel-child",
                    )
                    .unwrap(),
                })
                .unwrap(),
            ),
        })
        .await
        .expect("child thread metadata seeded");

    let child_state = runtime
        .turn_coordinator
        .get_run_state(GetRunStateRequest {
            scope: child_scope,
            run_id: child_run_id,
        })
        .await
        .expect("child state");
    assert_eq!(child_state.status, TurnStatus::Cancelled);

    {
        let cancellation_events = overloaded_event_sink
            .attempted_events
            .lock()
            .expect("attempted event recording mutex is not poisoned");
        assert_eq!(
            cancellation_events.len(),
            2,
            "parent and child cancellation events must both be attempted"
        );
        assert!(
            cancellation_events
                .iter()
                .all(|event| event.kind == RuntimeEventKind::LoopCancelled),
            "the best-effort cancellation path must emit only the expected cancellation records"
        );
    }

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn send_user_message_uses_caller_supplied_skill_context_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "should not reach model".to_string(),
        requests: Arc::clone(&requests),
    });
    let skill_context_source = Arc::new(FailingSkillContextSource::default());
    let skill_context_source_for_input: Arc<dyn HostSkillContextSource> =
        skill_context_source.clone();
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-skill-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-skill-tenant".to_string(),
        agent_id: "runtime-skill-agent".to_string(),
        source_binding_id: "runtime-skill-source".to_string(),
        reply_target_binding_id: "runtime-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_skill_context_source(skill_context_source_for_input)
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "ping"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_ne!(reply.status, TurnStatus::Completed);
    assert_eq!(
        skill_context_source.calls.load(Ordering::SeqCst),
        ironclaw_processes::MAX_CRASH_RECOVERY_RECLAIMS as usize,
        "composition should retry caller-supplied transient skill context failures only up to the durable claim bound"
    );
    assert!(
        requests
            .lock()
            .expect("recording gateway requests lock poisoned")
            .is_empty(),
        "skill context failure should stop before model dispatch"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}
