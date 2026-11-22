//! Factory-level tests: what `create_registry_provider_inner` actually puts
//! on the wire for each direct API-key protocol — native streaming routing
//! and provider-native tool schema policy. Driven through the factory (not a
//! hand-built adapter) so a factory that forgets a builder call fails here.

use std::sync::Arc;

use crate::config::{CacheRetention, RegistryProviderConfig};
use crate::provider::{ChatMessage, CompletionRequest, ToolCompletionRequest, ToolDefinition};
use crate::registry::ProviderProtocol;
use crate::sse_test_server::{DeltaRecorder, ScriptedFrame, captured, serve, serve_sse};

fn config(
    protocol: ProviderProtocol,
    provider_id: &str,
    base_url: &str,
    model: &str,
) -> RegistryProviderConfig {
    RegistryProviderConfig::generic(
        protocol,
        provider_id,
        Some(secrecy::SecretString::from("test-key".to_string())),
        base_url,
        model,
    )
}

fn tool_with_optional_field() -> ToolDefinition {
    ToolDefinition {
        name: "search".to_string(),
        description: "Search".to_string(),
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

/// Find the first object in `value` whose `properties` declares `limit`.
fn find_tool_schema(value: &serde_json::Value) -> Option<&serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            if map
                .get("properties")
                .and_then(|props| props.get("limit"))
                .is_some()
            {
                return Some(value);
            }
            map.values().find_map(find_tool_schema)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_tool_schema),
        _ => None,
    }
}

fn required_names(schema: &serde_json::Value) -> Vec<String> {
    schema["required"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_ascii_lowercase))
                .collect()
        })
        .unwrap_or_default()
}

async fn captured_tool_schema(config: RegistryProviderConfig) -> serde_json::Value {
    let (base_url, request) = serve(
        400,
        "application/json",
        vec![ScriptedFrame::now(
            r#"{"error":{"message":"capture only"}}"#,
        )],
    )
    .await;
    let mut config = config;
    config.base_url = base_url;
    let provider =
        crate::create_registry_provider_inner(&config, 5).expect("factory builds the provider");
    #[expect(
        clippy::let_underscore_must_use,
        reason = "only the captured request body is asserted; the canned 400 reply is irrelevant"
    )]
    let _ = provider
        .complete_with_tools(ToolCompletionRequest::new(
            vec![ChatMessage::user("search")],
            vec![tool_with_optional_field()],
        ))
        .await;
    let body = captured(request).await.body;
    find_tool_schema(&body)
        .cloned()
        .unwrap_or_else(|| panic!("tool schema on the wire: {body}"))
}

/// Non-OpenAI tool APIs get the flatten-only policy: the optional `limit`
/// stays optional instead of becoming required-nullable strict mode.
#[tokio::test]
async fn non_openai_factories_send_native_optional_tool_fields() {
    for (protocol, provider_id, model) in [
        (ProviderProtocol::Ollama, "ollama", "llama3.2"),
        (ProviderProtocol::Gemini, "gemini", "gemini-2.5-pro"),
        (
            ProviderProtocol::OpenRouter,
            "openrouter",
            "anthropic/claude-sonnet-4-6",
        ),
        (ProviderProtocol::Anthropic, "anthropic", "claude-opus-4-6"),
    ] {
        let schema = captured_tool_schema(config(protocol, provider_id, "", model)).await;
        assert_eq!(
            required_names(&schema),
            ["query"],
            "{provider_id}: optional fields must stay optional: {schema}"
        );
        assert_ne!(
            schema["properties"]["limit"]["type"],
            serde_json::json!(["integer", "null"]),
            "{provider_id}: no strict-mode nullable rewrite: {schema}"
        );
    }
}

/// OpenAI strict-mode targets keep the strict policy: every field required,
/// optional ones nullable.
#[tokio::test]
async fn openai_strict_targets_keep_required_nullable_schemas() {
    for (protocol, provider_id, model) in [
        (ProviderProtocol::OpenAiCompletions, "groq", "llama-3.3-70b"),
        (ProviderProtocol::OpenRouter, "openrouter", "openai/gpt-5"),
    ] {
        let schema = captured_tool_schema(config(protocol, provider_id, "", model)).await;
        let mut required = required_names(&schema);
        required.sort();
        assert_eq!(required, ["limit", "query"], "{provider_id}: {schema}");
    }
}

/// `ANTHROPIC_API_KEY` goes to the native Messages API provider: a streamed
/// call is a real SSE stream whose deltas reach the sink.
#[tokio::test]
async fn anthropic_api_key_factory_streams_natively() {
    let frames = vec![
        ScriptedFrame::event(
            "message_start",
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":1}}}),
        ),
        ScriptedFrame::event(
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"streamed"}}),
        ),
        ScriptedFrame::event(
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        ),
    ];
    let (base_url, request) = serve_sse(frames).await;
    let mut config = config(
        ProviderProtocol::Anthropic,
        "anthropic",
        &base_url,
        "claude-opus-4-6",
    );
    config.cache_retention = CacheRetention::Short;
    let provider = crate::create_registry_provider_inner(&config, 5).expect("factory");
    let sink = Arc::new(DeltaRecorder::default());
    let response = provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hi")]),
            sink.clone(),
        )
        .await
        .expect("native stream");
    let request = captured(request).await;
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.header("x-api-key"), Some("test-key"));
    assert_eq!(sink.text(), ["streamed"]);
    assert_eq!(response.content, "streamed");
    assert_eq!(provider.provider_id(), "anthropic");
}

/// `OPENAI_API_KEY` on the standard endpoint goes to the Responses API; a
/// custom base URL, or any other provider reusing the OpenAI protocol,
/// keeps Chat Completions compatibility.
#[test]
fn openai_responses_api_is_gated_to_the_standard_endpoint() {
    let gate = |provider_id: &str, base_url: &str| {
        crate::openai_responses_api_base(&config(
            ProviderProtocol::OpenAiCompletions,
            provider_id,
            base_url,
            "gpt-5.5",
        ))
    };
    for base in [
        "",
        "https://api.openai.com/v1",
        "https://api.openai.com/v1/",
        "https://api.openai.com",
        "  https://API.openai.com/v1  ",
    ] {
        assert_eq!(
            gate("openai", base),
            Some("https://api.openai.com/v1"),
            "{base:?}"
        );
    }
    assert_eq!(gate("openai", "https://my-proxy.example.com/v1"), None);
    assert_eq!(gate("openai", "http://127.0.0.1:8080"), None);
    assert_eq!(gate("groq", ""), None);
    assert_eq!(gate("openai_compatible", ""), None);
}

/// The Responses factory (the one the gate selects) builds the API-key
/// route: Bearer key, operator headers, unsupported-param stripping, native
/// SSE streaming, `openai` identity.
#[tokio::test]
async fn openai_responses_factory_streams_with_operator_config() {
    let frames = vec![
        ScriptedFrame::data(serde_json::json!({"type":"response.output_text.delta","delta":"hi"})),
        ScriptedFrame::data(
            serde_json::json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}),
        ),
    ];
    let (base_url, request) = serve_sse(frames).await;
    let mut config = config(ProviderProtocol::OpenAiCompletions, "openai", "", "gpt-4.1")
        .with_extra_headers(vec![("OpenAI-Project".to_string(), "proj_1".to_string())]);
    config.unsupported_params = vec!["temperature".to_string()];
    let provider = crate::create_openai_responses_from_registry(&config, &base_url, 5)
        .expect("responses factory");
    let sink = Arc::new(DeltaRecorder::default());
    provider
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hi")]).with_temperature(0.5),
            sink.clone(),
        )
        .await
        .expect("native stream");
    let request = captured(request).await;
    assert_eq!(request.request_line, "POST /responses HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert_eq!(request.header("openai-project"), Some("proj_1"));
    assert!(
        request.body.get("temperature").is_none(),
        "unsupported_params strips temperature: {}",
        request.body
    );
    assert_eq!(sink.text(), ["hi"]);
    assert_eq!(provider.provider_id(), "openai");
}

async fn captured_openrouter_body(model: &str, retention: CacheRetention) -> serde_json::Value {
    let (base_url, request) = serve(
        400,
        "application/json",
        vec![ScriptedFrame::now(
            r#"{"error":{"message":"capture only"}}"#,
        )],
    )
    .await;
    let mut config = config(ProviderProtocol::OpenRouter, "openrouter", &base_url, model);
    config.cache_retention = retention;
    let provider = crate::create_registry_provider_inner(&config, 5).expect("openrouter");
    #[expect(
        clippy::let_underscore_must_use,
        reason = "only the captured request body is asserted"
    )]
    let _ = provider
        .complete_with_tools(ToolCompletionRequest::new(
            vec![ChatMessage::system("sys"), ChatMessage::user("hi")],
            vec![tool_with_optional_field()],
        ))
        .await;
    captured(request).await.body
}

/// OpenRouter `anthropic/*` models get Anthropic's top-level automatic
/// caching marker (1h TTL under `Long`); the OpenAI-shaped tools array is
/// untouched; other upstreams get no marker.
#[tokio::test]
async fn openrouter_anthropic_models_send_automatic_cache_control() {
    let body = captured_openrouter_body("anthropic/claude-sonnet-4-6", CacheRetention::Short).await;
    assert_eq!(
        body["cache_control"],
        serde_json::json!({"type":"ephemeral"}),
        "{body}"
    );
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1, "{body}");
    assert_eq!(tools[0]["type"], "function", "{body}");

    let body = captured_openrouter_body("anthropic/claude-opus-4-6", CacheRetention::Long).await;
    assert_eq!(
        body["cache_control"],
        serde_json::json!({"type":"ephemeral","ttl":"1h"}),
        "{body}"
    );

    for (model, retention) in [
        ("openai/gpt-5", CacheRetention::Short),
        ("anthropic/claude-sonnet-4-6", CacheRetention::None),
        ("anthropic/claude-2.1", CacheRetention::Short),
    ] {
        let body = captured_openrouter_body(model, retention).await;
        assert!(body.get("cache_control").is_none(), "{model}: {body}");
    }
}

/// rig-core's streams are not lossless for any of these protocols (the
/// shared OpenAI-compatible, OpenRouter, DeepSeek and Gemini drivers yield a
/// synthesized terminal frame after EOF and drop the finish reason; Ollama's
/// NDJSON reader splits on raw chunk boundaries and discards error bodies),
/// so their streamed calls stay on the buffered fallback: no streaming
/// request, no deltas.
#[tokio::test]
async fn rig_backed_factories_stay_buffered_until_their_streams_are_lossless() {
    for (protocol, provider_id, model) in [
        (ProviderProtocol::Ollama, "ollama", "llama3.2"),
        (ProviderProtocol::Gemini, "gemini", "gemini-2.5-pro"),
        (ProviderProtocol::OpenRouter, "openrouter", "openai/gpt-5"),
        (ProviderProtocol::DeepSeek, "deepseek", "deepseek-chat"),
        (
            ProviderProtocol::OpenAiCompletions,
            "openai_compatible",
            "local-model",
        ),
    ] {
        let (base_url, request) = serve(
            400,
            "application/json",
            vec![ScriptedFrame::now(
                r#"{"error":{"message":"capture only"}}"#,
            )],
        )
        .await;
        let config = config(protocol, provider_id, &base_url, model);
        let provider = crate::create_registry_provider_inner(&config, 5).expect("factory");
        let sink = Arc::new(DeltaRecorder::default());
        #[expect(
            clippy::let_underscore_must_use,
            reason = "only the captured request shape is asserted"
        )]
        let _ = provider
            .complete_streaming(
                CompletionRequest::new(vec![ChatMessage::user("hi")]),
                sink.clone(),
            )
            .await;
        let request = captured(request).await;
        assert_ne!(
            request.body["stream"], true,
            "{provider_id}: {}",
            request.body
        );
        assert!(
            !request.request_line.contains("streamGenerateContent"),
            "{provider_id}: {}",
            request.request_line
        );
        assert!(sink.text().is_empty(), "{provider_id}");
    }
}
