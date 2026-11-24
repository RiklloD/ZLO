//! OpenAI platform API key (`OPENAI_API_KEY`) on the shared Responses API
//! engine: Bearer key auth with no Codex OAuth extras, native SSE streaming
//! with exact finish reasons, `prompt_cache_key`, encrypted-reasoning
//! round-trips, sampling options, JSON-object mode, and the idle-progress
//! deadline.

use super::*;
use crate::sse_test_server::{DeltaRecorder, ScriptedFrame, captured, serve, serve_sse};
use std::time::Instant;

fn api_key_provider(base_url: &str, model: &str, timeout_secs: u64) -> OpenAiCodexProvider {
    OpenAiCodexProvider::new_api_key(model, base_url, "sk-test-openai", timeout_secs)
        .expect("api-key provider")
}

fn event(data: serde_json::Value) -> ScriptedFrame {
    ScriptedFrame::data(data)
}

fn completed(status: &str) -> ScriptedFrame {
    event(serde_json::json!({
        "type": "response.completed",
        "response": {"status": status, "usage": {"input_tokens": 40, "output_tokens": 7, "input_tokens_details": {"cached_tokens": 32}}}
    }))
}

fn text_delta(text: &str) -> ScriptedFrame {
    event(serde_json::json!({"type": "response.output_text.delta", "delta": text}))
}

fn search_tool() -> ToolDefinition {
    ToolDefinition {
        name: "search".to_string(),
        description: "Search".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
    }
}

/// One streamed tool turn pins the whole API-key wire contract.
#[tokio::test]
async fn api_key_tool_stream_uses_bearer_key_and_streams_natively() {
    let frames = vec![
        event(serde_json::json!({"type":"response.created","response":{"status":"in_progress"}})),
        event(
            serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"Thinking..."}),
        ),
        event(
            serde_json::json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAAenc"}}),
        ),
        text_delta("Let me "),
        text_delta("search."),
        event(
            serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search"}}),
        ),
        event(
            serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"query\":\"rust\"}"}),
        ),
        event(
            serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search","arguments":"{\"query\":\"rust\"}"}}),
        ),
        completed("completed"),
    ];
    let (base_url, request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 30);
    let sink = Arc::new(DeltaRecorder::default());
    let mut tool_request = ToolCompletionRequest::new(
        vec![ChatMessage::system("sys"), ChatMessage::user("find rust")],
        vec![search_tool()],
    )
    .with_max_tokens(2048);
    tool_request.metadata.insert(
        crate::provider::PROMPT_CACHE_KEY_METADATA.to_string(),
        "thread-key-123".to_string(),
    );

    let response = provider
        .complete_with_tools_streaming(tool_request, sink.clone())
        .await
        .expect("native stream completes");
    let request = captured(request).await;

    assert_eq!(request.request_line, "POST /responses HTTP/1.1");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-test-openai")
    );
    assert!(request.header("chatgpt-account-id").is_none());
    assert!(request.header("openai-beta").is_none());
    let body = &request.body;
    assert_eq!(body["stream"], true, "{body}");
    assert_eq!(body["store"], false, "{body}");
    assert_eq!(body["prompt_cache_key"], "thread-key-123", "{body}");
    assert_eq!(body["max_output_tokens"], 2048, "{body}");
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    // Plain function tools — no plan-route namespace grouping.
    assert_eq!(body["tools"][0]["type"], "function", "{body}");
    assert_eq!(body["tools"][0]["name"], "search", "{body}");

    assert_eq!(sink.text(), ["Let me ", "search."]);
    assert_eq!(sink.reasoning(), ["Thinking..."]);
    assert_eq!(response.content.as_deref(), Some("Let me search."));
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(
        response.tool_calls[0].arguments,
        serde_json::json!({"query":"rust"})
    );
    assert_eq!(response.input_tokens, 40);
    assert_eq!(response.cache_read_input_tokens, 32);
    assert!(
        response.tool_calls[0].signature.is_some(),
        "encrypted reasoning rides the tool call for replay"
    );
    assert_eq!(provider.provider_id(), "openai");
    let (input_cost, _) = provider.cost_per_token();
    assert!(input_cost > Decimal::ZERO, "API keys are billed per token");
}

/// `response.incomplete` is a real terminal event: a `max_output_tokens`
/// stop is `Length` (and its partial tool call is not promoted to
/// `ToolUse`), not a dropped-connection `StreamInterrupted`.
#[tokio::test]
async fn api_key_incomplete_response_reports_exact_length() {
    let frames = vec![
        text_delta("partial"),
        event(
            serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search","arguments":"{\"query\":\"ru"}}),
        ),
        event(serde_json::json!({
            "type": "response.incomplete",
            "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 2}}
        })),
    ];
    let (base_url, _request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 30);
    let sink = Arc::new(DeltaRecorder::default());
    let response = provider
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(vec![ChatMessage::user("go")], vec![search_tool()]),
            sink.clone(),
        )
        .await
        .expect("incomplete is a terminal response, not an interruption");
    assert_eq!(response.finish_reason, FinishReason::Length);
    // A truncated call whose arguments are not JSON is never announced.
    assert_eq!(sink.events(), ["text:partial"]);
}

/// P2.1: a closed `function_call` item is announced through
/// `tool_call_ready` while the stream is still open — before the marker
/// text that follows it, and before call #2's arguments finish streaming.
#[tokio::test]
async fn api_key_stream_announces_a_closed_function_call_before_the_next_finishes() {
    let frames = vec![
        event(
            serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search"}}),
        ),
        event(
            serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"query\":\"a\"}"}),
        ),
        event(
            serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search","arguments":"{\"query\":\"a\"}"}}),
        ),
        text_delta("between").delayed(Duration::from_millis(50)),
        event(
            serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"search"}}),
        ),
        event(
            serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"query\":"}),
        ),
        event(
            serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"\"b\"}"}),
        )
        .delayed(Duration::from_millis(50)),
        event(
            serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"search","arguments":"{\"query\":\"b\"}"}}),
        ),
        completed("completed"),
    ];
    let (base_url, _request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 30);
    let sink = Arc::new(DeltaRecorder::default());
    let response = provider
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(vec![ChatMessage::user("go")], vec![search_tool()]),
            sink.clone(),
        )
        .await
        .expect("stream completes");

    assert_eq!(
        sink.events(),
        ["tool:call_1", "text:between", "tool:call_2"]
    );
    // The final response stays authoritative: both calls, in order.
    let ids: Vec<&str> = response
        .tool_calls
        .iter()
        .map(|call| call.id.as_str())
        .collect();
    assert_eq!(ids, ["call_1", "call_2"]);
    assert_eq!(
        response.tool_calls[1].arguments,
        serde_json::json!({"query": "b"})
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

/// A refusal content part is a content-policy stop.
#[tokio::test]
async fn api_key_refusal_is_content_filter() {
    let frames = vec![
        event(
            serde_json::json!({"type":"response.refusal.delta","delta":"I can't help with that."}),
        ),
        completed("completed"),
    ];
    let (base_url, _request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 30);
    let response = provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("bad")]),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("stream completes");
    assert_eq!(response.finish_reason, FinishReason::ContentFilter);
}

/// JSON-object mode and temperature reach the API-key wire (the Chat
/// Completions path supported both); reasoning models never get a
/// temperature.
#[tokio::test]
async fn api_key_honors_json_object_mode_and_sampling() {
    let (base_url, request) = serve_sse(vec![text_delta("{}"), completed("completed")]).await;
    let provider = api_key_provider(&base_url, "gpt-4.1", 30);
    let mut completion =
        CompletionRequest::new(vec![ChatMessage::user("json please")]).with_temperature(0.3);
    completion.response_format = Some(crate::provider::CompletionResponseFormat::JsonObject);
    provider
        .complete_streaming(completion, Arc::new(DeltaRecorder::default()))
        .await
        .expect("json-object request succeeds");
    let body = captured(request).await.body;
    assert_eq!(body["text"]["format"]["type"], "json_object", "{body}");
    assert!(
        (body["temperature"].as_f64().unwrap_or_default() - 0.3).abs() < 1e-6,
        "{body}"
    );
    assert!(
        body.get("reasoning").is_none(),
        "non-reasoning models reject `reasoning`: {body}"
    );
    assert!(
        body["text"].get("verbosity").is_none(),
        "non-reasoning models reject `text.verbosity`: {body}"
    );

    let (base_url, request) = serve_sse(vec![text_delta("ok"), completed("completed")]).await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 30);
    provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hi")]).with_temperature(0.3),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("request succeeds");
    let body = captured(request).await.body;
    assert!(body.get("temperature").is_none(), "{body}");
}

/// The Codex (ChatGPT backend) route keeps rejecting JSON-object mode and
/// never sends API-key sampling fields — its wire shape is unchanged.
#[tokio::test]
async fn codex_route_keeps_its_wire_shape() {
    let provider = OpenAiCodexProvider::new(
        "gpt-5.5",
        "http://127.0.0.1:9",
        &crate::codex_test_helpers::make_test_jwt("acct_test"),
        30,
    )
    .expect("codex provider");
    let mut completion = CompletionRequest::new(vec![ChatMessage::user("json")]);
    completion.response_format = Some(crate::provider::CompletionResponseFormat::JsonObject);
    let error = provider
        .complete(completion)
        .await
        .expect_err("codex route rejects json-object mode");
    assert!(
        matches!(error, LlmError::InvalidRequest { .. }),
        "{error:?}"
    );

    let mut body = serde_json::json!({"model": "gpt-4.1"});
    provider.apply_api_key_request_options(&mut body, Some(10), Some(0.2), None);
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("temperature").is_none());
    assert_eq!(provider.provider_id(), "openai_codex");
}

/// Long generations stream past the idle window while events keep arriving;
/// there is no total wall-clock cap.
#[tokio::test]
async fn api_key_stream_outlives_the_idle_window_while_progress_arrives() {
    let mut frames: Vec<ScriptedFrame> = (0..6)
        .map(|chunk| text_delta(&format!("c{chunk} ")).delayed(Duration::from_millis(400)))
        .collect();
    frames.push(completed("completed"));
    let (base_url, _request) = serve_sse(frames).await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 1);
    let sink = Arc::new(DeltaRecorder::default());
    let started = Instant::now();
    let response = provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("long")]),
            sink.clone(),
        )
        .await
        .expect("a progressing stream is not timed out");
    assert!(started.elapsed() > Duration::from_secs(2));
    assert_eq!(sink.text().len(), 6);
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

/// Operator extra headers (organization / project) reach the wire, and the
/// models endpoint is the platform `/models` with Bearer auth.
#[tokio::test]
async fn api_key_forwards_extra_headers_and_lists_models() {
    let (base_url, request) = serve_sse(vec![text_delta("ok"), completed("completed")]).await;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("openai-organization"),
        reqwest::header::HeaderValue::from_static("org-test"),
    );
    let provider = api_key_provider(&base_url, "gpt-5.5", 30).with_extra_headers(headers.clone());
    provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hi")]),
            Arc::new(DeltaRecorder::default()),
        )
        .await
        .expect("request succeeds");
    assert_eq!(
        captured(request).await.header("openai-organization"),
        Some("org-test")
    );

    let (base_url, request) = serve(
        200,
        "application/json",
        vec![ScriptedFrame::now(
            r#"{"data":[{"id":"gpt-5.5"},{"id":"gpt-4.1"}]}"#,
        )],
    )
    .await;
    let provider = api_key_provider(&base_url, "gpt-5.5", 30).with_extra_headers(headers);
    let models = provider.list_models().await.expect("models listed");
    assert_eq!(models, ["gpt-5.5", "gpt-4.1"]);
    let request = captured(request).await;
    assert_eq!(request.request_line, "GET /models HTTP/1.1");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-test-openai")
    );
}
