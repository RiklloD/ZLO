use super::*;

// -- Type conversion helpers --

/// Round an f32 to f64 without precision artifacts.
///
/// Direct `f32 as f64` preserves the binary representation, producing values
/// like `0.699999988079071` instead of `0.7`. Some providers (e.g. Zhipu/GLM)
/// reject these values with a 400 error. Rounding to 6 decimal places removes
/// the artifact while preserving all meaningful precision for temperature.
pub(super) fn round_f32_to_f64(val: f32) -> f64 {
    ((val as f64) * 1_000_000.0).round() / 1_000_000.0
}

/// Convert IronClaw messages to rig-core format.
///
/// Returns `(preamble, chat_history)` where preamble is extracted from
/// any System message and chat_history contains the rest.
pub(super) fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<RigMessage>) {
    let mut preamble: Option<String> = None;
    let mut history = Vec::new();

    for msg in messages {
        match msg.role {
            crate::Role::System => {
                // Concatenate system messages into preamble
                match preamble {
                    Some(ref mut p) => {
                        p.push('\n');
                        p.push_str(&msg.content);
                    }
                    None => preamble = Some(msg.content.clone()),
                }
            }
            crate::Role::HostReminder => {
                // Host guidance rides the conversation tail, so it lands right
                // after the tool results of the iteration that produced it
                // (#6985). Anthropic rejects consecutive User messages — the
                // same constraint the Tool arm below merges for — so fold the
                // reminder into the trailing User message instead of pushing a
                // second one.
                if msg.content.is_empty() {
                    continue;
                }
                let reminder = UserContent::text(&msg.content);
                match history.last_mut() {
                    Some(RigMessage::User { content }) => content.push(reminder),
                    _ => history.push(RigMessage::User {
                        content: OneOrMany::one(reminder),
                    }),
                }
            }
            crate::Role::User => {
                if msg.content_parts.is_empty() {
                    // Skip empty user messages — some providers (e.g. Kimi) reject "content": ""
                    if msg.content.is_empty() {
                        continue;
                    }
                    history.push(RigMessage::user(&msg.content));
                } else {
                    // Build multimodal user message with text + image parts
                    let mut contents: Vec<UserContent> = vec![UserContent::text(&msg.content)];
                    for part in &msg.content_parts {
                        if let crate::ContentPart::ImageUrl { image_url } = part {
                            let detail =
                                ImageDetail::from_str(&image_url.normalized_openai_detail())
                                    .unwrap_or_default();
                            // Parse data: URL for base64 images, or use raw URL
                            let image = if let Some(rest) = image_url.url.strip_prefix("data:") {
                                // Format: data:<mime>;base64,<data>
                                let (mime, b64) =
                                    rest.split_once(";base64,").unwrap_or(("image/jpeg", rest));
                                Image {
                                    data: DocumentSourceKind::base64(b64),
                                    media_type: ImageMediaType::from_mime_type(mime),
                                    detail: Some(detail.clone()),
                                    additional_params: None,
                                }
                            } else {
                                Image {
                                    data: DocumentSourceKind::url(&image_url.url),
                                    media_type: None,
                                    detail: Some(detail),
                                    additional_params: None,
                                }
                            };
                            contents.push(UserContent::Image(image));
                        }
                    }
                    if let Ok(many) = OneOrMany::many(contents) {
                        history.push(RigMessage::User { content: many });
                    } else {
                        history.push(RigMessage::user(&msg.content));
                    }
                }
            }
            crate::Role::Assistant => {
                if let Some(ref tool_calls) = msg.tool_calls {
                    // Assistant message with tool calls
                    let mut contents: Vec<AssistantContent> = Vec::new();
                    if !msg.content.is_empty() {
                        contents.push(AssistantContent::text(&msg.content));
                    }
                    // Round-trip provider-emitted reasoning artifacts. rig-core's
                    // dedicated DeepSeek/Gemini/OpenRouter clients consume
                    // `AssistantContent::Reasoning` on the message and re-emit
                    // it as the wire-format reasoning field on the next request.
                    // Without this, DeepSeek/Gemini reject the follow-up turn
                    // with HTTP 400. See #3201, #3225.
                    if let Some(reasoning) = assistant_reasoning_content(msg) {
                        contents.push(reasoning);
                    }
                    for (idx, tc) in tool_calls.iter().enumerate() {
                        let tool_call_id =
                            normalized_tool_call_id(Some(tc.id.as_str()), history.len() + idx);
                        let mut rig_tc = rig::message::ToolCall::new(
                            tool_call_id.clone(),
                            ToolFunction::new(tc.name.clone(), tc.arguments.clone()),
                        )
                        .with_call_id(tool_call_id);
                        // Echo provider-emitted per-tool-call signatures back
                        // (Gemini's `thought_signature`). The reviewer's
                        // motivating example: a signed Gemini `functionCall`
                        // returned in turn N must carry the same signature
                        // when sent back in turn N+1, otherwise the API
                        // rejects with "Function call is missing a
                        // thought_signature in functionCall parts" (#3225).
                        if tc.signature.is_some() {
                            rig_tc = rig_tc.with_signature(tc.signature.clone());
                        }
                        contents.push(AssistantContent::ToolCall(rig_tc));
                    }
                    if let Ok(many) = OneOrMany::many(contents) {
                        history.push(RigMessage::Assistant {
                            id: None,
                            content: many,
                        });
                    } else {
                        // Shouldn't happen but fall back to text
                        history.push(RigMessage::assistant(&msg.content));
                    }
                } else if message_has_reasoning(msg) {
                    // Assistant message with reasoning but no tool calls
                    // (e.g., a "thinking" turn followed by a final answer).
                    // The next request still needs the reasoning echoed so the
                    // provider validates the chain, even when the message has
                    // no tool calls of its own.
                    let mut contents: Vec<AssistantContent> = Vec::new();
                    if !msg.content.is_empty() {
                        contents.push(AssistantContent::text(&msg.content));
                    }
                    if let Some(reasoning) = assistant_reasoning_content(msg) {
                        contents.push(reasoning);
                    }
                    if let Ok(many) = OneOrMany::many(contents) {
                        history.push(RigMessage::Assistant {
                            id: None,
                            content: many,
                        });
                    }
                } else {
                    // Skip empty assistant messages — these occur when thinking-tag stripping
                    // leaves a blank response; sending "content": "" causes 400 on strict
                    // OpenAI-compatible providers (e.g. Kimi).
                    if msg.content.is_empty() {
                        continue;
                    }
                    history.push(RigMessage::assistant(&msg.content));
                }
            }
            crate::Role::Tool => {
                // Tool result message: wrap as User { ToolResult }.
                // Merge consecutive tool results into a single User message
                // so the API sees one multi-result message instead of
                // multiple consecutive User messages (which Anthropic rejects).
                let tool_id = normalized_tool_call_id(msg.tool_call_id.as_deref(), history.len());
                let tool_result = UserContent::ToolResult(RigToolResult {
                    id: tool_id.clone(),
                    call_id: Some(tool_id),
                    content: OneOrMany::one(ToolResultContent::text(&msg.content)),
                });

                let should_merge = matches!(
                    history.last(),
                    Some(RigMessage::User { content }) if content.iter().all(|c| matches!(c, UserContent::ToolResult(_)))
                );

                if should_merge {
                    if let Some(RigMessage::User { content }) = history.last_mut() {
                        content.push(tool_result);
                    }
                } else {
                    history.push(RigMessage::User {
                        content: OneOrMany::one(tool_result),
                    });
                }
            }
        }
    }

    (preamble, history)
}

/// Responses-style providers require a non-empty tool call ID.
///
/// IDs must be compatible with providers like Mistral, which constrain IDs
/// to `[a-zA-Z0-9]{9}`. We therefore:
/// - pass through any non-empty raw ID that already matches this constraint;
/// - otherwise deterministically map the raw string into a provider-compliant ID;
/// - and when `raw` is empty/None, delegate to `generate_tool_call_id`.
pub(super) fn normalized_tool_call_id(raw: Option<&str>, seed: usize) -> String {
    // Trim and treat empty as None.
    let trimmed = raw.and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t) }
    });

    if let Some(id) = trimmed {
        // If the ID already satisfies `[a-zA-Z0-9]{9}`, pass it through unchanged.
        if id.len() == 9 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return id.to_string();
        }

        // Otherwise, deterministically hash the raw ID and feed the hash-derived
        // seed into the provider-level generator so that the encoding and any
        // provider-specific constraints remain centralized in one place.
        let digest = Sha256::digest(id.as_bytes());
        // Derive a 64-bit value from the first 8 bytes of the digest, then
        // split it into two usize seeds so we preserve all 64 bits of entropy
        // even on 32-bit targets.
        let hash64 = {
            // SHA-256 always produces 32 bytes, so indexing the first 8 is safe.
            let bytes: [u8; 8] = [
                digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6],
                digest[7],
            ];
            u64::from_be_bytes(bytes)
        };
        let hi_seed: usize = (hash64 >> 32) as usize;
        let lo_seed: usize = (hash64 & 0xFFFF_FFFF) as usize;
        return crate::provider::generate_tool_call_id(hi_seed, lo_seed);
    }

    // Fallback for missing/empty raw IDs: use the provider-level generator,
    // which already produces compliant IDs.
    crate::provider::generate_tool_call_id(seed, 0)
}

/// Convert IronClaw tool definitions to rig-core format.
///
/// Applies `normalize_schema_strict` at the boundary, which both
/// strict-normalizes nested objects AND flattens any top-level
/// `oneOf`/`anyOf`/`allOf`/`enum`/`not` (OpenAI's tool API rejects those at
/// the top level even when the rest of the schema is valid). The flatten may
/// append an advisory hint to the tool description, so we pass an owned
/// clone through and read it back.
pub(super) fn convert_tools(tools: &[IronToolDefinition]) -> Vec<RigToolDefinition> {
    tools
        .iter()
        .map(|t| {
            let mut description = t.description.clone();
            let parameters = shape_tool_schema(
                ToolSchemaPolicy::StrictOpenAi,
                &t.parameters,
                &mut description,
            );
            RigToolDefinition {
                name: t.name.clone(),
                description,
                parameters,
            }
        })
        .collect()
}

/// Convert IronClaw tool_choice string to rig-core ToolChoice.
pub(super) fn convert_tool_choice(choice: Option<&str>) -> Option<RigToolChoice> {
    let choice = choice?;
    match choice.to_lowercase().as_str() {
        "auto" => Some(RigToolChoice::Auto),
        "required" => Some(RigToolChoice::Required),
        "none" => Some(RigToolChoice::None),
        // A named tool: rig providers without specific-tool support reject
        // this loudly, which beats silently downgrading a forced call.
        _ => Some(RigToolChoice::Specific {
            function_names: vec![choice.to_string()],
        }),
    }
}

pub(super) fn message_has_reasoning(msg: &ChatMessage) -> bool {
    msg.reasoning_details
        .as_ref()
        .is_some_and(|details| !details.is_empty())
        || msg
            .reasoning
            .as_ref()
            .is_some_and(|reasoning| !reasoning.trim().is_empty())
}

pub(super) fn assistant_reasoning_content(msg: &ChatMessage) -> Option<AssistantContent> {
    if let Some(details) = msg
        .reasoning_details
        .as_ref()
        .filter(|details| !details.is_empty())
    {
        return Some(AssistantContent::Reasoning(iron_reasoning_to_rig(details)));
    }

    msg.reasoning
        .as_ref()
        .filter(|reasoning| !reasoning.trim().is_empty())
        .map(|reasoning| AssistantContent::Reasoning(rig::message::Reasoning::new(reasoning)))
}

pub(super) fn iron_reasoning_to_rig(details: &IronReasoningDetails) -> rig::message::Reasoning {
    let mut reasoning = rig::message::Reasoning::new("");
    reasoning.id = details.id.clone();
    reasoning.content = details
        .content
        .iter()
        .map(|detail| match detail {
            IronReasoningDetail::Text { text, signature } => rig::message::ReasoningContent::Text {
                text: text.clone(),
                signature: signature.clone(),
            },
            IronReasoningDetail::Encrypted(text) => {
                rig::message::ReasoningContent::Encrypted(text.clone())
            }
            IronReasoningDetail::Redacted { data } => {
                rig::message::ReasoningContent::Redacted { data: data.clone() }
            }
            IronReasoningDetail::Summary(text) => {
                rig::message::ReasoningContent::Summary(text.clone())
            }
        })
        .collect();
    reasoning
}

pub(super) fn rig_reasoning_to_iron(
    reasoning: &rig::message::Reasoning,
) -> Option<IronReasoningDetails> {
    let content = reasoning
        .content
        .iter()
        .filter_map(|detail| match detail {
            rig::message::ReasoningContent::Text { text, signature } => {
                Some(IronReasoningDetail::Text {
                    text: text.clone(),
                    signature: signature.clone(),
                })
            }
            rig::message::ReasoningContent::Encrypted(text) => {
                Some(IronReasoningDetail::Encrypted(text.clone()))
            }
            rig::message::ReasoningContent::Redacted { data } => {
                Some(IronReasoningDetail::Redacted { data: data.clone() })
            }
            rig::message::ReasoningContent::Summary(text) => {
                Some(IronReasoningDetail::Summary(text.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let details = IronReasoningDetails {
        id: reasoning.id.clone(),
        content,
    };
    (!details.is_empty()).then_some(details)
}

/// Extract text, tool calls, and provider-emitted reasoning artifacts from a
/// rig-core completion response.
///
/// The returned `reasoning` is the concatenation of every
/// `AssistantContent::Reasoning` chunk in the response. Callers MUST attach it
/// to the assistant `ChatMessage` they store for the next turn — DeepSeek's
/// thinking mode and Gemini 2.5+ both reject the next request with HTTP 400
/// when the prior message had reasoning that wasn't echoed back. See #3201,
/// #3225, and the rig-core deepseek client source where
/// `last_reasoning_content` is round-tripped onto the last assistant message
/// of the next request.
pub(super) fn extract_response(
    choice: &OneOrMany<AssistantContent>,
    _usage: &RigUsage,
    provider_finish: Option<FinishReason>,
) -> (
    Option<String>,
    Vec<IronToolCall>,
    FinishReason,
    Option<String>,
    Option<IronReasoningDetails>,
) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<IronToolCall> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut reasoning_details: Vec<IronReasoningDetail> = Vec::new();
    let mut reasoning_id: Option<String> = None;

    for content in choice.iter() {
        match content {
            AssistantContent::Text(t) if !t.text.is_empty() => {
                text_parts.push(t.text.clone());
            }
            AssistantContent::Text(_) => {}
            AssistantContent::ToolCall(tc) => {
                tool_calls.push(IronToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                    reasoning: None,
                    // Capture Gemini `thought_signature` (and any other
                    // per-tool-call signatures) so the next turn can echo
                    // them. Without this, Gemini 2.5+ rejects the next
                    // request with HTTP 400. See #3225.
                    signature: tc.signature.clone(),
                    arguments_parse_error: None,
                });
            }
            AssistantContent::Reasoning(r) if !r.content.is_empty() => {
                if reasoning_id.is_none() {
                    reasoning_id = r.id.clone();
                }
                if let Some(details) = rig_reasoning_to_iron(r) {
                    if let Some(display_text) = details.display_text() {
                        reasoning_parts.push(display_text);
                    }
                    reasoning_details.extend(details.content);
                }
            }
            // Image variants are not mapped to IronClaw types
            _ => {}
        }
    }

    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };
    let typed_reasoning = if reasoning_details.is_empty() {
        None
    } else {
        Some(IronReasoningDetails {
            id: reasoning_id,
            content: reasoning_details,
        })
    };

    let finish = resolve_finish_reason(provider_finish, !tool_calls.is_empty());

    (text, tool_calls, finish, reasoning, typed_reasoning)
}

#[derive(Default)]
pub(super) struct StreamedReasoningAccumulator {
    pub(super) parts: Vec<String>,
    pub(super) details: Vec<IronReasoningDetail>,
    pub(super) id: Option<String>,
}

impl StreamedReasoningAccumulator {
    pub(super) fn push_reasoning(&mut self, reasoning: &rig::message::Reasoning) {
        if self.id.is_none() {
            self.id = reasoning.id.clone();
        }
        if let Some(details) = rig_reasoning_to_iron(reasoning) {
            if let Some(display_text) = details.display_text() {
                self.parts.push(display_text);
            }
            self.details.extend(details.content);
        }
    }

    pub(super) fn push_delta(&mut self, id: Option<String>, reasoning: String) {
        if self.id.is_none() {
            self.id = id;
        }
        if !reasoning.trim().is_empty() {
            self.parts.push(reasoning);
        }
    }

    pub(super) fn finish(self) -> (Option<String>, Option<IronReasoningDetails>) {
        let reasoning = if self.parts.is_empty() {
            None
        } else {
            Some(self.parts.join("\n"))
        };
        let details = if self.details.is_empty() {
            None
        } else {
            Some(IronReasoningDetails {
                id: self.id,
                content: self.details,
            })
        };
        (reasoning, details)
    }
}

pub(super) struct DrainedStreamingResponse {
    pub(super) text: Option<String>,
    pub(super) tool_calls: Vec<IronToolCall>,
    pub(super) finish: FinishReason,
    pub(super) reasoning: Option<String>,
    pub(super) reasoning_details: Option<IronReasoningDetails>,
    pub(super) usage: RigUsage,
    pub(super) cache_creation_input_tokens: u32,
}

/// Saturate u64 to u32 for token counts.
pub(super) fn saturate_u32(val: u64) -> u32 {
    val.min(u32::MAX as u64) as u32
}

/// Normalize a tool call name returned by an OpenAI-compatible provider.
///
/// Some proxies (e.g. VibeProxy) prepend `proxy_` to tool names.
/// If the returned name doesn't match any known tool but stripping a
/// `proxy_` prefix yields a match, use the stripped version.
pub(super) fn normalize_tool_name(name: &str, known_tools: &HashSet<String>) -> String {
    if known_tools.contains(name) {
        return name.to_string();
    }

    if let Some(stripped) = name.strip_prefix("proxy_")
        && known_tools.contains(stripped)
    {
        return stripped.to_string();
    }

    name.to_string()
}
