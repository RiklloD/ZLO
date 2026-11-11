use super::*;

/// The tail host reminder must not create a second consecutive user
/// message after a tool result. This adapter serves OpenAI, Anthropic,
/// Ollama and Tinfoil; the Tool arm already merges for exactly this reason
/// ("multiple consecutive User messages (which Anthropic rejects)"), and
/// #6985 put a reminder at the tail of every loop iteration, landing
/// directly after that iteration's tool results.
#[test]
fn host_reminder_merges_into_the_trailing_user_message() {
    use crate::provider::{ChatMessage, ToolCall};

    let messages = vec![
        ChatMessage::system("cached prefix"),
        ChatMessage::user("fetch the page"),
        ChatMessage::assistant_with_tool_calls(
            Some("calling".to_string()),
            vec![ToolCall {
                id: "call-1".to_string(),
                name: "http".to_string(),
                arguments: serde_json::json!({}),
                arguments_parse_error: None,
                reasoning: None,
                signature: None,
            }],
        ),
        ChatMessage::tool_result("call-1", "http", "200 OK"),
        ChatMessage::host_reminder("Current date/time at loop start: 10:00"),
    ];

    let (preamble, history) = super::convert_messages(&messages);

    assert_eq!(preamble.as_deref(), Some("cached prefix"));
    for pair in history.windows(2) {
        let same_role = matches!(
            (&pair[0], &pair[1]),
            (
                super::RigMessage::User { .. },
                super::RigMessage::User { .. }
            )
        );
        assert!(!same_role, "consecutive User messages: {history:#?}");
    }
    // The reminder rode into the tool-result user message rather than
    // becoming a second one.
    let super::RigMessage::User { content } = history.last().expect("trailing user message") else {
        panic!("expected a trailing user message, got {history:#?}");
    };
    assert!(
        content.iter().any(|part| matches!(
            part,
            super::UserContent::Text(text) if text.text.contains("Current date/time at loop start")
        )),
        "reminder text missing from the merged message: {content:#?}"
    );
}
#[test]
fn test_round_f32_to_f64_no_precision_artifacts() {
    // Direct f32->f64 cast produces 0.699999988079071 instead of 0.7
    assert_eq!(round_f32_to_f64(0.7_f32), 0.7_f64);
    assert_eq!(round_f32_to_f64(0.5_f32), 0.5_f64);
    assert_eq!(round_f32_to_f64(1.0_f32), 1.0_f64);
    assert_eq!(round_f32_to_f64(0.0_f32), 0.0_f64);
    // Original cast produces artifacts — our fix should not
    assert_ne!(0.7_f32 as f64, 0.7_f64);
}

// ── normalize_schema_strict: top-level flatten ────────────────────────
//
// OpenAI's tool API rejects schemas whose top level isn't `type:
// "object"` or that contain top-level `oneOf`/`anyOf`/`allOf`/`enum`/
// `not`. The GitHub Copilot MCP server exposes a tool with exactly that
// shape (action dispatch via top-level union), and the agent gets HTTP
// 400 the moment it tries to enumerate tools. `normalize_schema_strict`
// detects the bad shape, flattens parameters to a permissive object
// envelope, and stuffs the original schema into the description as
// advisory text so the LLM can still pick variant fields. Both rig-based
// providers and the Codex Responses API client share this normalizer.

#[test]
fn test_convert_messages_system_to_preamble() {
    let messages = vec![
        ChatMessage::system("You are a helpful assistant."),
        ChatMessage::user("Hello"),
    ];
    let (preamble, history) = convert_messages(&messages);
    assert_eq!(preamble, Some("You are a helpful assistant.".to_string()));
    assert_eq!(history.len(), 1);
}

#[test]
fn test_convert_messages_multiple_systems_concatenated() {
    let messages = vec![
        ChatMessage::system("System 1"),
        ChatMessage::system("System 2"),
        ChatMessage::user("Hi"),
    ];
    let (preamble, history) = convert_messages(&messages);
    assert_eq!(preamble, Some("System 1\nSystem 2".to_string()));
    assert_eq!(history.len(), 1);
}

#[test]
fn test_convert_messages_tool_result() {
    // Use a conforming 9-char alphanumeric ID so it passes through unchanged.
    let messages = vec![ChatMessage::tool_result(
        "abcDE1234",
        "search",
        "result text",
    )];
    let (preamble, history) = convert_messages(&messages);
    assert!(preamble.is_none());
    assert_eq!(history.len(), 1);
    // Tool results become User messages in rig-core
    match &history[0] {
        RigMessage::User { content } => match content.first() {
            UserContent::ToolResult(r) => {
                assert_eq!(r.id, "abcDE1234");
                assert_eq!(r.call_id.as_deref(), Some("abcDE1234"));
            }
            other => panic!("Expected tool result content, got: {:?}", other),
        },
        other => panic!("Expected User message, got: {:?}", other),
    }
}

#[test]
fn test_convert_messages_assistant_with_tool_calls() {
    // Use a conforming 9-char alphanumeric ID so it passes through unchanged.
    let tc = IronToolCall {
        id: "Xt7mK9pQ2".to_string(),
        name: "search".to_string(),
        arguments: serde_json::json!({"query": "test"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    };
    let msg = ChatMessage::assistant_with_tool_calls(Some("thinking".to_string()), vec![tc]);
    let messages = vec![msg];
    let (_preamble, history) = convert_messages(&messages);
    assert_eq!(history.len(), 1);
    match &history[0] {
        RigMessage::Assistant { content, .. } => {
            // Should have both text and tool call
            assert!(content.iter().count() >= 2);
            for item in content.iter() {
                if let AssistantContent::ToolCall(tc) = item {
                    assert_eq!(tc.call_id.as_deref(), Some("Xt7mK9pQ2"));
                }
            }
        }
        other => panic!("Expected Assistant message, got: {:?}", other),
    }
}

#[test]
fn test_convert_messages_tool_result_without_id_gets_fallback() {
    let messages = vec![ChatMessage {
        role: crate::Role::Tool,
        content: "result text".to_string(),
        content_parts: Vec::new(),
        tool_call_id: None,
        name: Some("search".to_string()),
        tool_calls: None,
        reasoning: None,
        reasoning_details: None,
        tool_result_structured_json_view: false,
    }];
    let (_preamble, history) = convert_messages(&messages);
    match &history[0] {
        RigMessage::User { content } => match content.first() {
            UserContent::ToolResult(r) => {
                // Missing ID → normalized_tool_call_id generates a 9-char alphanumeric ID.
                assert_eq!(
                    r.id.len(),
                    9,
                    "fallback ID should be 9 chars, got: {}",
                    r.id
                );
                assert!(r.id.chars().all(|c| c.is_ascii_alphanumeric()));
                assert_eq!(r.call_id.as_deref(), Some(r.id.as_str()));
            }
            other => panic!("Expected tool result content, got: {:?}", other),
        },
        other => panic!("Expected User message, got: {:?}", other),
    }
}

#[test]
fn test_convert_messages_data_url_without_detail_defaults_to_auto() {
    let messages = vec![ChatMessage::user_with_parts(
        "describe this",
        vec![crate::ContentPart::ImageUrl {
            image_url: crate::ImageUrl {
                url: "data:image/jpeg;base64,Zm9v".to_string(),
                detail: None,
            },
        }],
    )];

    let (_preamble, history) = convert_messages(&messages);
    match &history[0] {
        RigMessage::User { content } => {
            let image = content
                .iter()
                .find_map(|item| match item {
                    UserContent::Image(image) => Some(image),
                    _ => None,
                })
                .expect("expected image content");
            assert_eq!(image.detail, Some(ImageDetail::Auto));
        }
        other => panic!("Expected User message, got: {:?}", other),
    }
}

#[test]
fn test_convert_messages_image_detail_preserves_explicit_values() {
    let low_messages = vec![ChatMessage::user_with_parts(
        "low detail",
        vec![crate::ContentPart::ImageUrl {
            image_url: crate::ImageUrl {
                url: "https://example.com/image-low.png".to_string(),
                detail: Some("low".to_string()),
            },
        }],
    )];
    let high_messages = vec![ChatMessage::user_with_parts(
        "high detail",
        vec![crate::ContentPart::ImageUrl {
            image_url: crate::ImageUrl {
                url: "https://example.com/image-high.png".to_string(),
                detail: Some("high".to_string()),
            },
        }],
    )];

    let (_, low_history) = convert_messages(&low_messages);
    let (_, high_history) = convert_messages(&high_messages);

    for (history, expected) in [
        (&low_history, ImageDetail::Low),
        (&high_history, ImageDetail::High),
    ] {
        match &history[0] {
            RigMessage::User { content } => {
                let image = content
                    .iter()
                    .find_map(|item| match item {
                        UserContent::Image(image) => Some(image),
                        _ => None,
                    })
                    .expect("expected image content");
                assert_eq!(image.detail, Some(expected.clone()));
            }
            other => panic!("Expected User message, got: {:?}", other),
        }
    }
}

#[test]
fn test_convert_tools() {
    let tools = vec![IronToolDefinition {
        name: "search".to_string(),
        description: "Search the web".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            }
        }),
    }];
    let rig_tools = convert_tools(&tools);
    assert_eq!(rig_tools.len(), 1);
    assert_eq!(rig_tools[0].name, "search");
    assert_eq!(rig_tools[0].description, "Search the web");
}

#[test]
fn test_convert_tool_choice() {
    assert!(matches!(
        convert_tool_choice(Some("auto")),
        Some(RigToolChoice::Auto)
    ));
    assert!(matches!(
        convert_tool_choice(Some("required")),
        Some(RigToolChoice::Required)
    ));
    assert!(matches!(
        convert_tool_choice(Some("none")),
        Some(RigToolChoice::None)
    ));
    assert!(matches!(
        convert_tool_choice(Some("AUTO")),
        Some(RigToolChoice::Auto)
    ));
    assert!(convert_tool_choice(None).is_none());
    match convert_tool_choice(Some("builtin__structured_result")) {
        Some(RigToolChoice::Specific { function_names }) => {
            assert_eq!(
                function_names,
                vec!["builtin__structured_result".to_string()]
            );
        }
        other => panic!("named tool choice must map to Specific, got {other:?}"),
    }
}

#[test]
fn test_extract_response_text_only() {
    let content = OneOrMany::one(AssistantContent::text("Hello world"));
    let usage = RigUsage::new();
    let (text, calls, finish, _reasoning, _reasoning_details) =
        extract_response(&content, &usage, None);
    assert_eq!(text, Some("Hello world".to_string()));
    assert!(calls.is_empty());
    assert_eq!(finish, FinishReason::Stop);
}

#[test]
fn test_extract_response_tool_call() {
    let tc = AssistantContent::tool_call("call_1", "search", serde_json::json!({"q": "test"}));
    let content = OneOrMany::one(tc);
    let usage = RigUsage::new();
    let (text, calls, finish, _reasoning, _reasoning_details) =
        extract_response(&content, &usage, None);
    assert!(text.is_none());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "search");
    assert_eq!(finish, FinishReason::ToolUse);
}

#[test]
fn test_assistant_tool_call_empty_id_gets_generated() {
    let tc = IronToolCall {
        id: "".to_string(),
        name: "search".to_string(),
        arguments: serde_json::json!({"query": "test"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    };
    let messages = vec![ChatMessage::assistant_with_tool_calls(None, vec![tc])];
    let (_preamble, history) = convert_messages(&messages);

    match &history[0] {
        RigMessage::Assistant { content, .. } => {
            let tool_call = content.iter().find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc),
                _ => None,
            });
            let tc = tool_call.expect("should have a tool call");
            // Empty ID → normalized_tool_call_id generates a 9-char alphanumeric ID.
            assert_eq!(
                tc.id.len(),
                9,
                "generated id should be 9 chars, got: {}",
                tc.id
            );
            assert!(tc.id.chars().all(|c| c.is_ascii_alphanumeric()));
            assert_eq!(tc.call_id.as_deref(), Some(tc.id.as_str()));
        }
        other => panic!("Expected Assistant message, got: {:?}", other),
    }
}

#[test]
fn test_assistant_tool_call_whitespace_id_gets_generated() {
    let tc = IronToolCall {
        id: "   ".to_string(),
        name: "search".to_string(),
        arguments: serde_json::json!({"query": "test"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    };
    let messages = vec![ChatMessage::assistant_with_tool_calls(None, vec![tc])];
    let (_preamble, history) = convert_messages(&messages);

    match &history[0] {
        RigMessage::Assistant { content, .. } => {
            let tool_call = content.iter().find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc),
                _ => None,
            });
            let tc = tool_call.expect("should have a tool call");
            // Whitespace-only ID → normalized_tool_call_id generates a 9-char alphanumeric ID.
            assert_eq!(
                tc.id.len(),
                9,
                "generated id should be 9 chars, got: {}",
                tc.id
            );
            assert!(tc.id.chars().all(|c| c.is_ascii_alphanumeric()));
        }
        other => panic!("Expected Assistant message, got: {:?}", other),
    }
}

#[test]
fn test_assistant_and_tool_result_missing_ids_share_generated_id() {
    // Simulate: assistant emits a tool call with empty id, then tool
    // result arrives without an id. Both should get deterministic
    // generated ids that match (based on their position in history).
    let tc = IronToolCall {
        id: "".to_string(),
        name: "search".to_string(),
        arguments: serde_json::json!({"query": "test"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    };
    let assistant_msg = ChatMessage::assistant_with_tool_calls(None, vec![tc]);
    let tool_result_msg = ChatMessage {
        role: crate::Role::Tool,
        content: "search results here".to_string(),
        content_parts: Vec::new(),
        tool_call_id: None,
        name: Some("search".to_string()),
        tool_calls: None,
        reasoning: None,
        reasoning_details: None,
        tool_result_structured_json_view: false,
    };
    let messages = vec![assistant_msg, tool_result_msg];
    let (_preamble, history) = convert_messages(&messages);

    // Extract the generated call_id from the assistant tool call
    let assistant_call_id = match &history[0] {
        RigMessage::Assistant { content, .. } => {
            let tc = content.iter().find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc),
                _ => None,
            });
            tc.expect("should have tool call").id.clone()
        }
        other => panic!("Expected Assistant message, got: {:?}", other),
    };

    // Extract the generated call_id from the tool result
    let tool_result_call_id = match &history[1] {
        RigMessage::User { content } => match content.first() {
            UserContent::ToolResult(r) => r
                .call_id
                .clone()
                .expect("tool result call_id must be present"),
            other => panic!("Expected ToolResult, got: {:?}", other),
        },
        other => panic!("Expected User message, got: {:?}", other),
    };

    assert!(
        !assistant_call_id.is_empty(),
        "assistant call_id must not be empty"
    );
    assert!(
        !tool_result_call_id.is_empty(),
        "tool result call_id must not be empty"
    );

    // NOTE: With the current seed-based generation, these IDs will differ
    // because the assistant tool call uses seed=0 (history.len() at that
    // point) and the tool result uses seed=1 (history.len() after the
    // assistant message was pushed). This documents the current behavior.
    // A future improvement could thread the assistant's generated ID into
    // the tool result for exact matching.
    assert_ne!(
        assistant_call_id, tool_result_call_id,
        "Current impl generates different IDs for assistant call and tool result \
             because seeds differ; this documents the known limitation"
    );
}

#[test]
fn test_saturate_u32() {
    assert_eq!(saturate_u32(100), 100);
    assert_eq!(saturate_u32(u64::MAX), u32::MAX);
    assert_eq!(saturate_u32(u32::MAX as u64), u32::MAX);
}

// -- normalize_tool_name tests --

#[test]
fn test_normalize_tool_name_exact_match() {
    let known = HashSet::from(["echo".to_string(), "list_jobs".to_string()]);
    assert_eq!(normalize_tool_name("echo", &known), "echo");
}

#[test]
fn test_normalize_tool_name_proxy_prefix_match() {
    let known = HashSet::from(["echo".to_string(), "list_jobs".to_string()]);
    assert_eq!(normalize_tool_name("proxy_echo", &known), "echo");
}

#[test]
fn test_normalize_tool_name_proxy_prefix_no_match_kept() {
    let known = HashSet::from(["echo".to_string(), "list_jobs".to_string()]);
    assert_eq!(
        normalize_tool_name("proxy_unknown", &known),
        "proxy_unknown"
    );
}

#[test]
fn test_normalize_tool_name_unknown_passthrough() {
    let known = HashSet::from(["echo".to_string()]);
    assert_eq!(normalize_tool_name("other_tool", &known), "other_tool");
}

/// Regression test: consecutive tool_result messages from parallel tool
/// execution must be merged into a single User message with multiple
/// ToolResult content items. Without merging, APIs like Anthropic reject
/// the request due to consecutive User messages.
#[test]
fn test_consecutive_tool_results_merged_into_single_user_message() {
    let tc1 = IronToolCall {
        id: "call_a".to_string(),
        name: "search".to_string(),
        arguments: serde_json::json!({"q": "rust"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    };
    let tc2 = IronToolCall {
        id: "call_b".to_string(),
        name: "fetch".to_string(),
        arguments: serde_json::json!({"url": "https://example.com"}),
        reasoning: None,
        signature: None,
        arguments_parse_error: None,
    };
    let assistant = ChatMessage::assistant_with_tool_calls(None, vec![tc1, tc2]);
    let result_a = ChatMessage::tool_result("call_a", "search", "search results");
    let result_b = ChatMessage::tool_result("call_b", "fetch", "fetch results");

    let messages = vec![assistant, result_a, result_b];
    let (_preamble, history) = convert_messages(&messages);

    // Should be: 1 assistant + 1 merged user (not 1 assistant + 2 users)
    assert_eq!(
        history.len(),
        2,
        "Expected 2 messages (assistant + merged user), got {}",
        history.len()
    );

    // The second message should contain both tool results
    match &history[1] {
        RigMessage::User { content } => {
            assert_eq!(
                content.len(),
                2,
                "Expected 2 tool results in merged user message, got {}",
                content.len()
            );
            for item in content.iter() {
                assert!(
                    matches!(item, UserContent::ToolResult(_)),
                    "Expected ToolResult content"
                );
            }
        }
        other => panic!("Expected User message, got: {:?}", other),
    }
}

/// Verify that a tool_result after a non-tool User message is NOT merged.
#[test]
fn test_tool_result_after_user_text_not_merged() {
    let user_msg = ChatMessage::user("hello");
    let tool_msg = ChatMessage::tool_result("call_1", "search", "results");

    let messages = vec![user_msg, tool_msg];
    let (_preamble, history) = convert_messages(&messages);

    // Should be 2 separate User messages (text user + tool result user)
    assert_eq!(history.len(), 2);
}

/// Empty user messages (e.g. after thinking-tag stripping) must be skipped.
/// Strict providers like Kimi return 400 when "content": "" is sent.
#[test]
fn test_empty_user_message_is_skipped() {
    let empty = ChatMessage::user("");
    let non_empty = ChatMessage::user("hello");
    let messages = vec![empty, non_empty];
    let (_preamble, history) = convert_messages(&messages);

    assert_eq!(history.len(), 1, "empty user message must be dropped");
    match &history[0] {
        RigMessage::User { content } => {
            assert_eq!(content.len(), 1);
            let first = content.iter().next().expect("one content item");
            match first {
                UserContent::Text(t) => assert_eq!(t.text, "hello"),
                other => panic!("expected Text, got {:?}", other),
            }
        }
        other => panic!("expected User message, got {:?}", other),
    }
}

/// Empty assistant messages (e.g. after thinking-tag stripping) must be skipped.
#[test]
fn test_empty_assistant_message_is_skipped() {
    let empty_asst = ChatMessage {
        role: crate::Role::Assistant,
        content: String::new(),
        tool_calls: None,
        reasoning: None,
        reasoning_details: None,
        tool_result_structured_json_view: false,
        tool_call_id: None,
        name: None,
        content_parts: vec![],
    };
    let non_empty = ChatMessage::user("hi");
    let messages = vec![empty_asst, non_empty];
    let (_preamble, history) = convert_messages(&messages);

    assert_eq!(history.len(), 1, "empty assistant message must be dropped");
    assert!(matches!(history[0], RigMessage::User { .. }));
}

/// A conversation mixing normal and empty messages: only non-empty ones survive.
#[test]
fn test_mixed_empty_and_non_empty_messages_filtered_correctly() {
    let user1 = ChatMessage::user("first");
    let empty_asst = ChatMessage {
        role: crate::Role::Assistant,
        content: String::new(),
        tool_calls: None,
        reasoning: None,
        reasoning_details: None,
        tool_result_structured_json_view: false,
        tool_call_id: None,
        name: None,
        content_parts: vec![],
    };
    let user2 = ChatMessage::user("");
    let asst = ChatMessage::assistant("response");
    let messages = vec![user1, empty_asst, user2, asst];
    let (_preamble, history) = convert_messages(&messages);

    assert_eq!(history.len(), 2, "only non-empty messages should survive");
    assert!(matches!(history[0], RigMessage::User { .. }));
    assert!(matches!(history[1], RigMessage::Assistant { .. }));
}

// -- normalized_tool_call_id tests --

#[test]
fn test_normalized_tool_call_id_conforming_passthrough() {
    // A 9-char alphanumeric ID should pass through unchanged.
    let id = normalized_tool_call_id(Some("abcDE1234"), 42);
    assert_eq!(id, "abcDE1234");
}

#[test]
fn test_normalized_tool_call_id_non_conforming_hashed() {
    // An ID that doesn't match [a-zA-Z0-9]{9} should be hashed into one.
    let id = normalized_tool_call_id(Some("call_abc_long_id"), 0);
    assert_eq!(id.len(), 9);
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    // Should NOT be the raw input.
    assert_ne!(id, "call_abc_l");
}

#[test]
fn test_normalized_tool_call_id_empty_input() {
    let id = normalized_tool_call_id(Some(""), 5);
    assert_eq!(id.len(), 9);
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn test_normalized_tool_call_id_whitespace_input() {
    let id = normalized_tool_call_id(Some("   "), 5);
    assert_eq!(id.len(), 9);
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    // Empty and whitespace-only with the same seed should produce identical results.
    let id_empty = normalized_tool_call_id(Some(""), 5);
    assert_eq!(id, id_empty);
}

#[test]
fn test_normalized_tool_call_id_none_input() {
    let id = normalized_tool_call_id(None, 7);
    assert_eq!(id.len(), 9);
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    // None and empty string with same seed should produce identical results.
    let id_empty = normalized_tool_call_id(Some(""), 7);
    assert_eq!(id, id_empty);
}

#[test]
fn test_normalized_tool_call_id_deterministic() {
    let id1 = normalized_tool_call_id(Some("call_xyz_123"), 0);
    let id2 = normalized_tool_call_id(Some("call_xyz_123"), 0);
    assert_eq!(id1, id2, "same input must produce same output");
}

#[test]
fn test_normalized_tool_call_id_different_inputs_differ() {
    let id_a = normalized_tool_call_id(Some("call_aaa"), 0);
    let id_b = normalized_tool_call_id(Some("call_bbb"), 0);
    assert_ne!(
        id_a, id_b,
        "different raw IDs should produce different hashed IDs"
    );
}

/// Regression for #3201 / #3225 (the high-severity gap in PR #3326):
/// the dedicated rig-core DeepSeek/Gemini/OpenRouter clients only fix the
/// reasoning round-trip *inside* rig-core. IronClaw's RigAdapter sits
/// between the agent loop and rig-core, and previously dropped both
/// `AssistantContent::Reasoning` (DeepSeek `reasoning_content`) and
/// per-tool-call `signature` (Gemini `thought_signature`) on the response →
/// IronClaw conversion. On the next request it rebuilt rig messages
/// without either field, so the provider rejected the follow-up turn.
///
/// This test simulates a 2-turn tool loop:
///   1. extract a rig response carrying both reasoning and a signed tool call
///   2. round-trip those onto an IronClaw `ChatMessage`
///   3. convert that message back into rig format
///   4. assert both reasoning and signature appear on the rebuilt rig message
///
/// If any layer drops a field, the next turn would fail with HTTP 400.
#[test]
fn reasoning_and_signature_round_trip_through_chat_message() {
    // --- Turn 1: provider returned reasoning + signed tool call ---
    let rig_response = OneOrMany::many(vec![
        AssistantContent::Reasoning(rig::message::Reasoning::new(
            "Let me check the weather first.",
        )),
        AssistantContent::ToolCall(
            rig::message::ToolCall::new(
                "call_abc123".to_string(),
                ToolFunction::new(
                    "get_weather".to_string(),
                    serde_json::json!({"city": "London"}),
                ),
            )
            .with_signature(Some("thought-sig-deadbeef".to_string())),
        ),
    ])
    .unwrap();
    let usage = RigUsage::new();
    let (text, tool_calls, finish, reasoning, reasoning_details) =
        extract_response(&rig_response, &usage, None);

    assert_eq!(finish, FinishReason::ToolUse);
    assert_eq!(text, None);
    assert_eq!(
        reasoning.as_deref(),
        Some("Let me check the weather first."),
        "extract_response must capture AssistantContent::Reasoning so the \
             next request can echo DeepSeek's reasoning_content (#3201)",
    );
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        tool_calls[0].signature.as_deref(),
        Some("thought-sig-deadbeef"),
        "extract_response must capture ToolCall.signature so the next \
             request can echo Gemini's thought_signature (#3225)",
    );

    // --- IronClaw stores the assistant message + tool result ---
    let assistant = ChatMessage::assistant_with_tool_calls(text, tool_calls)
        .with_reasoning_details(reasoning_details.clone());
    let tool_result = ChatMessage::tool_result("call_abc123", "get_weather", "{\"temp_c\": 14}");

    // --- Turn 2: IronClaw rebuilds the rig request from stored messages ---
    let messages = vec![
        ChatMessage::user("What's the weather?"),
        assistant,
        tool_result,
    ];
    let (_preamble, history) = convert_messages(&messages);

    // The rebuilt rig assistant message must carry both reasoning and
    // signature; otherwise the dedicated DeepSeek/Gemini/OpenRouter rig
    // clients would emit an empty `reasoning_content` / unsigned
    // `functionCall` and the API would reject with HTTP 400.
    let assistant_msg = history
        .iter()
        .find(|m| matches!(m, RigMessage::Assistant { .. }))
        .expect("rebuilt rig history should contain the assistant message");
    let RigMessage::Assistant { content, .. } = assistant_msg else {
        unreachable!()
    };

    let mut found_reasoning = false;
    let mut found_signed_tool_call = false;
    for c in content.iter() {
        match c {
            AssistantContent::Reasoning(r) => {
                let reasoning = r
                    .content
                    .iter()
                    .map(|content| match content {
                        rig::message::ReasoningContent::Text { text, .. } => text.as_str(),
                        _ => "",
                    })
                    .collect::<Vec<_>>();
                assert_eq!(reasoning, vec!["Let me check the weather first."]);
                found_reasoning = true;
            }
            AssistantContent::ToolCall(tc) => {
                assert_eq!(
                    tc.signature.as_deref(),
                    Some("thought-sig-deadbeef"),
                    "rebuilt rig tool call must carry the original \
                         thought_signature (#3225)",
                );
                found_signed_tool_call = true;
            }
            _ => {}
        }
    }
    assert!(
        found_reasoning,
        "convert_messages must emit AssistantContent::Reasoning when \
             ChatMessage carries reasoning — without this, DeepSeek thinking \
             mode rejects the next turn (#3201)",
    );
    assert!(
        found_signed_tool_call,
        "convert_messages must propagate ToolCall.signature when \
             rebuilding rig tool calls — without this, Gemini 2.5+ rejects \
             the next turn (#3225)",
    );
}

#[test]
fn typed_reasoning_round_trips_through_chat_message() {
    let mut typed_reasoning = rig::message::Reasoning::new("");
    typed_reasoning.id = Some("rsn_123".to_string());
    typed_reasoning.content = vec![
        rig::message::ReasoningContent::Encrypted("encrypted-payload".to_string()),
        rig::message::ReasoningContent::Redacted {
            data: "redacted-payload".to_string(),
        },
        rig::message::ReasoningContent::Summary("safe summary".to_string()),
    ];
    let rig_response = OneOrMany::many(vec![
        AssistantContent::Reasoning(typed_reasoning.clone()),
        AssistantContent::ToolCall(rig::message::ToolCall::new(
            "call_abc123".to_string(),
            ToolFunction::new("lookup".to_string(), serde_json::json!({})),
        )),
    ])
    .unwrap();

    let usage = RigUsage::new();
    let (text, tool_calls, _finish, reasoning, reasoning_details) =
        extract_response(&rig_response, &usage, None);

    assert_eq!(text, None);
    assert_eq!(reasoning.as_deref(), Some("safe summary"));
    let assistant = ChatMessage::assistant_with_tool_calls(text, tool_calls)
        .with_reasoning_details(reasoning_details);

    let (_preamble, history) = convert_messages(&[
        ChatMessage::user("continue"),
        assistant,
        ChatMessage::tool_result("call_abc123", "lookup", "{}"),
    ]);
    let assistant_msg = history
        .iter()
        .find(|m| matches!(m, RigMessage::Assistant { .. }))
        .expect("rebuilt rig history should contain the assistant message");
    let RigMessage::Assistant { content, .. } = assistant_msg else {
        unreachable!()
    };

    let replayed_reasoning = content
        .iter()
        .find_map(|content| match content {
            AssistantContent::Reasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .expect("rebuilt assistant message should include typed reasoning");
    assert_eq!(replayed_reasoning.id, typed_reasoning.id);
    assert_eq!(replayed_reasoning.content, typed_reasoning.content);
}

/// Regression: a Text block with empty text but a non-empty signature must
/// survive `with_reasoning_details` and reach `convert_messages` unchanged.
/// Gemini 2.5+ emits `thought_signature` blocks this way; dropping them
/// breaks the next turn with HTTP 400 (#3201, #3225).
#[test]
fn signature_only_text_block_survives_round_trip() {
    let signature_only = IronReasoningDetail::Text {
        text: String::new(),
        signature: Some("thought-sig-xyz".to_string()),
    };
    let details = IronReasoningDetails {
        id: Some("rsn_sig".to_string()),
        content: vec![signature_only],
    };
    // Must not be filtered by with_reasoning_details.
    let assistant = ChatMessage::assistant("ok").with_reasoning_details(Some(details.clone()));
    assert!(
        assistant.reasoning_details.is_some(),
        "with_reasoning_details must preserve a signature-only Text block"
    );

    let (_preamble, history) = convert_messages(&[ChatMessage::user("ping"), assistant]);
    let assistant_msg = history
        .iter()
        .find(|m| matches!(m, RigMessage::Assistant { .. }))
        .expect("history must include the assistant message");
    let RigMessage::Assistant { content, .. } = assistant_msg else {
        unreachable!()
    };
    let replayed = content
        .iter()
        .find_map(|c| match c {
            AssistantContent::Reasoning(r) => Some(r),
            _ => None,
        })
        .expect("convert_messages must emit AssistantContent::Reasoning for signature-only block");
    assert_eq!(replayed.id.as_deref(), Some("rsn_sig"));
    assert_eq!(replayed.content.len(), 1);
    match &replayed.content[0] {
        rig::message::ReasoningContent::Text { text, signature } => {
            assert!(text.is_empty(), "text must remain empty");
            assert_eq!(
                signature.as_deref(),
                Some("thought-sig-xyz"),
                "signature must be preserved through the round-trip"
            );
        }
        other => panic!("expected ReasoningContent::Text, got {other:?}"),
    }
}

/// `with_reasoning` must drop empty/whitespace-only strings rather than
/// echoing `reasoning_content: ""` (some strict-mode providers reject
/// empty reasoning fields, and an empty echo carries no signal anyway).
#[test]
fn chat_message_with_reasoning_drops_empty_input() {
    let msg = ChatMessage::assistant("hi").with_reasoning(Some(String::new()));
    assert!(msg.reasoning.is_none());
    let msg = ChatMessage::assistant("hi").with_reasoning(Some("   ".to_string()));
    assert!(msg.reasoning.is_none());
    let msg = ChatMessage::assistant("hi").with_reasoning(None);
    assert!(msg.reasoning.is_none());
    let msg = ChatMessage::assistant("hi").with_reasoning(Some("real".to_string()));
    assert_eq!(msg.reasoning.as_deref(), Some("real"));
}
