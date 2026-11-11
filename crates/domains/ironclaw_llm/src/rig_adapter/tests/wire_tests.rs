use super::*;

#[test]
fn output_schema_rejects_non_object_root_before_dispatch() {
    let mut request = build_rig_request(
        None,
        Vec::new(),
        Vec::new(),
        None,
        None,
        None,
        CacheRetention::None,
    )
    .expect("empty rig request");
    let error = set_output_schema(
        &mut request,
        Some(CompletionResponseFormat::JsonSchema(
            crate::provider::JsonSchemaResponseFormat::strict(
                "suggestions",
                serde_json::json!(["not", "an", "object"]),
            ),
        )),
        "test-provider",
        true,
    )
    .expect_err("a JSON Schema response format must have an object root");

    assert!(matches!(
        error,
        LlmError::InvalidRequest { provider, reason }
            if provider == "test-provider" && reason.contains("must be an object")
    ));
}

#[tokio::test]
async fn tool_completion_request_serializes_the_model_once() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    );
    adapter
        .complete_with_tools(tool_request_with_model_override())
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    assert_single_model_override(&body);
}

#[tokio::test]
async fn completion_request_serializes_native_output_schema() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    )
    .with_structured_output_support(true);
    let mut request = CompletionRequest::new(vec![ChatMessage::user("Return suggestions")]);
    request.response_format = Some(crate::provider::CompletionResponseFormat::JsonSchema(
        crate::provider::JsonSchemaResponseFormat::strict(
            "suggestions",
            serde_json::json!({
                "type": "object",
                "properties": {"items": {"type": "array"}}
            }),
        ),
    ));
    adapter
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(
        body["response_format"]["json_schema"]["name"],
        "suggestions"
    );
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["properties"]["items"]["type"],
        "array"
    );
}

#[tokio::test]
async fn completion_request_serializes_native_json_object_mode() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    )
    .with_json_object_support(true);
    let mut request = CompletionRequest::new(vec![ChatMessage::user("Return an object")]);
    request.response_format = Some(crate::provider::CompletionResponseFormat::JsonObject);
    adapter
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(
        body["response_format"],
        serde_json::json!({"type": "json_object"})
    );
}

/// When the request carries [`crate::provider::PROMPT_CACHE_KEY_METADATA`]
/// and the adapter is marked as an OpenAI-compatible Chat Completions
/// path (`with_prompt_cache_key_support(true)`, matching
/// `create_openai_compat_from_registry`'s wiring), the serialized body
/// carries a top-level `prompt_cache_key` with that exact value.
#[tokio::test]
async fn completion_request_serializes_prompt_cache_key_when_metadata_present() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    )
    .with_prompt_cache_key_support(true);
    let mut request = CompletionRequest::new(vec![ChatMessage::user("hello")]);
    request.metadata.insert(
        crate::provider::PROMPT_CACHE_KEY_METADATA.to_string(),
        "thread-cache-key-abc".to_string(),
    );
    adapter
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(body["prompt_cache_key"], "thread-cache-key-abc");
}

/// A routed `provider::model::effort` turn: the router strips the model
/// to its plain id and the adapter spells the effort in the wire body.
async fn routed_effort_body(
    build: impl FnOnce(&str) -> Arc<dyn LlmProvider>,
    turn_model: &str,
) -> serde_json::Value {
    struct NoRoutes;
    #[async_trait]
    impl crate::provider_routing::RoutedProviderSource for NoRoutes {
        async fn provider(&self, id: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
            Err(LlmError::RequestFailed {
                provider: id.to_string(),
                reason: "no routes in this test".to_string(),
            })
        }
    }
    let (base_url, captured_body) = capture_one_http_request().await;
    let router = crate::provider_routing::ProviderRouter::new(build(&base_url), Arc::new(NoRoutes));
    let mut request = CompletionRequest::new(vec![ChatMessage::user("hello")]);
    request.model = Some(turn_model.to_string());
    router
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");
    let body = captured_request_body(captured_body).await;
    serde_json::from_str(&body).expect("request JSON")
}

fn openai_chat(base_url: &str) -> Arc<dyn LlmProvider> {
    use rig::client::CompletionClient;
    let client = rig::providers::openai::Client::builder()
        .api_key("test-key")
        .base_url(base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    Arc::new(
        RigAdapter::new(client.completion_model("gpt-4o"), "gpt-4o")
            .with_provider_id("openai")
            .with_effort_style(crate::reasoning_effort::EffortStyle::OpenAiChat),
    )
}

#[tokio::test]
async fn a_routed_effort_reaches_each_api_protocol_on_the_wire() {
    let body = routed_effort_body(openai_chat, "::gpt-5.2::high").await;
    assert_eq!(body["model"], "gpt-5.2");
    assert_eq!(body["reasoning_effort"], "high");

    let body = routed_effort_body(openai_chat, "::gpt-4o::high").await;
    assert!(
        body.get("reasoning_effort").is_none(),
        "a model without the knob gets no field: {body}"
    );

    let body = routed_effort_body(openai_chat, "::glm-5.1::off").await;
    assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));

    let anthropic = |base_url: &str| -> Arc<dyn LlmProvider> {
        use rig::client::CompletionClient;
        let client: rig::providers::anthropic::Client =
            rig::providers::anthropic::Client::builder()
                .api_key("test-key")
                .base_url(base_url)
                .build()
                .expect("build Anthropic client");
        Arc::new(
            RigAdapter::new(
                client.completion_model("claude-opus-5-5"),
                "claude-opus-5-5",
            )
            .with_provider_id("anthropic")
            .with_default_max_tokens(8192)
            .with_effort_style(crate::reasoning_effort::EffortStyle::Anthropic),
        )
    };
    let body = routed_effort_body(anthropic, "::claude-opus-5-5::max").await;
    assert_eq!(
        body["output_config"],
        serde_json::json!({ "effort": "max" })
    );
    assert_eq!(body["thinking"], serde_json::json!({ "type": "adaptive" }));

    let gemini = |base_url: &str| -> Arc<dyn LlmProvider> {
        use rig::client::CompletionClient;
        let client: rig::providers::gemini::Client = rig::providers::gemini::Client::builder()
            .api_key("test-key")
            .base_url(base_url)
            .build()
            .expect("build Gemini client");
        Arc::new(
            RigAdapter::new(client.completion_model("gemini-4-argon"), "gemini-4-argon")
                .with_provider_id("gemini")
                .with_effort_style(crate::reasoning_effort::EffortStyle::Gemini),
        )
    };
    let body = routed_effort_body(gemini, "::gemini-4-argon::low").await;
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        serde_json::json!({ "thinkingLevel": "low" })
    );
}

/// Absent metadata must not synthesize the field.
#[tokio::test]
async fn completion_request_omits_prompt_cache_key_when_metadata_absent() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    )
    .with_prompt_cache_key_support(true);
    let request = CompletionRequest::new(vec![ChatMessage::user("hello")]);
    adapter
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "no prompt_cache_key may be emitted when no metadata was supplied: {body}"
    );
}

/// The kill switch: listing `"prompt_cache_key"` in `unsupported_params`
/// suppresses the field even though metadata carries a value and the
/// adapter is otherwise OpenAI-compatible-Chat-Completions-shaped.
#[tokio::test]
async fn completion_request_omits_prompt_cache_key_when_in_unsupported_params() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build OpenAI-compatible client")
        .completions_api();
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    )
    .with_prompt_cache_key_support(true)
    .with_unsupported_params(vec!["prompt_cache_key".to_string()]);
    let mut request = CompletionRequest::new(vec![ChatMessage::user("hello")]);
    request.metadata.insert(
        crate::provider::PROMPT_CACHE_KEY_METADATA.to_string(),
        "thread-cache-key-abc".to_string(),
    );
    adapter
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "unsupported_params must suppress prompt_cache_key: {body}"
    );
}

/// `prompt_cache_key` is an OpenAI concept. An Anthropic-protocol
/// request (`prompt_cache_key_supported` stays `false` by default —
/// `create_anthropic_from_registry` never calls
/// `with_prompt_cache_key_support`) must not gain a top-level
/// `prompt_cache_key`, and its `cache_control` markers must remain
/// unchanged.
#[tokio::test]
async fn anthropic_request_does_not_gain_prompt_cache_key() {
    use rig::client::CompletionClient;
    use rig::providers::anthropic;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = anthropic::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build Anthropic client");
    let model = client.completion_model("claude-opus-4-6");
    let adapter =
        RigAdapter::new(model, "claude-opus-4-6").with_cache_retention(CacheRetention::Short);

    let mut request = CompletionRequest::new(vec![ChatMessage::user("hello")]);
    request.metadata.insert(
        crate::provider::PROMPT_CACHE_KEY_METADATA.to_string(),
        "thread-cache-key-abc".to_string(),
    );
    adapter
        .complete(request)
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "Anthropic-protocol requests must never carry prompt_cache_key: {body}"
    );
    assert_eq!(body["cache_control"]["type"], "ephemeral");
}

#[derive(Clone)]
struct CacheableCompletionModel;

impl CompletionModel for CacheableCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = ();
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self
    }

    async fn completion(
        &self,
        _request: RigRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
        Ok(rig::completion::CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text("cached response")),
            usage: RigUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            raw_response: serde_json::json!({}),
            message_id: None,
        })
    }

    async fn stream(
        &self,
        _request: RigRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        Err(CompletionError::ProviderError(
            "stream unsupported".to_string(),
        ))
    }
}

#[tokio::test]
async fn cached_provider_separates_normalized_rig_model_overrides() {
    use crate::response_cache::{CachedProvider, ResponseCacheConfig};

    let adapter = Arc::new(RigAdapter::new(
        CacheableCompletionModel,
        "configured-model",
    ));
    let cached = CachedProvider::new(adapter, ResponseCacheConfig::default());

    cached
        .complete(
            CompletionRequest::new(vec![ChatMessage::user("same prompt")])
                .with_model(" override-a "),
        )
        .await
        .expect("first completion succeeds");
    cached
        .complete(
            CompletionRequest::new(vec![ChatMessage::user("same prompt")]).with_model("override-b"),
        )
        .await
        .expect("second completion succeeds");
    cached
        .complete(
            CompletionRequest::new(vec![ChatMessage::user("same prompt")]).with_model("override-a"),
        )
        .await
        .expect("normalized override reuses its cache entry");

    assert_eq!(cached.len(), 2);
    assert_eq!(cached.total_hits(), 1);
}

#[tokio::test]
async fn deepseek_tool_completion_request_serializes_the_model_once() {
    use rig::client::CompletionClient;
    use rig::providers::deepseek;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = deepseek::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build DeepSeek client");
    let adapter = RigAdapter::new(
        client.completion_model("configured-model"),
        "configured-model",
    );
    adapter
        .complete_with_tools(tool_request_with_model_override())
        .await
        .expect_err("capture server intentionally rejects the request");

    let body = captured_request_body(captured_body).await;
    assert_single_model_override(&body);
}
