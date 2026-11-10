use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_loop_contracts::{
    LoopCapabilityPort, ProviderToolCall, RegisterProviderToolCallRequest,
};
use ironclaw_loop_host::{
    HostManagedModelError, HostManagedModelErrorKind, HostManagedModelGateway,
    HostManagedModelMessageRole, HostManagedModelRequest, HostManagedModelResponse,
    ToolDisclosureMode,
};
use ironclaw_threads::{MessageKind, ThreadHistoryRequest};
use ironclaw_turns::TurnStatus;

use crate::runtime_input::{PollSettings, RebornRuntimeIdentity, RebornRuntimeInput};

use super::build_reborn_runtime;

const RUNTIME_SEND_TIMEOUT: Duration = Duration::from_secs(20);

fn provider_tool_call(
    tool_definitions: &[ironclaw_loop_contracts::ProviderToolDefinition],
    capability_id: &str,
    call_id: &str,
    arguments: serde_json::Value,
) -> ProviderToolCall {
    let capability_id = CapabilityId::new(capability_id).expect("capability id");
    let tool = tool_definitions
        .iter()
        .find(|definition| definition.capability_id == capability_id)
        .unwrap_or_else(|| panic!("{capability_id} provider tool definition should exist"));
    ProviderToolCall {
        provider_id: "test-provider".to_string(),
        provider_model_id: "test-model".to_string(),
        turn_id: Some("provider-turn-1".to_string()),
        id: call_id.to_string(),
        name: tool.name.clone(),
        arguments,
        response_reasoning: None,
        reasoning: None,
        signature: None,
    }
}

fn model_capability_error(error: impl std::fmt::Display) -> HostManagedModelError {
    let safe_summary = error.to_string();
    HostManagedModelError::safe(HostManagedModelErrorKind::Unavailable, safe_summary)
}

#[derive(Debug, Default)]
struct ReplyAttachmentGateway {
    calls: StdMutex<usize>,
}

#[async_trait]
impl HostManagedModelGateway for ReplyAttachmentGateway {
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
            let mut calls = self
                .calls
                .lock()
                .expect("reply attachment gateway call lock poisoned");
            let current = *calls;
            *calls += 1;
            current
        };
        if call_index >= 2 {
            let tool_result_count = request
                .messages
                .iter()
                .filter(|message| message.role == HostManagedModelMessageRole::ToolResult)
                .count();
            assert_eq!(tool_result_count, 2);
            return Ok(HostManagedModelResponse::assistant_reply(
                "The CSV is attached.",
            ));
        }

        let tool_definitions = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?;
        let call = match call_index {
            0 => provider_tool_call(
                &tool_definitions,
                "builtin.write_file",
                "call-write-reply-attachment",
                serde_json::json!({
                    "path": "/workspace/reports/result.csv",
                    "content": "name,value\nalpha,1\n"
                }),
            ),
            1 => provider_tool_call(
                &tool_definitions,
                "builtin.attach_workspace_file_to_reply",
                "call-attach-workspace-file",
                serde_json::json!({
                    "path": "/workspace/reports/result.csv"
                }),
            ),
            _ => unreachable!("handled above"),
        };
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(call))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

#[tokio::test]
async fn production_reply_attachment_capability_registers_durable_run_intent() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway: Arc<dyn HostManagedModelGateway> = Arc::new(ReplyAttachmentGateway::default());
    let input =
        RebornRuntimeInput::from_build_input(crate::deployment::local_filesystem_build_input(
            "runtime-reply-attachment-owner",
            root.path().join("standalone"),
        ))
        .with_tool_disclosure(ToolDisclosureMode::Off)
        .with_identity(RebornRuntimeIdentity {
            tenant_id: "runtime-reply-attachment-tenant".to_string(),
            agent_id: "runtime-reply-attachment-agent".to_string(),
            source_binding_id: "runtime-reply-attachment-source".to_string(),
            reply_target_binding_id: "runtime-reply-attachment-reply".to_string(),
        })
        .with_poll_settings(PollSettings {
            interval: Duration::from_millis(10),
            max_total: RUNTIME_SEND_TIMEOUT,
        })
        .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(
            &conversation,
            "Create a CSV report and attach it to your reply.",
        ),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");
    assert_eq!(reply.status, TurnStatus::Completed);

    let history = runtime
        .thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: runtime.thread_scope.clone(),
            thread_id: conversation.0.clone(),
        })
        .await
        .expect("thread history");
    let assistant = history
        .messages
        .iter()
        .find(|message| message.kind == MessageKind::Assistant)
        .expect("finalized assistant reply");
    assert_eq!(assistant.attachments.len(), 1);
    assert_eq!(
        assistant.attachments[0].storage_key.as_deref(),
        Some("/workspace/reports/result.csv")
    );
    assert_eq!(
        assistant.attachments[0].filename.as_deref(),
        Some("result.csv")
    );
    assert_eq!(assistant.attachments[0].mime_type, "text/csv");
    assert_eq!(assistant.attachments[0].size_bytes, Some(19));

    runtime.shutdown().await.expect("runtime shutdown");
}

/// A crash between vendor egress and the result write leaves a one-shot
/// attempt `Sending`. The next boot over the same storage marks it `Unknown`,
/// the terminal-ambiguous state nothing ever resends, and leaves attempts that
/// already reached a terminal state alone.
#[tokio::test]
async fn restart_marks_attempts_left_sending_by_a_crash_unknown() {
    use ironclaw_host_api::ids::ThreadId;
    use ironclaw_host_api::turn::{ReplyTargetBindingRef, TurnScope};
    use ironclaw_outbound::{
        OutboundDeliveryAttempt, OutboundDeliveryId, OutboundDeliveryStatus, OutboundPushCandidate,
        OutboundPushKind, ProjectionUpdateRef,
    };

    let root = tempfile::tempdir().expect("tempdir");
    let storage = root.path().join("standalone");
    let input = || {
        RebornRuntimeInput::from_build_input(crate::deployment::local_filesystem_build_input(
            "runtime-delivery-recovery-owner",
            storage.clone(),
        ))
        .with_identity(RebornRuntimeIdentity {
            tenant_id: "runtime-delivery-recovery-tenant".to_string(),
            agent_id: "runtime-delivery-recovery-agent".to_string(),
            source_binding_id: "runtime-delivery-recovery-source".to_string(),
            reply_target_binding_id: "runtime-delivery-recovery-reply".to_string(),
        })
    };

    let crashed = build_reborn_runtime(input()).await.expect("first boot");
    assert!(
        crashed.delivery_coordinator().is_some(),
        "the standalone runtime owns a delivery coordinator"
    );
    let (store, ..) = crashed
        .outbound_delivery_stores_for_test()
        .expect("outbound stores");
    let attempt = |thread: &str, status: OutboundDeliveryStatus| {
        let scope = TurnScope::new_with_owner(
            crashed.thread_scope.tenant_id.clone(),
            Some(crashed.thread_scope.agent_id.clone()),
            crashed.thread_scope.project_id.clone(),
            ThreadId::new(thread).expect("thread id"),
            crashed.thread_scope.owner_user_id.clone(),
        );
        OutboundDeliveryAttempt {
            delivery_id: OutboundDeliveryId::new(),
            candidate: OutboundPushCandidate {
                tenant_id: scope.tenant_id.clone(),
                agent_id: scope.agent_id.clone(),
                project_id: scope.project_id.clone(),
                thread_id: scope.thread_id.clone(),
                turn_run_id: None,
                target: ReplyTargetBindingRef::new("tg:-100:_:42").expect("reply target"),
                kind: OutboundPushKind::DeliveryStatus,
                projection_ref: ProjectionUpdateRef::new(format!("projection:{thread}"))
                    .expect("projection ref"),
                requires_reply_target_revalidation: false,
            },
            scope,
            status,
            attempted_at: chrono::Utc::now(),
            failure_kind: None,
        }
    };
    let interrupted = attempt("crashed-send", OutboundDeliveryStatus::Sending);
    let delivered = attempt("finished-send", OutboundDeliveryStatus::Delivered);
    for seeded in [&interrupted, &delivered] {
        store
            .record_delivery_attempt(seeded.clone())
            .await
            .expect("seed attempt");
    }
    crashed.shutdown().await.expect("first shutdown");

    let restarted = build_reborn_runtime(input()).await.expect("restart");
    let (store, ..) = restarted
        .outbound_delivery_stores_for_test()
        .expect("outbound stores");
    let status_of = |seeded: &OutboundDeliveryAttempt| {
        let store = Arc::clone(&store);
        let scope = seeded.scope.clone();
        let delivery_id = seeded.delivery_id;
        async move {
            store
                .load_delivery_attempt(scope, delivery_id)
                .await
                .expect("attempt loads")
                .expect("attempt kept")
                .status
        }
    };
    assert_eq!(
        status_of(&interrupted).await,
        OutboundDeliveryStatus::Unknown
    );
    assert_eq!(
        status_of(&delivered).await,
        OutboundDeliveryStatus::Delivered
    );
    restarted.shutdown().await.expect("restart shutdown");
}
