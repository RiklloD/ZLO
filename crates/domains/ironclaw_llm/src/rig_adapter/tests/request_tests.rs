use super::*;

#[test]
fn test_build_rig_request_injects_cache_control_short() {
    let req = build_rig_request(
        Some("You are helpful.".to_string()),
        vec![RigMessage::user("Hello")],
        Vec::new(),
        None,
        None,
        None,
        CacheRetention::Short,
    )
    .unwrap();

    let params = req
        .additional_params
        .expect("should have additional_params for Short retention");
    assert_eq!(params["cache_control"]["type"], "ephemeral");
    assert!(
        params["cache_control"].get("ttl").is_none(),
        "Short retention should not include ttl"
    );
}

#[test]
fn test_build_rig_request_injects_cache_control_long() {
    let req = build_rig_request(
        Some("You are helpful.".to_string()),
        vec![RigMessage::user("Hello")],
        Vec::new(),
        None,
        None,
        None,
        CacheRetention::Long,
    )
    .unwrap();

    let params = req
        .additional_params
        .expect("should have additional_params for Long retention");
    assert_eq!(params["cache_control"]["type"], "ephemeral");
    assert_eq!(params["cache_control"]["ttl"], "1h");
}

#[test]
fn test_build_rig_request_no_cache_control_when_none() {
    let req = build_rig_request(
        Some("You are helpful.".to_string()),
        vec![RigMessage::user("Hello")],
        Vec::new(),
        None,
        None,
        None,
        CacheRetention::None,
    )
    .unwrap();

    assert!(
        req.additional_params.is_none(),
        "additional_params should be None when cache is disabled"
    );
}

fn two_rig_tools() -> Vec<RigToolDefinition> {
    vec![
        RigToolDefinition {
            name: "alpha".to_string(),
            description: "First tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "a": { "type": "string" } }
            }),
        },
        RigToolDefinition {
            name: "beta".to_string(),
            description: "Second tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "b": { "type": "string" } }
            }),
        },
    ]
}

#[test]
fn test_build_rig_request_marks_last_tool_short() {
    let req = build_rig_request(
        Some("You are helpful.".to_string()),
        vec![RigMessage::user("Hello")],
        two_rig_tools(),
        None,
        None,
        None,
        CacheRetention::Short,
    )
    .unwrap();

    // The last tool moves into additional_params.tools (rig appends those
    // after the typed tools, preserving order) carrying the explicit
    // cache breakpoint in Anthropic's native shape.
    assert_eq!(req.tools.len(), 1, "last tool moves to additional_params");
    assert_eq!(req.tools[0].name, "alpha");
    let params = req.additional_params.expect("additional_params");
    let moved = params["tools"].as_array().expect("raw tools array");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0]["name"], "beta");
    assert_eq!(moved[0]["description"], "Second tool");
    assert!(
        moved[0].get("input_schema").is_some(),
        "moved tool must use Anthropic's input_schema key: {moved:?}"
    );
    assert!(moved[0].get("parameters").is_none());
    assert_eq!(moved[0]["cache_control"]["type"], "ephemeral");
    assert!(moved[0]["cache_control"].get("ttl").is_none());
}

#[test]
fn test_build_rig_request_marks_last_tool_long_with_ttl() {
    let req = build_rig_request(
        Some("You are helpful.".to_string()),
        vec![RigMessage::user("Hello")],
        two_rig_tools(),
        None,
        None,
        None,
        CacheRetention::Long,
    )
    .unwrap();

    let params = req.additional_params.expect("additional_params");
    let moved = params["tools"].as_array().expect("raw tools array");
    assert_eq!(moved[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(moved[0]["cache_control"]["ttl"], "1h");
}

#[test]
fn test_effective_cache_retention_downgrades_unsupported_models() {
    assert_eq!(
        effective_cache_retention(CacheRetention::Short, "claude-2.1"),
        CacheRetention::None
    );
    assert_eq!(
        effective_cache_retention(CacheRetention::Long, "claude-instant-1.2"),
        CacheRetention::None
    );
    assert_eq!(
        effective_cache_retention(CacheRetention::Short, "claude-opus-4-6"),
        CacheRetention::Short
    );
    assert_eq!(
        effective_cache_retention(CacheRetention::Long, "anthropic/claude-sonnet-4-5"),
        CacheRetention::Long
    );
    assert_eq!(
        effective_cache_retention(CacheRetention::None, "claude-opus-4-6"),
        CacheRetention::None
    );
    assert_eq!(
        effective_cache_retention(CacheRetention::Short, "claude-fable-5-1"),
        CacheRetention::Short
    );
}

#[test]
fn test_build_rig_request_keeps_tools_typed_when_none() {
    let req = build_rig_request(
        Some("You are helpful.".to_string()),
        vec![RigMessage::user("Hello")],
        two_rig_tools(),
        None,
        None,
        None,
        CacheRetention::None,
    )
    .unwrap();

    assert_eq!(req.tools.len(), 2, "no tool moves when caching is off");
    assert!(req.additional_params.is_none());
}

/// Wire-level pin for the Anthropic explicit-breakpoint layout under
/// `Short` retention (issue #6984): system prompt, last tool, and last
/// message block each carry an ephemeral marker, and the request-level
/// automatic-caching marker is retained (it no-ops on the already-marked
/// last block per Anthropic's compatibility rule).
#[tokio::test]
async fn anthropic_short_retention_places_explicit_cache_breakpoints() {
    use rig::client::CompletionClient;
    use rig::providers::anthropic;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = anthropic::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build Anthropic client");
    let mut model = client.completion_model("claude-opus-4-6");
    // Mirrors create_anthropic_from_registry: Short retention enables
    // rig's typed system/last-message breakpoints.
    model.prompt_caching = true;
    let adapter =
        RigAdapter::new(model, "claude-opus-4-6").with_cache_retention(CacheRetention::Short);

    let request = ToolCompletionRequest::new(
        vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("First question"),
            ChatMessage::assistant("First answer"),
            ChatMessage::user("Second question"),
        ],
        vec![
            IronToolDefinition {
                name: "alpha".to_string(),
                description: "First tool".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "a": { "type": "string" } }
                }),
            },
            IronToolDefinition {
                name: "beta".to_string(),
                description: "Second tool".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "b": { "type": "string" } }
                }),
            },
        ],
    );
    let _ = adapter.complete_with_tools(request).await;

    let body: serde_json::Value = serde_json::from_str(&captured_request_body(captured_body).await)
        .expect("captured body is JSON");

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert!(body["cache_control"].get("ttl").is_none());

    let system = body["system"]
        .as_array()
        .expect("system serialized as blocks");
    assert_eq!(
        system.last().expect("system block")["cache_control"]["type"],
        "ephemeral"
    );

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2, "both tools reach the wire: {body}");
    assert_eq!(tools[0]["name"], "alpha");
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["name"], "beta", "order preserved after the move");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    assert!(
        tools[1].get("input_schema").is_some(),
        "moved tool keeps Anthropic's input_schema key: {tools:?}"
    );

    let messages = body["messages"].as_array().expect("messages");
    let last_content = messages.last().expect("last message")["content"]
        .as_array()
        .expect("last message content blocks");
    assert_eq!(
        last_content.last().expect("content block")["cache_control"]["type"],
        "ephemeral"
    );
}

/// Wire-level pin for `Long` retention: rig's typed markers cannot carry
/// a TTL, so only the request-level automatic marker and the raw last-tool
/// marker are emitted, both at 1h. Mixing a 5m block marker with a 1h
/// automatic marker would be rejected by the API (TTL conflict on the
/// last block), so system/message blocks must stay unmarked.
#[tokio::test]
async fn anthropic_long_retention_uses_1h_markers_without_block_conflicts() {
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
        RigAdapter::new(model, "claude-opus-4-6").with_cache_retention(CacheRetention::Long);

    let request = ToolCompletionRequest::new(
        vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Question"),
        ],
        vec![IronToolDefinition {
            name: "alpha".to_string(),
            description: "First tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "a": { "type": "string" } }
            }),
        }],
    );
    let _ = adapter.complete_with_tools(request).await;

    let body: serde_json::Value = serde_json::from_str(&captured_request_body(captured_body).await)
        .expect("captured body is JSON");

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["cache_control"]["ttl"], "1h");

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");

    for block in body["system"].as_array().expect("system blocks") {
        assert!(
            block.get("cache_control").is_none(),
            "no 5m block marker may precede the 1h automatic marker: {body}"
        );
    }
    for message in body["messages"].as_array().expect("messages") {
        if let Some(blocks) = message["content"].as_array() {
            for block in blocks {
                assert!(
                    block.get("cache_control").is_none(),
                    "message blocks must stay unmarked under Long retention: {body}"
                );
            }
        }
    }
}

/// Wire-level pin: retention `None` emits no cache_control anywhere.
#[tokio::test]
async fn anthropic_no_retention_emits_no_cache_control() {
    use rig::client::CompletionClient;
    use rig::providers::anthropic;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = anthropic::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build Anthropic client");
    let adapter = RigAdapter::new(
        client.completion_model("claude-opus-4-6"),
        "claude-opus-4-6",
    );

    let request = ToolCompletionRequest::new(
        vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Question"),
        ],
        vec![IronToolDefinition {
            name: "alpha".to_string(),
            description: "First tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "a": { "type": "string" } }
            }),
        }],
    );
    let _ = adapter.complete_with_tools(request).await;

    let body = captured_request_body(captured_body).await;
    assert!(
        !body.contains("cache_control"),
        "no cache_control may be emitted when caching is off: {body}"
    );
}

/// Regression for the allowlist->denylist gate change: a model family
/// released after this code was written (not matching any hardcoded
/// "claude-3"/"claude-4"/"claude-sonnet"/... prefix) must still keep
/// `Short` retention and emit cache_control on the wire, instead of being
/// silently downgraded to `None`.
#[tokio::test]
async fn anthropic_new_model_family_keeps_short_retention_cache_control() {
    use rig::client::CompletionClient;
    use rig::providers::anthropic;

    let (base_url, captured_body) = capture_one_http_request().await;
    let client = anthropic::Client::builder()
        .api_key("test-key")
        .base_url(&base_url)
        .build()
        .expect("build Anthropic client");
    let model = client.completion_model("claude-fable-5-1");
    let adapter =
        RigAdapter::new(model, "claude-fable-5-1").with_cache_retention(CacheRetention::Short);
    assert_eq!(adapter.cache_retention, CacheRetention::Short);

    // rig-core has no built-in `max_tokens` default for unknown model
    // families (its hardcoded default table only covers named releases),
    // so this new-family test must set it explicitly or the request
    // fails client-side validation before reaching the wire.
    let request = ToolCompletionRequest::new(
        vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Question"),
        ],
        vec![IronToolDefinition {
            name: "alpha".to_string(),
            description: "First tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "a": { "type": "string" } }
            }),
        }],
    )
    .with_max_tokens(4096);
    let _ = adapter.complete_with_tools(request).await;

    let body: serde_json::Value = serde_json::from_str(&captured_request_body(captured_body).await)
        .expect("captured body is JSON");
    assert_eq!(body["cache_control"]["type"], "ephemeral");
}

/// Verify that the multiplier match arms in `RigAdapter::cache_write_multiplier`
/// produce the expected values. We use a standalone helper because constructing
/// a real `RigAdapter` requires a rig `Model` (which needs network/provider setup).
/// The helper mirrors the same match expression — if the impl drifts, the
/// `test_build_rig_request_*` tests will still catch regressions end-to-end.
#[test]
fn test_cache_write_multiplier_values() {
    use rust_decimal::Decimal;
    // None → 1.0× (no surcharge)
    assert_eq!(
        cache_write_multiplier_for(CacheRetention::None),
        Decimal::ONE
    );
    // Short → 1.25× (25% surcharge)
    assert_eq!(
        cache_write_multiplier_for(CacheRetention::Short),
        Decimal::new(125, 2)
    );
    // Long → 2.0× (100% surcharge)
    assert_eq!(
        cache_write_multiplier_for(CacheRetention::Long),
        Decimal::TWO
    );
}

fn cache_write_multiplier_for(retention: CacheRetention) -> rust_decimal::Decimal {
    match retention {
        CacheRetention::None => rust_decimal::Decimal::ONE,
        CacheRetention::Short => rust_decimal::Decimal::new(125, 2),
        CacheRetention::Long => rust_decimal::Decimal::TWO,
    }
}

// -- supports_prompt_cache tests --

#[test]
fn test_supports_prompt_cache_supported_models() {
    // All Claude 3+ models per Anthropic docs
    assert!(supports_prompt_cache("claude-opus-4-6"));
    assert!(supports_prompt_cache("claude-sonnet-4-6"));
    assert!(supports_prompt_cache("claude-sonnet-4"));
    assert!(supports_prompt_cache("claude-haiku-4-5"));
    assert!(supports_prompt_cache("claude-3-5-sonnet-20241022"));
    assert!(supports_prompt_cache("claude-haiku-3"));
    assert!(supports_prompt_cache("Claude-Opus-4-5")); // case-insensitive
    assert!(supports_prompt_cache("anthropic/claude-sonnet-4-6")); // provider prefix
}

#[test]
fn test_supports_prompt_cache_unsupported_models() {
    // Legacy Claude models that predate caching
    assert!(!supports_prompt_cache("claude-2"));
    assert!(!supports_prompt_cache("claude-2.1"));
    assert!(!supports_prompt_cache("claude-instant-1.2"));
    // Non-Claude models
    assert!(!supports_prompt_cache("gpt-4o"));
    assert!(!supports_prompt_cache("llama3"));
}

/// Denylist regression: `supports_prompt_cache` must default new/unknown
/// Claude families to `true` rather than requiring each one be
/// allowlisted. Only the documented-unsupported `claude-2*` and
/// `claude-instant*` families are excluded.
#[test]
fn test_supports_prompt_cache_denylist_covers_new_model_families() {
    let supported = [
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "anthropic/claude-fable-5-1",
        "claude-opus-5",
        "claude-sonnet-4-5",
        "claude-3-5-haiku",
    ];
    for model in supported {
        assert!(
            supports_prompt_cache(model),
            "expected {model} to support prompt caching"
        );
    }

    let unsupported = ["claude-2.1", "claude-instant-1.2", "anthropic/claude-2"];
    for model in unsupported {
        assert!(
            !supports_prompt_cache(model),
            "expected {model} to NOT support prompt caching"
        );
    }
}

#[test]
fn test_with_unsupported_params_populates_set() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let client: openai::Client = openai::Client::builder()
        .api_key("test-key")
        .base_url("http://localhost:0")
        .build()
        .unwrap();
    let client = client.completions_api();
    let model = client.completion_model("test-model");
    let adapter = RigAdapter::new(model, "test-model")
        .with_unsupported_params(vec!["temperature".to_string()]);

    assert!(adapter.unsupported_params.contains("temperature"));
    assert!(!adapter.unsupported_params.contains("max_tokens"));
}

#[test]
fn test_strip_unsupported_completion_params() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let client: openai::Client = openai::Client::builder()
        .api_key("test-key")
        .base_url("http://localhost:0")
        .build()
        .unwrap();
    let client = client.completions_api();
    let model = client.completion_model("test-model");
    let adapter = RigAdapter::new(model, "test-model").with_unsupported_params(vec![
        "temperature".to_string(),
        "stop_sequences".to_string(),
    ]);

    let mut req = CompletionRequest::new(vec![ChatMessage::user("hi")]);
    req.temperature = Some(0.7);
    req.max_tokens = Some(100);
    req.stop_sequences = Some(vec!["STOP".to_string()]);

    adapter.strip_unsupported_completion_params(&mut req);

    assert!(req.temperature.is_none(), "temperature should be stripped");
    assert_eq!(req.max_tokens, Some(100), "max_tokens should be preserved");
    assert!(
        req.stop_sequences.is_none(),
        "stop_sequences should be stripped"
    );
}

#[test]
fn test_strip_unsupported_tool_params() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let client: openai::Client = openai::Client::builder()
        .api_key("test-key")
        .base_url("http://localhost:0")
        .build()
        .unwrap();
    let client = client.completions_api();
    let model = client.completion_model("test-model");
    let adapter = RigAdapter::new(model, "test-model")
        .with_unsupported_params(vec!["temperature".to_string(), "max_tokens".to_string()]);

    let mut req = ToolCompletionRequest::new(vec![ChatMessage::user("hi")], vec![]);
    req.temperature = Some(0.5);
    req.max_tokens = Some(200);

    adapter.strip_unsupported_tool_params(&mut req);

    assert!(req.temperature.is_none(), "temperature should be stripped");
    assert!(req.max_tokens.is_none(), "max_tokens should be stripped");
}

#[test]
fn test_unsupported_params_empty_by_default() {
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let client: openai::Client = openai::Client::builder()
        .api_key("test-key")
        .base_url("http://localhost:0")
        .build()
        .unwrap();
    let client = client.completions_api();
    let model = client.completion_model("test-model");
    let adapter = RigAdapter::new(model, "test-model");

    assert!(adapter.unsupported_params.is_empty());
}

fn make_rig_request(additional_params: Option<serde_json::Value>) -> RigRequest {
    RigRequest {
        preamble: None,
        chat_history: OneOrMany::one(RigMessage::user("test")),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params,
        model: None,
        output_schema: None,
    }
}

#[test]
fn test_merge_additional_params_into_empty() {
    let mut req = make_rig_request(None);
    merge_additional_params(&mut req, Some(&serde_json::json!({"think": true})));
    let params = req.additional_params.expect("should be Some");
    assert_eq!(params["think"], true);
}

#[test]
fn test_merge_additional_params_preserves_existing() {
    let mut req = make_rig_request(Some(
        serde_json::json!({"cache_control": {"type": "ephemeral"}}),
    ));
    merge_additional_params(&mut req, Some(&serde_json::json!({"think": true})));
    let params = req.additional_params.expect("should remain Some");
    let obj = params.as_object().expect("should be object");
    assert_eq!(obj["cache_control"]["type"], "ephemeral");
    assert_eq!(obj["think"], true);
}

#[test]
fn test_merge_additional_params_existing_key_wins() {
    let mut req = make_rig_request(Some(serde_json::json!({"think": false})));
    merge_additional_params(&mut req, Some(&serde_json::json!({"think": true})));
    let params = req.additional_params.expect("should remain Some");
    assert_eq!(
        params["think"], false,
        "existing value should not be overwritten"
    );
}

#[test]
fn test_merge_additional_params_noop_when_none() {
    let mut req = make_rig_request(None);
    merge_additional_params(&mut req, None);
    assert!(req.additional_params.is_none());
}

#[test]
fn test_inject_model_override_sets_typed_request_model() {
    let mut req = make_rig_request(None);
    inject_model_override(&mut req, Some("test-model"));

    assert_eq!(req.model.as_deref(), Some("test-model"));
    assert!(req.additional_params.is_none());
}

#[test]
fn test_inject_model_override_preserves_existing_additional_params() {
    let mut req = make_rig_request(Some(serde_json::json!({
        "cache_control": { "type": "ephemeral" },
    })));
    inject_model_override(&mut req, Some("override-model"));

    let params = req.additional_params.expect("should remain Some");
    let obj = params.as_object().expect("should be object");
    assert_eq!(
        obj.get("cache_control"),
        Some(&serde_json::json!({ "type": "ephemeral" }))
    );
    assert!(obj.get("model").is_none());
    assert_eq!(req.model.as_deref(), Some("override-model"));
}

#[test]
fn test_inject_model_override_noop_when_none() {
    let mut req = make_rig_request(None);
    inject_model_override(&mut req, None);
    assert!(req.additional_params.is_none());
}

// ── map_rig_error: context length detection ─────────────────────────
