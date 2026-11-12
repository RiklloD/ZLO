//! Host-runtime capabilities exposed to model calls and the tool-call
//! trajectory / safe-preview observers.

use super::*;

#[tokio::test]
async fn standalone_runtime_exposes_host_runtime_capabilities_to_model_calls() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(ToolCallingGateway::default());
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway.clone();
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-tools-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-tools-tenant".to_string(),
        agent_id: "runtime-tools-agent".to_string(),
        source_binding_id: "runtime-tools-source".to_string(),
        reply_target_binding_id: "runtime-tools-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "use echo tool"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    assert_eq!(reply.text.as_deref(), Some("tool ok"));
    assert_eq!(
        *gateway
            .stream_model_calls
            .lock()
            .expect("tool gateway stream count lock poisoned"),
        0,
        "runtime should use capability-aware model path"
    );
    assert_eq!(
        gateway
            .requests
            .lock()
            .expect("tool gateway requests lock poisoned")
            .len(),
        2,
        "tool call should require initial request plus tool-result follow-up"
    );
    let history = runtime
        .thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: runtime.thread_scope.clone(),
            thread_id: conversation.0.clone(),
        })
        .await
        .expect("thread history");
    let tool_result = history
        .messages
        .iter()
        .find(|message| message.kind == MessageKind::ToolResultReference)
        .expect("tool result reference should persist in thread history");
    assert!(
        tool_result
            .tool_result_ref
            .as_deref()
            .is_some_and(|result_ref| result_ref.starts_with("result:")),
        "tool result should persist a durable result ref"
    );
    assert!(
        tool_result.tool_result_provider_call.is_none(),
        "product thread history should scrub provider replay metadata"
    );
    let context = runtime
        .thread_service
        .load_context_messages(LoadContextMessagesRequest {
            scope: runtime.thread_scope.clone(),
            thread_id: conversation.0.clone(),
            message_ids: vec![tool_result.message_id],
        })
        .await
        .expect("tool result context");
    let provider_call = context.messages[0]
        .tool_result_provider_call
        .as_ref()
        .expect("model context should preserve provider replay metadata");
    assert_eq!(provider_call.provider_call_id, "call-1");
    assert_eq!(
        provider_call.capability_id,
        CapabilityId::new("builtin.echo").unwrap()
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

impl crate::RebornTrajectoryObserver for RecordingTrajectoryObserver {
    fn on_capability_input(
        &self,
        call_id: &str,
        capability_id: &str,
        arguments: &serde_json::Value,
    ) {
        self.inputs.lock().expect("inputs lock").push((
            call_id.to_string(),
            capability_id.to_string(),
            arguments.clone(),
        ));
    }

    fn on_capability_result(&self, call_id: &str, capability_id: &str, output: &serde_json::Value) {
        self.results.lock().expect("results lock").push((
            call_id.to_string(),
            capability_id.to_string(),
            output.clone(),
        ));
    }
}

/// End-to-end guard for the #4588 trajectory observer seam: a real runtime
/// turn that dispatches the `builtin.echo` capability must fire BOTH the
/// input and result callbacks installed via
/// `RebornRuntimeInput::with_raw_trajectory_observer`. This drives the
/// result hook on the genuine dispatch path (the prior direct-call unit
/// test was dropped as false confidence — it stayed green even when
/// end-to-end dispatch was broken).
#[tokio::test]
async fn standalone_runtime_forwards_tool_call_trajectory_to_raw_observer() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(ToolCallingGateway::default());
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway.clone();
    let observer = Arc::new(RecordingTrajectoryObserver::default());
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trajectory-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trajectory-tenant".to_string(),
        agent_id: "runtime-trajectory-agent".to_string(),
        source_binding_id: "runtime-trajectory-source".to_string(),
        reply_target_binding_id: "runtime-trajectory-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    // Raw (not safe-preview) so we can assert verbatim arguments + output.
    .with_raw_trajectory_observer(observer.clone())
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "use echo tool"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");
    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    // Shut down before inspecting the recorded callbacks so the std-Mutex
    // guards are never held across an `.await` (clippy::await_holding_lock).
    runtime.shutdown().await.expect("runtime shutdown");

    let echo_id = CapabilityId::new("builtin.echo").unwrap();

    let inputs = observer.inputs.lock().expect("inputs lock");
    assert_eq!(inputs.len(), 1, "exactly one capability input observed");
    let (input_call_id, input_capability, arguments) = &inputs[0];
    assert!(!input_call_id.is_empty(), "input call_id should be present");
    assert_eq!(input_capability, echo_id.as_str());
    assert_eq!(
        arguments,
        &serde_json::json!({"message": "hello from tool"}),
        "observer should receive the raw model-emitted tool arguments"
    );

    let results = observer.results.lock().expect("results lock");
    assert_eq!(results.len(), 1, "exactly one capability result observed");
    let (result_call_id, result_capability, output) = &results[0];
    assert_eq!(result_capability, echo_id.as_str());
    assert_eq!(
        result_call_id, input_call_id,
        "result and input callbacks correlate by call_id"
    );
    assert!(
        output.to_string().contains("hello from tool"),
        "observer should receive the staged capability output, got {output}"
    );
}

/// Caller-level guard for the **default** (safe-preview) observer path:
/// installing via the public `with_trajectory_observer` and driving a real
/// turn with a large tool payload must deliver a *bounded* preview to the
/// observer — proving truncation is wired between dispatch and the observer,
/// not just unit-tested on the helper in isolation.
#[tokio::test]
async fn standalone_runtime_safe_preview_observer_receives_bounded_payload() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(LargeEchoToolCallingGateway::default());
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway.clone();
    let observer = Arc::new(RecordingTrajectoryObserver::default());
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-preview-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-preview-tenant".to_string(),
        agent_id: "runtime-preview-agent".to_string(),
        source_binding_id: "runtime-preview-source".to_string(),
        reply_target_binding_id: "runtime-preview-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    // Default path → safe-preview truncation applied before the observer.
    .with_trajectory_observer(observer.clone())
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "echo a big payload"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");
    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    // Shut down before inspecting the recorded callbacks so the std-Mutex
    // guards are never held across an `.await` (clippy::await_holding_lock).
    runtime.shutdown().await.expect("runtime shutdown");

    let original_len = large_echo_message().len();

    let inputs = observer.inputs.lock().expect("inputs lock");
    assert_eq!(inputs.len(), 2, "echo and result_read inputs observed");
    let observed_message = inputs[0].2["message"].as_str().expect("message string");
    assert!(
        observed_message.len() < original_len && observed_message.contains("[truncated"),
        "observer should receive a truncated preview of the large argument, got {} bytes",
        observed_message.len()
    );
    assert_eq!(inputs[1].1, "builtin.result_read");

    let results = observer.results.lock().expect("results lock");
    assert_eq!(results.len(), 2, "echo and result_read outputs observed");
    assert!(
        results[0].2.to_string().contains("[truncated"),
        "observer should receive a truncated preview of the large result"
    );
    assert_eq!(results[1].1, "builtin.result_read");
}
