//! Caller-level coverage for cache-forked compaction: a live loop call
//! through the real gateway records its provider request, and a compaction
//! through the real compaction port + system inference port replays it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::ids::{AgentId, CapabilityId, ProjectId, TenantId, UserId};
use ironclaw_host_api::resolution::{Resolution, ResolutionBatch};
use ironclaw_llm::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmError, LlmProvider, Role,
    ToolCall, ToolCompletionRequest, ToolCompletionResponse,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, InMemoryRunProfileResolver, LoopCapabilityPort, LoopCompactionMode,
    LoopCompactionOutcome, LoopCompactionPort, LoopCompactionRequest, LoopRequest,
    LoopRequestBatch, LoopRunContext, ProviderToolDefinition, RunProfileResolutionRequest,
    RunProfileResolver, SystemInferencePort, SystemInferenceTaskId, VisibleCapabilityRequest,
    VisibleCapabilitySurface,
};
use ironclaw_threads::{
    AcceptInboundMessageRequest, EnsureThreadRequest, InMemorySessionThreadService, MessageContent,
    SessionThreadService, ThreadHistoryRequest, ThreadMessageRangeRequest, ThreadScope,
};
use ironclaw_turns::{LoopMessageRef, TurnId, TurnRunId, TurnScope};

use crate::{
    HostManagedLoopCompactionPort, HostManagedModelGateway, HostManagedModelMessage,
    HostManagedModelMessageRole, HostManagedModelRequest, HostManagedToolResultContent,
    LlmModelProfilePolicy, LlmProviderModelGateway, ModelGatewayBackedSystemInferencePort,
    WarmPrefixCacheCompactionSource,
};

const PROVIDER_ID: &str = "fork-provider";
const PROVIDER_MODEL: &str = "fork-model";
const LIVE_SYSTEM_PROMPT: &str = "You are the live assistant. Follow the repository rules.";
const SUMMARIZER_PROMPT: &str = "## Active Task\nsummarizer format guidance";
const FORK_INSTRUCTION_MARKER: &str = "The conversation above is being compacted";

struct ScriptedProvider {
    tool_requests: Mutex<Vec<ToolCompletionRequest>>,
    completions: Mutex<Vec<CompletionRequest>>,
    tool_responses: Mutex<VecDeque<ToolCompletionResponse>>,
}

impl ScriptedProvider {
    fn new(tool_responses: Vec<ToolCompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            tool_requests: Mutex::new(Vec::new()),
            completions: Mutex::new(Vec::new()),
            tool_responses: Mutex::new(tool_responses.into()),
        })
    }

    fn tool_requests(&self) -> Vec<ToolCompletionRequest> {
        self.tool_requests.lock().expect("lock").clone()
    }

    fn completions(&self) -> Vec<CompletionRequest> {
        self.completions.lock().expect("lock").clone()
    }
}

fn text_tool_response(content: &str) -> ToolCompletionResponse {
    ToolCompletionResponse {
        content: Some(content.to_string()),
        tool_calls: Vec::new(),
        input_tokens: 10,
        output_tokens: 5,
        finish_reason: FinishReason::Stop,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning: None,
        reasoning_details: None,
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn model_name(&self) -> &str {
        PROVIDER_MODEL
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        Default::default()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.completions.lock().expect("lock").push(request);
        Ok(CompletionResponse {
            content: "standalone summary".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            finish_reason: FinishReason::Stop,
            reasoning: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            usage_source: ironclaw_host_api::resource::ModelUsageSource::Provider,
        })
    }

    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        self.tool_requests.lock().expect("lock").push(request);
        Ok(self
            .tool_responses
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| text_tool_response("unscripted reply")))
    }
}

struct ToolSurface;

#[async_trait]
impl LoopCapabilityPort for ToolSurface {
    fn tool_definitions(&self) -> Result<Vec<ProviderToolDefinition>, AgentLoopHostError> {
        Ok(vec![
            ProviderToolDefinition::from_parts(
                CapabilityId::new("builtin.read_file").expect("capability id"),
                "builtin__read_file",
                "Read a workspace file.",
                serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            )
            .expect("tool definition"),
        ])
    }

    async fn visible_capabilities(
        &self,
        _request: VisibleCapabilityRequest,
    ) -> Result<VisibleCapabilitySurface, AgentLoopHostError> {
        unreachable!("the warm-prefix test never lists capabilities")
    }

    async fn invoke_capability(
        &self,
        _request: LoopRequest,
    ) -> Result<Resolution, AgentLoopHostError> {
        unreachable!("the warm-prefix test never dispatches")
    }

    async fn invoke_capability_batch(
        &self,
        _request: LoopRequestBatch,
    ) -> Result<ResolutionBatch, AgentLoopHostError> {
        unreachable!("the warm-prefix test never dispatches")
    }
}

struct Fixture {
    provider: Arc<ScriptedProvider>,
    gateway: Arc<LlmProviderModelGateway<ScriptedProvider>>,
    threads: Arc<InMemorySessionThreadService>,
    scope: ThreadScope,
    run_context: LoopRunContext,
    /// Durable message refs, by sequence (index 0 = sequence 1).
    refs: Vec<LoopMessageRef>,
}

impl Fixture {
    async fn new(tool_responses: Vec<ToolCompletionResponse>) -> Self {
        let run_context = run_context().await;
        let provider = ScriptedProvider::new(tool_responses);
        let policy = LlmModelProfilePolicy::new().allow_model_profile(
            run_context.resolved_run_profile.model_profile_id.clone(),
            None,
        );
        let gateway = Arc::new(LlmProviderModelGateway::with_provider_identity(
            PROVIDER_ID,
            Arc::clone(&provider),
            policy,
        ));
        let threads = Arc::new(InMemorySessionThreadService::default());
        let scope = ThreadScope {
            tenant_id: TenantId::new("tenant-warm-prefix").expect("tenant"),
            agent_id: AgentId::new("agent-warm-prefix").expect("agent"),
            project_id: Some(ProjectId::new("project-warm-prefix").expect("project")),
            owner_user_id: Some(UserId::new("user-warm-prefix").expect("user")),
            mission_id: None,
        };
        threads
            .ensure_thread(EnsureThreadRequest {
                scope: scope.clone(),
                thread_id: Some(run_context.thread_id.clone()),
                created_by_actor_id: "tester".to_string(),
                title: None,
                metadata_json: None,
            })
            .await
            .expect("thread");
        for content in [
            "refactor the parser module",
            "now also update the tests",
            "and bump the changelog",
        ] {
            threads
                .accept_inbound_message(AcceptInboundMessageRequest {
                    scope: scope.clone(),
                    thread_id: run_context.thread_id.clone(),
                    actor_id: "user".to_string(),
                    source_binding_id: None,
                    reply_target_binding_id: None,
                    external_event_id: None,
                    content: MessageContent::text(content),
                })
                .await
                .expect("append user");
        }
        let refs = threads
            .list_thread_messages_range(ThreadMessageRangeRequest {
                scope: scope.clone(),
                thread_id: run_context.thread_id.clone(),
                after_sequence: 0,
                through_sequence: 3,
            })
            .await
            .expect("range")
            .messages
            .into_iter()
            .map(|message| crate::message_ref(message.message_id).expect("ref"))
            .collect();
        Self {
            provider,
            gateway,
            threads,
            scope,
            run_context,
            refs,
        }
    }

    /// The live loop prompt: system, user#1, a structured tool round, an
    /// assistant reply, user#2, user#3.
    fn live_request(&self, include_first_user: bool) -> HostManagedModelRequest {
        let mut messages = vec![host_message(
            HostManagedModelMessageRole::System,
            LIVE_SYSTEM_PROMPT,
            "msg:system-prompt",
        )];
        if include_first_user {
            messages.push(host_message(
                HostManagedModelMessageRole::User,
                "refactor the parser module",
                self.refs[0].as_str(),
            ));
        }
        messages.push(tool_result_message());
        messages.push(host_message(
            HostManagedModelMessageRole::Assistant,
            "I split the tokenizer out of parser.rs.",
            "msg:22222222-2222-2222-2222-222222222222",
        ));
        messages.push(host_message(
            HostManagedModelMessageRole::User,
            "now also update the tests",
            self.refs[1].as_str(),
        ));
        messages.push(host_message(
            HostManagedModelMessageRole::User,
            "and bump the changelog",
            self.refs[2].as_str(),
        ));
        HostManagedModelRequest {
            model_profile_id: self
                .run_context
                .resolved_run_profile
                .model_profile_id
                .clone(),
            fallback_index: 0,
            messages,
            surface_version: None,
            resolved_model_route: self.run_context.resolved_model_route.clone(),
            run_id: self.run_context.run_id,
            turn_id: self.run_context.turn_id,
            thread_id: Some(self.run_context.thread_id.clone()),
            tool_choice: None,
            response_format: None,
        }
    }

    async fn run_live_call(&self, include_first_user: bool) {
        self.gateway
            .stream_model_with_capabilities(
                self.live_request(include_first_user),
                Arc::new(ToolSurface),
            )
            .await
            .expect("live call");
    }

    fn compaction_port(
        &self,
        source_run_id: TurnRunId,
    ) -> HostManagedLoopCompactionPort<InMemorySessionThreadService> {
        let inference: Arc<dyn SystemInferencePort> =
            Arc::new(ModelGatewayBackedSystemInferencePort::new(
                Arc::clone(&self.gateway),
                self.run_context.clone(),
            ));
        let cache = self.gateway.warm_prefix_cache().expect("gateway cache");
        HostManagedLoopCompactionPort::new(
            inference,
            Arc::clone(&self.threads),
            self.scope.clone(),
            SUMMARIZER_PROMPT,
        )
        .with_warm_prefix_source(Arc::new(WarmPrefixCacheCompactionSource::new(
            cache,
            source_run_id,
        )))
    }

    async fn compact_through_second_user(&self, source_run_id: TurnRunId) {
        let outcome = self
            .compaction_port(source_run_id)
            .compact_loop_context(LoopCompactionRequest {
                task_id: SystemInferenceTaskId::new(),
                thread_id: self.run_context.thread_id.clone(),
                last_compacted_through_seq: None,
                drop_through_seq: 2,
                first_retained_seq: None,
                preserve_tail_tokens: 8_000,
                mode: LoopCompactionMode::Fresh,
                deadline_ms: 5_000,
            })
            .await
            .expect("compaction");
        assert!(matches!(outcome, LoopCompactionOutcome::Compacted(_)));
    }

    async fn persisted_summaries(&self) -> Vec<String> {
        self.threads
            .list_thread_history(ThreadHistoryRequest {
                scope: self.scope.clone(),
                thread_id: self.run_context.thread_id.clone(),
            })
            .await
            .expect("history")
            .summary_artifacts
            .into_iter()
            .map(|summary| summary.content)
            .collect()
    }
}

async fn run_context() -> LoopRunContext {
    let turn_scope = TurnScope::new(
        TenantId::new("tenant-warm-prefix").expect("tenant"),
        Some(AgentId::new("agent-warm-prefix").expect("agent")),
        Some(ProjectId::new("project-warm-prefix").expect("project")),
        ironclaw_host_api::ids::ThreadId::new("thread-warm-prefix").expect("thread"),
    );
    let resolved = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .expect("profile");
    LoopRunContext::new(turn_scope, TurnId::new(), TurnRunId::new(), resolved)
}

fn host_message(
    role: HostManagedModelMessageRole,
    content: &str,
    content_ref: &str,
) -> HostManagedModelMessage {
    HostManagedModelMessage {
        role,
        content: content.to_string(),
        content_ref: LoopMessageRef::new(content_ref).expect("ref"),
        tool_result_provider_call: None,
        tool_result_content: None,
        image_parts: Vec::new(),
    }
}

fn tool_result_message() -> HostManagedModelMessage {
    let observation = serde_json::json!({
        "schema_version": 1,
        "status": "success",
        "summary": "Tool completed; preview contains the full result.",
        "detail": {
            "kind": "result_reference",
            "result_ref": "result:read-parser",
            "byte_len": 12,
            "preview": "fn parse() {}"
        },
        "trust": "untrusted_tool_output"
    });
    let envelope = ironclaw_threads::ToolResultReferenceEnvelope::with_model_observation(
        "result:read-parser",
        ironclaw_threads::ToolResultSafeSummary::new("read done").expect("safe summary"),
        observation,
    )
    .expect("observation envelope");
    let call: ironclaw_threads::ProviderToolCallReferenceEnvelope =
        serde_json::from_value(serde_json::json!({
            "provider_id": PROVIDER_ID,
            "provider_model_id": PROVIDER_MODEL,
            "provider_turn_id": "turn-1",
            "provider_call_id": "call-read-parser",
            "provider_tool_name": "builtin__read_file",
            "capability_id": "builtin.read_file",
            "arguments": {"path": "src/parser.rs"}
        }))
        .expect("provider call");
    HostManagedModelMessage {
        tool_result_provider_call: Some(call),
        tool_result_content: Some(HostManagedToolResultContent::Reference { envelope }),
        ..host_message(
            HostManagedModelMessageRole::ToolResult,
            "read done",
            "msg:11111111-1111-1111-1111-111111111111",
        )
    }
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("serialize")
}

#[tokio::test]
async fn compaction_replays_the_live_tools_system_and_history_prefix_byte_for_byte() {
    let fixture = Fixture::new(vec![
        text_tool_response("live reply"),
        text_tool_response("forked summary"),
    ])
    .await;
    fixture.run_live_call(true).await;
    assert_eq!(
        fixture.gateway.warm_prefix_cache().expect("cache").len(),
        1,
        "the successful live call is recorded"
    );

    fixture
        .compact_through_second_user(fixture.run_context.run_id)
        .await;

    let requests = fixture.provider.tool_requests();
    assert_eq!(requests.len(), 2, "live call plus exactly one forked call");
    assert!(
        fixture.provider.completions().is_empty(),
        "no standalone fallback"
    );
    let (live, fork) = (&requests[0], &requests[1]);

    // [system, user#1, assistant tool call, tool result, assistant, user#2]
    // is the cut through the compacted range; user#3 stays live.
    let cut = 6;
    assert_eq!(live.messages.len(), cut + 1);
    assert_eq!(fork.messages.len(), cut + 1);
    assert_eq!(
        json(&fork.tools),
        json(&live.tools),
        "tools[] byte-identical"
    );
    assert_eq!(
        json(&fork.messages[..cut]),
        json(&live.messages[..cut]),
        "system + structured history prefix byte-identical"
    );
    assert_eq!(fork.messages[2].role, Role::Assistant);
    assert_eq!(
        fork.messages[2]
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.first())
            .map(|call| call.id.as_str()),
        Some("call-read-parser"),
        "assistant tool call keeps its original id"
    );
    assert_eq!(fork.messages[3].role, Role::Tool);
    assert_eq!(
        fork.messages[3].tool_call_id.as_deref(),
        Some("call-read-parser")
    );
    let tail = &fork.messages[cut];
    assert_eq!(tail.role, Role::User);
    assert!(tail.content.contains(FORK_INSTRUCTION_MARKER));
    assert!(tail.content.ends_with(SUMMARIZER_PROMPT));
    assert!(
        !json(&fork.messages).contains("and bump the changelog"),
        "the retained tail is not summarized"
    );

    // Same model, same thread-scoped cache key, same sampling envelope, no
    // tool_choice change (tool_choice changes invalidate message caches).
    assert_eq!(fork.model, live.model);
    assert_eq!(fork.model.as_deref(), Some(PROVIDER_MODEL));
    assert!(fork.tool_choice.is_none() && live.tool_choice.is_none());
    let cache_key = ironclaw_llm::PROMPT_CACHE_KEY_METADATA;
    assert!(live.metadata.contains_key(cache_key));
    assert_eq!(fork.metadata.get(cache_key), live.metadata.get(cache_key));
    assert_eq!(fork.metadata, live.metadata);

    assert_eq!(fixture.persisted_summaries().await.len(), 1);
    assert!(fixture.persisted_summaries().await[0].contains("forked summary"));
}

#[tokio::test]
async fn a_tool_call_from_the_fork_is_never_executed_and_falls_back_to_standalone() {
    let mut tool_call_reply = text_tool_response("");
    tool_call_reply.tool_calls = vec![ToolCall {
        id: "call-x".to_string(),
        name: "builtin__read_file".to_string(),
        arguments: serde_json::json!({"path": "secrets.env"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    }];
    tool_call_reply.finish_reason = FinishReason::ToolUse;
    let fixture = Fixture::new(vec![text_tool_response("live reply"), tool_call_reply]).await;
    fixture.run_live_call(true).await;

    fixture
        .compact_through_second_user(fixture.run_context.run_id)
        .await;

    assert_eq!(fixture.provider.tool_requests().len(), 2);
    let standalone = fixture.provider.completions();
    assert_eq!(standalone.len(), 1, "standalone XML compaction ran");
    assert_eq!(standalone[0].messages[0].role, Role::System);
    assert_eq!(standalone[0].messages[0].content, SUMMARIZER_PROMPT);
    let summaries = fixture.persisted_summaries().await;
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].contains("standalone summary"));
}

#[tokio::test]
async fn absent_stale_or_incomplete_warm_prefix_compacts_standalone() {
    // Absent: no live call recorded.
    let fixture = Fixture::new(Vec::new()).await;
    fixture
        .compact_through_second_user(fixture.run_context.run_id)
        .await;
    assert!(fixture.provider.tool_requests().is_empty());
    assert_eq!(fixture.provider.completions().len(), 1);

    // Stale: recorded by another run of the thread.
    let fixture = Fixture::new(vec![text_tool_response("live reply")]).await;
    fixture.run_live_call(true).await;
    fixture.compact_through_second_user(TurnRunId::new()).await;
    assert_eq!(fixture.provider.tool_requests().len(), 1, "no fork attempt");
    assert_eq!(fixture.provider.completions().len(), 1);

    // Incomplete: the live prompt no longer contains user#1.
    let fixture = Fixture::new(vec![text_tool_response("live reply")]).await;
    fixture.run_live_call(false).await;
    fixture
        .compact_through_second_user(fixture.run_context.run_id)
        .await;
    assert_eq!(fixture.provider.tool_requests().len(), 1, "no fork attempt");
    assert_eq!(fixture.provider.completions().len(), 1);
}

#[tokio::test]
async fn gateway_rejects_a_fork_whose_scanned_view_differs_from_the_recording() {
    let fixture = Fixture::new(vec![text_tool_response("live reply")]).await;
    fixture.run_live_call(true).await;
    let cache = fixture.gateway.warm_prefix_cache().expect("cache");
    let recording = cache
        .latest(&fixture.run_context.thread_id)
        .expect("recording");
    let cut = recording
        .cut_covering(&fixture.refs[..2])
        .expect("cut through user#2");
    let (system_prompt, mut context_messages) = recording.scan_view(cut).expect("view");
    context_messages[0]
        .content
        .push_str(" (edited after scanning)");

    let error = fixture
        .gateway
        .stream_cache_fork(crate::HostManagedCacheForkRequest {
            fork: recording.cache_fork(cut).expect("fork"),
            model_profile_id: fixture
                .run_context
                .resolved_run_profile
                .model_profile_id
                .clone(),
            fallback_index: 0,
            resolved_model_route: fixture.run_context.resolved_model_route.clone(),
            run_id: fixture.run_context.run_id,
            turn_id: fixture.run_context.turn_id,
            thread_id: fixture.run_context.thread_id.clone(),
            system_prompt,
            context_messages,
            tail_instruction: "summarize".to_string(),
        })
        .await
        .expect_err("view mismatch is rejected");
    assert_eq!(error.kind, crate::HostManagedModelErrorKind::Unavailable);
    assert_eq!(
        fixture.provider.tool_requests().len(),
        1,
        "nothing reaches the provider"
    );
}

#[test]
fn cut_points_never_split_a_tool_round() {
    let refs = ["msg:a", "msg:b", "msg:c", "msg:d"];
    let boundaries = super::coalesced_boundaries(
        vec![
            super::WarmPrefixBoundary {
                content_ref: LoopMessageRef::new(refs[0]).expect("ref"),
                provider_message_count: Some(2),
            },
            super::WarmPrefixBoundary {
                content_ref: LoopMessageRef::new(refs[1]).expect("ref"),
                provider_message_count: Some(3),
            },
            // Two results of one provider turn: one converted unit.
            super::WarmPrefixBoundary {
                content_ref: LoopMessageRef::new(refs[2]).expect("ref"),
                provider_message_count: None,
            },
            super::WarmPrefixBoundary {
                content_ref: LoopMessageRef::new(refs[3]).expect("ref"),
                provider_message_count: Some(6),
            },
        ],
        2,
    );
    // Two leading system messages coalesce into one.
    assert_eq!(
        boundaries
            .iter()
            .map(|boundary| boundary.provider_message_count)
            .collect::<Vec<_>>(),
        vec![None, Some(2), None, Some(5)]
    );
}

#[test]
fn cache_is_bounded_and_keeps_one_recording_per_thread() {
    let cache = Arc::new(super::WarmPrefixCache::with_limits(2, 1024));
    let profile =
        ironclaw_loop_contracts::ModelProfileId::new("interactive_model").expect("profile id");
    let record = |thread: &str, content: &str| {
        let capture = super::WarmPrefixCapture::new(
            &cache,
            Some(&ironclaw_host_api::ids::ThreadId::new(thread).expect("thread")),
            TurnRunId::new(),
            &profile,
            None,
            0,
            Some(Vec::new()),
        )
        .expect("capture");
        capture.record_completion(CompletionRequest::new(vec![
            ChatMessage::system("system"),
            ChatMessage::user(content),
        ]));
    };
    record("thread-a", "first");
    record("thread-a", "second");
    assert_eq!(
        cache.len(),
        1,
        "a newer request replaces the thread's older one"
    );
    record("thread-b", "b");
    record("thread-c", "c");
    assert_eq!(cache.len(), 2, "least recently recorded thread evicted");
    let thread_a = ironclaw_host_api::ids::ThreadId::new("thread-a").expect("thread");
    assert!(cache.latest(&thread_a).is_none());
    record("thread-c", &"x".repeat(2048));
    let thread_c = ironclaw_host_api::ids::ThreadId::new("thread-c").expect("thread");
    assert!(
        cache.latest(&thread_c).is_none(),
        "an oversized newer request drops the stale one and is not kept"
    );
}
