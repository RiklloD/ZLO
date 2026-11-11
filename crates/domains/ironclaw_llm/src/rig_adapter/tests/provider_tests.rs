use super::*;

/// A rig model that replays a caller-supplied raw provider body alongside
/// a fixed assistant choice, so the adapter's public entry points can be
/// driven against a real provider payload.
#[derive(Clone)]
struct ReplayingCompletionModel {
    choice: OneOrMany<AssistantContent>,
    raw_response: serde_json::Value,
}

impl CompletionModel for ReplayingCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = StubStreamingResponse;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unimplemented!("constructed directly in tests")
    }

    async fn completion(
        &self,
        _request: RigRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
        Ok(rig::completion::CompletionResponse {
            choice: self.choice.clone(),
            usage: RigUsage::new(),
            raw_response: self.raw_response.clone(),
            message_id: None,
        })
    }

    async fn stream(
        &self,
        _request: RigRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        Err(CompletionError::ProviderError(
            "streaming path must not be used".to_string(),
        ))
    }
}

/// Test through the caller, not just the helper: a `max_tokens` truncation
/// that still emitted a (possibly cut-off) tool call must reach
/// `complete_with_tools`' return value as `Length`. Before #6284 item 8 the
/// adapter reported `ToolUse` and the loop executed the truncated call.
#[tokio::test]
async fn complete_with_tools_reports_provider_truncation_not_tool_use() {
    let adapter = RigAdapter::new(
        ReplayingCompletionModel {
            choice: OneOrMany::one(AssistantContent::tool_call(
                "call_1",
                "search",
                serde_json::json!({"q": "tru"}),
            )),
            raw_response: serde_json::json!({
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": null},
                    "finish_reason": "length",
                }],
            }),
        },
        "gpt-4o",
    );
    let request = ToolCompletionRequest::new(
        vec![ChatMessage::user("search for something")],
        vec![IronToolDefinition {
            name: "search".to_string(),
            description: "Search".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }],
    );

    let response = adapter
        .complete_with_tools(request)
        .await
        .expect("adapter returns the provider response");

    assert_eq!(
        response.finish_reason,
        FinishReason::Length,
        "a max_tokens truncation must not be laundered into ToolUse",
    );
}

/// The same seam for a content-policy block on the plain `complete` path.
#[tokio::test]
async fn complete_reports_provider_content_filter() {
    let adapter = RigAdapter::new(
        ReplayingCompletionModel {
            choice: OneOrMany::one(AssistantContent::text("I can't help with that.")),
            raw_response: serde_json::json!({
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "I can't help with that."},
                    "finish_reason": "content_filter",
                }],
            }),
        },
        "gpt-4o",
    );

    let response = adapter
        .complete(CompletionRequest::new(vec![ChatMessage::user("hi")]))
        .await
        .expect("adapter returns the provider response");

    assert_eq!(
        response.finish_reason,
        FinishReason::ContentFilter,
        "a provider refusal must not be reported as a clean stop",
    );
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StubStreamingResponse {
    input_tokens: u64,
    output_tokens: u64,
}

impl GetTokenUsage for StubStreamingResponse {
    fn token_usage(&self) -> Option<RigUsage> {
        Some(RigUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.input_tokens + self.output_tokens,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }
}

#[derive(Clone)]
struct DispatchCountingCompletionModel {
    completions: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
}

impl CompletionModel for DispatchCountingCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = StubStreamingResponse;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unimplemented!("constructed directly in tests")
    }

    async fn completion(
        &self,
        _request: RigRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
        self.completions.fetch_add(1, Ordering::Relaxed);
        Ok(rig::completion::CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text("dispatched")),
            usage: RigUsage::new(),
            raw_response: serde_json::json!({}),
            message_id: None,
        })
    }

    async fn stream(
        &self,
        _request: RigRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        self.streams.fetch_add(1, Ordering::Relaxed);
        Err(CompletionError::ProviderError(
            "structured-output test must reject before dispatch".to_string(),
        ))
    }
}

#[derive(Clone)]
struct MaxTokensCompletionModel {
    expected_max_tokens: Option<u64>,
}

impl CompletionModel for MaxTokensCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = StubStreamingResponse;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unimplemented!("constructed directly in tests")
    }

    async fn completion(
        &self,
        request: RigRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
        assert_eq!(request.max_tokens, self.expected_max_tokens);
        Ok(rig::completion::CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text("ok")),
            usage: RigUsage::new(),
            raw_response: serde_json::json!({
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop",
                }],
            }),
            message_id: None,
        })
    }

    async fn stream(
        &self,
        _request: RigRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        Err(CompletionError::ProviderError(
            "streaming path must not be used".to_string(),
        ))
    }
}

#[tokio::test]
async fn complete_applies_provider_default_max_tokens() {
    const DEFAULT_MAX_TOKENS: u32 = 8192;
    let adapter = RigAdapter::new(
        MaxTokensCompletionModel {
            expected_max_tokens: Some(u64::from(DEFAULT_MAX_TOKENS)),
        },
        "default-max-tokens",
    )
    .with_default_max_tokens(DEFAULT_MAX_TOKENS);

    let response = adapter
        .complete(CompletionRequest::new(vec![ChatMessage::user("hello")]))
        .await
        .expect("completion uses provider default max_tokens");

    assert_eq!(response.content, "ok");
}

#[tokio::test]
async fn complete_with_tools_preserves_explicit_max_tokens_over_provider_default() {
    let adapter = RigAdapter::new(
        MaxTokensCompletionModel {
            expected_max_tokens: Some(4096),
        },
        "explicit-max-tokens",
    )
    .with_default_max_tokens(8192);
    let request = ToolCompletionRequest::new(vec![ChatMessage::user("hello")], Vec::new())
        .with_max_tokens(4096);

    let response = adapter
        .complete_with_tools(request)
        .await
        .expect("tool completion preserves explicit max_tokens");

    assert_eq!(response.content.as_deref(), Some("ok"));
}

#[derive(Clone)]
struct StreamingOnlyCompletionModel {
    expected_max_tokens: Option<u64>,
}

impl CompletionModel for StreamingOnlyCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = StubStreamingResponse;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self {
            expected_max_tokens: None,
        }
    }

    async fn completion(
        &self,
        _request: RigRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
        Err(CompletionError::ProviderError(
            "non-streaming path must not be used".to_string(),
        ))
    }

    async fn stream(
        &self,
        request: RigRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        assert_eq!(request.max_tokens, self.expected_max_tokens);
        let stream = futures::stream::iter(vec![
            Ok(RawStreamingChoice::Message("Hel".to_string())),
            Ok(RawStreamingChoice::ReasoningDelta {
                id: Some("rsn-stream".to_string()),
                reasoning: "thinking".to_string(),
            }),
            Ok(RawStreamingChoice::Message("lo".to_string())),
            Ok(RawStreamingChoice::ToolCall(RawStreamingToolCall::new(
                "call-1".to_string(),
                "search".to_string(),
                serde_json::json!({"query": "ironclaw"}),
            ))),
            Ok(RawStreamingChoice::FinalResponse(StubStreamingResponse {
                input_tokens: 3,
                output_tokens: 4,
            })),
        ]);
        Ok(StreamingCompletionResponse::stream(Box::pin(stream)))
    }
}

#[tokio::test]
async fn deepseek_and_openrouter_reject_structured_output_before_any_dispatch() {
    for provider in ["deepseek", "openrouter"] {
        let completions = Arc::new(AtomicUsize::new(0));
        let streams = Arc::new(AtomicUsize::new(0));
        let adapter = RigAdapter::new(
            DispatchCountingCompletionModel {
                completions: Arc::clone(&completions),
                streams: Arc::clone(&streams),
            },
            "test-model",
        )
        .with_provider_id(provider)
        .with_structured_output_support(false);
        let structured_format = || {
            crate::provider::CompletionResponseFormat::JsonSchema(
                crate::provider::JsonSchemaResponseFormat::strict(
                    "suggestions",
                    serde_json::json!({"type": "object"}),
                ),
            )
        };

        let mut completion_request =
            CompletionRequest::new(vec![ChatMessage::user("return structured output")]);
        completion_request.response_format = Some(structured_format());
        let error = adapter
            .complete(completion_request)
            .await
            .expect_err("unsupported structured output must fail before dispatch");
        assert!(matches!(
            error,
            LlmError::InvalidRequest { provider: actual, reason }
                if actual == provider && reason.contains("structured output")
        ));

        let mut streaming_request =
            CompletionRequest::new(vec![ChatMessage::user("return structured output")]);
        streaming_request.response_format = Some(structured_format());
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
        let sink = Arc::new(RecordingCompletionStreamSink { sender: delta_tx });
        let error = adapter
            .complete_streaming(streaming_request, sink)
            .await
            .expect_err("unsupported structured streaming must fail before dispatch");
        assert!(matches!(
            error,
            LlmError::InvalidRequest { provider: actual, .. } if actual == provider
        ));

        let mut tool_request = ToolCompletionRequest::new(
            vec![ChatMessage::user("return structured output")],
            Vec::new(),
        );
        tool_request.response_format = Some(structured_format());
        let error = adapter
            .complete_with_tools(tool_request)
            .await
            .expect_err("unsupported tool structured output must fail before dispatch");
        assert!(matches!(
            error,
            LlmError::InvalidRequest { provider: actual, .. } if actual == provider
        ));

        let mut tool_streaming_request = ToolCompletionRequest::new(
            vec![ChatMessage::user("return structured output")],
            Vec::new(),
        );
        tool_streaming_request.response_format = Some(structured_format());
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
        let sink = Arc::new(RecordingCompletionStreamSink { sender: delta_tx });
        let error = adapter
            .complete_with_tools_streaming(tool_streaming_request, sink)
            .await
            .expect_err("unsupported tool structured streaming must fail before dispatch");
        assert!(matches!(
            error,
            LlmError::InvalidRequest { provider: actual, .. } if actual == provider
        ));

        assert_eq!(completions.load(Ordering::Relaxed), 0);
        assert_eq!(streams.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn streaming_methods_use_buffered_fallback_until_explicitly_enabled() {
    let adapter = RigAdapter::new(
        ReplayingCompletionModel {
            choice: OneOrMany::one(AssistantContent::text("buffered")),
            raw_response: serde_json::json!({
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "buffered"},
                    "finish_reason": "stop",
                }],
            }),
        },
        "untested-provider",
    );
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel();
    let sink = Arc::new(RecordingCompletionStreamSink { sender: delta_tx });

    let text_response = adapter
        .complete_streaming(
            CompletionRequest::new(vec![ChatMessage::user("hello")]),
            sink.clone(),
        )
        .await
        .expect("buffered text fallback succeeds");
    let tool_response = adapter
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(vec![ChatMessage::user("hello")], Vec::new()),
            sink,
        )
        .await
        .expect("buffered tool fallback succeeds");

    assert_eq!(text_response.content, "buffered");
    assert_eq!(tool_response.content.as_deref(), Some("buffered"));
    assert!(
        delta_rx.try_recv().is_err(),
        "buffered fallback must not emit deltas"
    );
}

#[tokio::test]
async fn complete_streaming_preserves_reasoning_deltas() {
    let adapter = with_native_streaming(RigAdapter::new(
        StreamingOnlyCompletionModel {
            expected_max_tokens: None,
        },
        "streaming-only",
    ));
    let request = CompletionRequest::new(vec![ChatMessage::user("hello")]);
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel();
    let sink = Arc::new(RecordingCompletionStreamSink { sender: delta_tx });

    let response = adapter
        .complete_streaming(request, sink)
        .await
        .expect("streaming completion succeeds");

    assert_eq!(delta_rx.try_recv().expect("first delta"), "Hel");
    assert_eq!(delta_rx.try_recv().expect("second delta"), "lo");
    assert_eq!(response.content, "Hello");
    assert_eq!(response.reasoning.as_deref(), Some("thinking"));
    assert_eq!(response.input_tokens, 3);
    assert_eq!(response.output_tokens, 4);
}

#[tokio::test]
async fn complete_with_tools_streaming_uses_rig_stream_and_emits_deltas() {
    let adapter = with_native_streaming(RigAdapter::new(
        StreamingOnlyCompletionModel {
            expected_max_tokens: None,
        },
        "streaming-only",
    ));
    let request = ToolCompletionRequest::new(
        vec![ChatMessage::user("search")],
        vec![IronToolDefinition {
            name: "search".to_string(),
            description: "Search the index".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
            }),
        }],
    );
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel();
    let sink = Arc::new(RecordingCompletionStreamSink { sender: delta_tx });

    let response = adapter
        .complete_with_tools_streaming(request, sink)
        .await
        .expect("streaming completion succeeds");

    assert_eq!(delta_rx.try_recv().expect("first delta"), "Hel");
    assert_eq!(delta_rx.try_recv().expect("second delta"), "lo");
    assert_eq!(response.content.as_deref(), Some("Hello"));
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "search");
    assert_eq!(response.reasoning.as_deref(), Some("thinking"));
    assert_eq!(response.input_tokens, 3);
    assert_eq!(response.output_tokens, 4);
}

#[tokio::test]
async fn complete_with_tools_streaming_applies_provider_default_max_tokens() {
    const DEFAULT_MAX_TOKENS: u32 = 8192;

    let adapter = with_native_streaming(RigAdapter::new(
        StreamingOnlyCompletionModel {
            expected_max_tokens: Some(u64::from(DEFAULT_MAX_TOKENS)),
        },
        "streaming-only",
    ))
    .with_default_max_tokens(DEFAULT_MAX_TOKENS);
    let request = ToolCompletionRequest::new(
        vec![ChatMessage::user("search")],
        vec![IronToolDefinition {
            name: "search".to_string(),
            description: "Search the index".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
            }),
        }],
    );
    let (delta_tx, _delta_rx) = mpsc::unbounded_channel();

    adapter
        .complete_with_tools_streaming(
            request,
            Arc::new(RecordingCompletionStreamSink { sender: delta_tx }),
        )
        .await
        .expect("streaming completion succeeds with provider default");
}

#[tokio::test]
async fn complete_streaming_preserves_explicit_max_tokens_over_provider_default() {
    let adapter = with_native_streaming(RigAdapter::new(
        StreamingOnlyCompletionModel {
            expected_max_tokens: Some(4096),
        },
        "streaming-only",
    ))
    .with_default_max_tokens(8192);
    let request = CompletionRequest::new(vec![ChatMessage::user("hello")]).with_max_tokens(4096);
    let (delta_tx, _delta_rx) = mpsc::unbounded_channel();

    adapter
        .complete_streaming(
            request,
            Arc::new(RecordingCompletionStreamSink { sender: delta_tx }),
        )
        .await
        .expect("explicit max_tokens takes precedence");
}
