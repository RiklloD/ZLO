//! Console API-key credential (`ANTHROPIC_API_KEY`) through the native
//! Messages API provider: auth headers, native SSE streaming with exact stop
//! reasons, cache breakpoints on the *streamed* path, the semantic-progress
//! idle deadline, flatten-only tool schemas, routed effort, and signed
//! thinking replay across a tool loop.

use super::*;
use crate::sse_test_server::{DeltaRecorder, ScriptedFrame, captured, serve_sse};
use std::time::{Duration, Instant};

fn api_key_provider(
    base_url: &str,
    retention: CacheRetention,
    timeout_secs: u64,
) -> AnthropicProvider {
    let mut config = RegistryProviderConfig::generic(
        crate::registry::ProviderProtocol::Anthropic,
        "anthropic",
        Some(SecretString::from("sk-ant-test-key".to_string())),
        base_url,
        "claude-opus-4-6",
    );
    config.cache_retention = retention;
    AnthropicProvider::new_api_key(&config, timeout_secs).expect("api-key provider")
}

fn message_start() -> ScriptedFrame {
    ScriptedFrame::event(
        "message_start",
        serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":21,"cache_read_input_tokens":13,"cache_creation_input_tokens":5}}}),
    )
}

fn text_delta(index: u64, text: &str) -> ScriptedFrame {
    ScriptedFrame::event(
        "content_block_delta",
        serde_json::json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
    )
}

fn stop(stop_reason: &str) -> Vec<ScriptedFrame> {
    vec![
        ScriptedFrame::event(
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":9}}),
        ),
        ScriptedFrame::event("message_stop", serde_json::json!({"type":"message_stop"})),
    ]
}

fn ping() -> ScriptedFrame {
    ScriptedFrame::event("ping", serde_json::json!({"type":"ping"}))
}

fn search_tool() -> ToolDefinition {
    ToolDefinition {
        name: "search".to_string(),
        description: "Search the web".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["query"]
        }),
    }
}

/// The interactive loop streams tool turns. Before this provider, the
/// API-key path went through rig-core with streaming disabled (zero deltas,
/// a 60 s total cap) and the OAuth streamed path dropped every
/// `cache_control` breakpoint. Pin the whole wire contract in one turn.
#[tokio::test]
async fn api_key_tool_stream_sends_key_auth_breakpoints_and_native_schema() {
    let mut frames = vec![
        message_start(),
        ScriptedFrame::event(
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ),
        ScriptedFrame::event(
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need a search."}}),
        ),
        ScriptedFrame::event(
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}),
        ),
        ScriptedFrame::event(
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        ),
        text_delta(1, "Searching "),
        text_delta(1, "now."),
        ScriptedFrame::event(
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"search","input":{}}}),
        ),
        ScriptedFrame::event(
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}}),
        ),
    ];
    frames.extend(stop("tool_use"));
    let (base_url, request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::Short, 30);
    let sink = Arc::new(DeltaRecorder::default());

    let response = provider
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(
                vec![
                    ChatMessage::system("You are helpful."),
                    ChatMessage::user("Find rust news."),
                ],
                vec![search_tool()],
            ),
            sink.clone(),
        )
        .await
        .expect("native stream completes");
    let request = captured(request).await;

    // Console API-key auth, never the OAuth Bearer/beta pair.
    assert_eq!(request.request_line, "POST /v1/messages HTTP/1.1");
    assert_eq!(request.header("x-api-key"), Some("sk-ant-test-key"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert!(request.header("authorization").is_none());
    assert!(request.header("anthropic-beta").is_none());

    // Native streaming.
    let body = &request.body;
    assert_eq!(body["stream"], true, "{body}");

    // Explicit cache breakpoints on the *streamed* request.
    assert_eq!(
        body["system"][0]["cache_control"]["type"], "ephemeral",
        "{body}"
    );
    assert_eq!(
        body["tools"][0]["cache_control"]["type"], "ephemeral",
        "{body}"
    );
    let last_message = body["messages"]
        .as_array()
        .expect("messages")
        .last()
        .cloned();
    let last_block = last_message
        .as_ref()
        .and_then(|message| message["content"].as_array())
        .and_then(|blocks| blocks.last())
        .expect("last message is cache-marked block content");
    assert_eq!(last_block["cache_control"]["type"], "ephemeral", "{body}");

    // Native JSON Schema: the optional field stays optional.
    let schema = &body["tools"][0]["input_schema"];
    assert_eq!(schema["required"], serde_json::json!(["query"]), "{body}");
    assert_eq!(schema["properties"]["limit"]["type"], "integer", "{body}");

    // Deltas streamed as they arrived; the response stays authoritative.
    assert_eq!(sink.text(), ["Searching ", "now."]);
    assert_eq!(sink.reasoning(), ["Need a search."]);
    assert_eq!(response.content.as_deref(), Some("Searching now."));
    assert_eq!(
        response.finish_reason,
        crate::provider::FinishReason::ToolUse
    );
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "toolu_1");
    assert_eq!(
        response.tool_calls[0].arguments,
        serde_json::json!({"query":"rust"})
    );
    assert_eq!(response.input_tokens, 21);
    assert_eq!(response.output_tokens, 9);
    assert_eq!(response.cache_read_input_tokens, 13);
    assert_eq!(response.cache_creation_input_tokens, 5);
    assert_eq!(
        response.reasoning_details,
        Some(ReasoningDetails {
            id: None,
            content: vec![ReasoningDetail::Text {
                text: "Need a search.".to_string(),
                signature: Some("sig-abc".to_string()),
            }],
        }),
        "the signed thinking block must survive for replay"
    );
}

/// `Long` retention stamps the 1h TTL on every streamed breakpoint, matching
/// the OAuth transport.
#[tokio::test]
async fn api_key_long_retention_streams_1h_ttl_markers() {
    let mut frames = vec![message_start(), text_delta(0, "ok")];
    frames.extend(stop("end_turn"));
    let (base_url, request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::Long, 30);

    provider
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(
                vec![ChatMessage::system("sys"), ChatMessage::user("hi")],
                vec![search_tool()],
            ),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("stream completes");
    let body = captured(request).await.body;
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h", "{body}");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h", "{body}");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["ttl"], "1h",
        "{body}"
    );
    assert_eq!(provider.cache_write_multiplier(), Decimal::TWO);
    assert_eq!(provider.cache_read_discount(), Decimal::TEN);
}

/// Long generations stream past the idle window as long as semantic progress
/// keeps arriving: there is no total wall-clock cap.
#[tokio::test]
async fn api_key_stream_outlives_the_idle_window_while_progress_arrives() {
    let mut frames = vec![message_start()];
    for chunk in 0..6 {
        frames.push(text_delta(0, &format!("chunk{chunk} ")).delayed(Duration::from_millis(400)));
    }
    frames.extend(stop("end_turn"));
    let (base_url, _request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 1);
    let sink = Arc::new(DeltaRecorder::default());

    let started = Instant::now();
    let response = provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("write a lot")]),
            sink.clone(),
        )
        .await
        .expect("progressing stream must not hit the idle deadline");
    assert!(
        started.elapsed() > Duration::from_secs(2),
        "the stream must have run past the 1s idle window"
    );
    assert_eq!(sink.text().len(), 6);
    assert_eq!(response.finish_reason, crate::provider::FinishReason::Stop);
}

/// `ping` keepalives are not model progress: a stream that only pings after
/// its first frame fails at the idle deadline instead of hanging forever.
#[tokio::test]
async fn api_key_pings_do_not_extend_the_idle_deadline() {
    let mut frames = vec![message_start(), text_delta(0, "partial")];
    for _ in 0..20 {
        frames.push(ping().delayed(Duration::from_millis(250)));
    }
    frames.extend(stop("end_turn"));
    let (base_url, _request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 1);

    let started = Instant::now();
    let error = provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hello")]),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect_err("keepalive-only stream must hit the idle deadline");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(&error, LlmError::StreamInterrupted { provider, reason }
            if provider == "anthropic" && reason.contains("no model progress")),
        "{error:?}"
    );
}

/// The provider's exact stop reason survives the stream (rig-core 0.36
/// dropped it and inferred from shape).
#[tokio::test]
async fn api_key_stream_preserves_exact_stop_reasons() {
    for (stop_reason, expected) in [
        ("max_tokens", crate::provider::FinishReason::Length),
        ("refusal", crate::provider::FinishReason::ContentFilter),
        ("stop_sequence", crate::provider::FinishReason::Stop),
        ("pause_turn", crate::provider::FinishReason::Unknown),
    ] {
        let mut frames = vec![message_start(), text_delta(0, "text")];
        frames.extend(stop(stop_reason));
        let (base_url, _request) = serve_sse(frames).await;
        let provider = api_key_provider(&base_url, CacheRetention::None, 30);
        let response = provider
            .complete_streaming(
                CompletionRequest::new(vec![ChatMessage::user("hello")]),
                Arc::new(DeltaRecorder::default()),
            )
            .await
            .expect("stream completes");
        assert_eq!(response.finish_reason, expected, "{stop_reason}");
    }
}

fn tool_loop_history(reasoning_details: Option<ReasoningDetails>) -> Vec<ChatMessage> {
    let mut assistant = ChatMessage::assistant_with_tool_calls(
        None,
        vec![ToolCall {
            id: "toolu_1".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"query":"rust"}),
            ..ToolCall::default()
        }],
    );
    assistant.reasoning_details = reasoning_details;
    vec![
        ChatMessage::user("Find rust news."),
        assistant,
        ChatMessage::tool_result("toolu_1", "search", "results"),
    ]
}

fn effort_request(messages: Vec<ChatMessage>, effort: &str) -> ToolCompletionRequest {
    let mut request = ToolCompletionRequest::new(messages, vec![search_tool()]);
    request.metadata.insert(
        crate::provider_routing::REASONING_EFFORT_METADATA.to_string(),
        effort.to_string(),
    );
    request
}

/// A routed effort on a tool loop enables adaptive thinking only when the
/// in-progress assistant tool call can lead with its signed thinking block,
/// and that block is replayed verbatim ahead of the `tool_use`.
#[tokio::test]
async fn routed_effort_replays_signed_thinking_across_the_tool_loop() {
    let signed = ReasoningDetails {
        id: None,
        content: vec![
            ReasoningDetail::Text {
                text: "Need a search.".to_string(),
                signature: Some("sig-abc".to_string()),
            },
            ReasoningDetail::Redacted {
                data: "opaque".to_string(),
            },
        ],
    };
    let mut frames = vec![message_start(), text_delta(0, "done")];
    frames.extend(stop("end_turn"));
    let (base_url, request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 30);
    provider
        .complete_with_tools_streaming(
            effort_request(tool_loop_history(Some(signed)), "high"),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("stream completes");
    let body = captured(request).await.body;
    assert_eq!(body["thinking"]["type"], "adaptive", "{body}");
    assert_eq!(body["output_config"]["effort"], "high", "{body}");
    let assistant = &body["messages"][1]["content"];
    assert_eq!(assistant[0]["type"], "thinking", "{body}");
    assert_eq!(assistant[0]["thinking"], "Need a search.");
    assert_eq!(assistant[0]["signature"], "sig-abc");
    assert_eq!(assistant[1]["type"], "redacted_thinking");
    assert_eq!(assistant[1]["data"], "opaque");
    assert_eq!(assistant[2]["type"], "tool_use");

    // History without a signed block (a pre-thinking turn, another
    // provider): effort still applies, thinking stays off, nothing replayed.
    let mut frames = vec![message_start(), text_delta(0, "done")];
    frames.extend(stop("end_turn"));
    let (base_url, request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 30);
    provider
        .complete_with_tools_streaming(
            effort_request(tool_loop_history(None), "high"),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("stream completes");
    let body = captured(request).await.body;
    assert!(body.get("thinking").is_none(), "{body}");
    assert_eq!(body["output_config"]["effort"], "high", "{body}");
    assert_eq!(
        body["messages"][1]["content"][0]["type"], "tool_use",
        "{body}"
    );
}

/// The buffered tool path also carries signed thinking back out.
#[tokio::test]
async fn buffered_tool_response_captures_signed_and_redacted_thinking() {
    let body = serde_json::json!({
        "content": [
            {"type":"thinking","thinking":"plan","signature":"sig-1"},
            {"type":"redacted_thinking","data":"enc"},
            {"type":"tool_use","id":"toolu_9","name":"search","input":{"query":"x"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 4}
    });
    let (base_url, _request) = crate::sse_test_server::serve(
        200,
        "application/json",
        vec![ScriptedFrame::now(body.to_string())],
    )
    .await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 30);
    let response = provider
        .complete_with_tools(ToolCompletionRequest::new(
            vec![ChatMessage::user("go")],
            vec![search_tool()],
        ))
        .await
        .expect("buffered response");
    assert_eq!(
        response.finish_reason,
        crate::provider::FinishReason::ToolUse
    );
    assert_eq!(
        response.reasoning_details,
        Some(ReasoningDetails {
            id: None,
            content: vec![
                ReasoningDetail::Text {
                    text: "plan".to_string(),
                    signature: Some("sig-1".to_string()),
                },
                ReasoningDetail::Redacted {
                    data: "enc".to_string(),
                },
            ],
        })
    );
}

/// A plain completion with a routed effort sends adaptive thinking and
/// `output_config.effort`; a model without effort levels sends neither.
#[tokio::test]
async fn routed_effort_reaches_plain_completions() {
    let mut frames = vec![message_start(), text_delta(0, "ok")];
    frames.extend(stop("end_turn"));
    let (base_url, request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 30);
    let mut completion = CompletionRequest::new(vec![ChatMessage::user("think")]);
    completion.metadata.insert(
        crate::provider_routing::REASONING_EFFORT_METADATA.to_string(),
        "max".to_string(),
    );
    provider
        .complete_streaming(completion, Arc::new(DeltaRecorder::default()))
        .await
        .expect("stream completes");
    let body = captured(request).await.body;
    assert_eq!(body["thinking"]["type"], "adaptive", "{body}");
    assert_eq!(body["output_config"]["effort"], "max", "{body}");
}

/// API-key model discovery hits `{base}/v1/models` with `x-api-key`.
#[tokio::test]
async fn api_key_lists_models_with_key_auth() {
    let (base_url, request) = crate::sse_test_server::serve(
        200,
        "application/json",
        vec![ScriptedFrame::now(
            r#"{"data":[{"id":"claude-opus-4-6"},{"id":"claude-haiku-4-5"}]}"#,
        )],
    )
    .await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 30);
    let models = provider.list_models().await.expect("models listed");
    assert_eq!(models, ["claude-opus-4-6", "claude-haiku-4-5"]);
    let request = captured(request).await;
    assert_eq!(request.request_line, "GET /v1/models HTTP/1.1");
    assert_eq!(request.header("x-api-key"), Some("sk-ant-test-key"));
}

/// An API-key 401 is a typed auth failure for the configured provider — the
/// OAuth credential-store refresh never runs for a Console key.
#[tokio::test]
async fn api_key_401_is_auth_failed_without_oauth_refresh() {
    let (base_url, _request) = crate::sse_test_server::serve(
        401,
        "application/json",
        vec![ScriptedFrame::now(
            r#"{"error":{"message":"invalid x-api-key"}}"#,
        )],
    )
    .await;
    let provider = api_key_provider(&base_url, CacheRetention::None, 30);
    let started = Instant::now();
    let error = provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hello")]),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect_err("401 must fail");
    assert!(
        matches!(&error, LlmError::AuthFailed { provider } if provider == "anthropic"),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(450),
        "no OAuth refresh back-off for an API key"
    );
}

/// Regression: the OAuth transport's *streamed* paths (what the interactive
/// loop uses) built their request without `apply_cache_breakpoints`, so
/// `claude login` users never got explicit cache breakpoints while
/// streaming. Both credentials now share one request builder.
#[tokio::test]
async fn oauth_streamed_requests_carry_cache_breakpoints() {
    let mut frames = vec![message_start(), text_delta(0, "ok")];
    frames.extend(stop("end_turn"));
    let (base_url, request) = serve_sse(frames).await;
    let mut config = RegistryProviderConfig::generic(
        crate::registry::ProviderProtocol::Anthropic,
        "anthropic_oauth",
        None,
        &base_url,
        "claude-opus-4-6",
    );
    config.oauth_token = Some(SecretString::from("sk-ant-oat-test".to_string()));
    config.cache_retention = CacheRetention::Short;
    let provider = AnthropicProvider::new_oauth(&config, 30).expect("oauth provider");
    provider
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(
                vec![ChatMessage::system("sys"), ChatMessage::user("hi")],
                vec![search_tool()],
            ),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("stream completes");
    let request = captured(request).await;
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-ant-oat-test")
    );
    assert_eq!(request.header("anthropic-beta"), Some("oauth-2025-04-20"));
    let body = request.body;
    assert_eq!(
        body["system"][0]["cache_control"]["type"], "ephemeral",
        "{body}"
    );
    assert_eq!(
        body["tools"][0]["cache_control"]["type"], "ephemeral",
        "{body}"
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral",
        "{body}"
    );
}
