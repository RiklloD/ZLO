//! Anthropic Messages API SSE accumulation, shared by both Anthropic
//! credentials (API key and `claude login` OAuth).
//!
//! The accumulator is the authority for the streamed response: text,
//! reasoning (including signed thinking blocks for replay), tool calls, usage,
//! and the exact provider `stop_reason`. [`ingest_anthropic_event`] reports
//! whether an event was *semantic progress* so the transport can re-arm its
//! idle deadline only on real model output — `ping`, `message_start`, and
//! usage-only frames never keep a stalled stream alive.

use std::collections::BTreeMap;

use crate::error::LlmError;
use crate::provider::{
    CompletionStreamSink, FinishReason, ReasoningDetail, ReasoningDetails, ToolCall,
    map_provider_finish_token, resolve_finish_reason,
};

use super::AnthropicUsage;

#[derive(Debug, Default)]
pub(super) struct AnthropicStreamingToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) input_json: String,
}

/// One streamed thinking content block, kept in stream order so a signed
/// block can be replayed verbatim on the next tool-loop request.
#[derive(Debug)]
enum StreamedThinking {
    Thinking { text: String, signature: String },
    Redacted { data: String },
}

#[derive(Debug, Default)]
pub(super) struct AnthropicStreamingResponse {
    provider: String,
    pub(super) content: String,
    reasoning_parts: Vec<String>,
    pub(super) tool_call_parts: BTreeMap<u64, AnthropicStreamingToolCall>,
    thinking_blocks: BTreeMap<u64, StreamedThinking>,
    pub(super) reasoning: Option<String>,
    pub(super) reasoning_details: Option<ReasoningDetails>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) stop_reason: Option<String>,
    pub(super) usage: AnthropicUsage,
    pub(super) terminal: bool,
}

impl AnthropicStreamingResponse {
    /// An empty accumulator whose errors name `provider`.
    pub(super) fn for_provider(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            ..Self::default()
        }
    }

    pub(super) fn finish(mut self) -> Result<Self, LlmError> {
        for state in self.tool_call_parts.values() {
            if state.id.is_empty() || state.name.is_empty() {
                return Err(LlmError::InvalidResponse {
                    provider: self.provider.clone(),
                    reason: "streamed tool_use block is missing its id or name".to_string(),
                });
            }
        }
        let provider = self.provider.clone();
        self.tool_calls = std::mem::take(&mut self.tool_call_parts)
            .into_values()
            .map(|state| {
                let arguments = if state.input_json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&state.input_json).map_err(|error| {
                        LlmError::InvalidResponse {
                            provider: provider.clone(),
                            reason: format!("streamed tool arguments are invalid JSON: {error}"),
                        }
                    })?
                };
                Ok(ToolCall {
                    id: state.id,
                    name: state.name,
                    arguments,
                    reasoning: None,
                    signature: None,
                    arguments_parse_error: None,
                })
            })
            .collect::<Result<Vec<_>, LlmError>>()?;
        self.reasoning = (!self.reasoning_parts.is_empty()).then(|| self.reasoning_parts.join(""));
        let details: Vec<ReasoningDetail> = std::mem::take(&mut self.thinking_blocks)
            .into_values()
            .filter_map(|block| match block {
                StreamedThinking::Thinking { text, signature } => {
                    (!text.is_empty() || !signature.is_empty()).then(|| ReasoningDetail::Text {
                        text,
                        signature: (!signature.is_empty()).then_some(signature),
                    })
                }
                StreamedThinking::Redacted { data } => {
                    (!data.is_empty()).then_some(ReasoningDetail::Redacted { data })
                }
            })
            .collect();
        self.reasoning_details = (!details.is_empty()).then_some(ReasoningDetails {
            id: None,
            content: details,
        });
        Ok(self)
    }

    /// The tool call a just-closed `tool_use` block at `index` streamed, when
    /// it is complete: id and name present and arguments valid JSON. `None`
    /// for any other block or a malformed one — [`Self::finish`] stays the
    /// authority that rejects malformed calls for the whole response.
    fn ready_tool_call(&self, index: u64) -> Option<ToolCall> {
        let state = self.tool_call_parts.get(&index)?;
        if state.id.is_empty() || state.name.is_empty() {
            return None;
        }
        let arguments = if state.input_json.is_empty() {
            serde_json::json!({})
        } else {
            // silent-ok: a malformed block is not announced early; `finish`
            // rejects it as InvalidResponse for the whole stream.
            serde_json::from_str(&state.input_json).ok()?
        };
        Some(ToolCall {
            id: state.id.clone(),
            name: state.name.clone(),
            arguments,
            reasoning: None,
            signature: None,
            arguments_parse_error: None,
        })
    }

    /// The finish reason this stream reported: the provider's exact
    /// `stop_reason`, refined to `ToolUse` only when it was a clean stop
    /// that carried tool calls (or said nothing at all).
    pub(super) fn finish_reason(&self) -> FinishReason {
        map_anthropic_finish_reason(self.stop_reason.as_deref(), !self.tool_calls.is_empty())
    }
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

/// Fold one SSE event into `response`. Returns `true` when the event carried
/// semantic model progress (text, reasoning, a signature, a tool-call
/// id/name/argument fragment, or the terminal stop reason).
pub(super) async fn ingest_anthropic_event(
    response: &mut AnthropicStreamingResponse,
    event_name: &str,
    data: &str,
    sink: &dyn CompletionStreamSink,
) -> Result<bool, LlmError> {
    let body: serde_json::Value =
        serde_json::from_str(data).map_err(|error| LlmError::InvalidResponse {
            provider: response.provider.clone(),
            reason: format!(
                "stream JSON parse error: {error}. Raw: {}",
                ironclaw_common::truncate_for_preview(data, 512)
            ),
        })?;
    let event_type = json_str(&body, "type").unwrap_or(event_name);
    let index = body
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let mut progress = false;
    match event_type {
        "message_start" => {
            if let Some(usage) = body.get("message").and_then(|message| message.get("usage")) {
                update_anthropic_usage(&mut response.usage, usage);
            }
        }
        "content_block_start" => {
            let block = body
                .get("content_block")
                .unwrap_or(&serde_json::Value::Null);
            match json_str(block, "type") {
                Some("text") => {
                    if let Some(text) = json_str(block, "text").filter(|text| !text.is_empty()) {
                        response.content.push_str(text);
                        sink.text_delta(text.to_string()).await;
                        progress = true;
                    }
                }
                Some("tool_use") => {
                    let state = AnthropicStreamingToolCall {
                        id: json_str(block, "id").unwrap_or_default().to_string(),
                        name: json_str(block, "name").unwrap_or_default().to_string(),
                        input_json: String::new(),
                    };
                    progress = !state.id.is_empty() || !state.name.is_empty();
                    response.tool_call_parts.insert(index, state);
                }
                Some("thinking") => {
                    let text = json_str(block, "thinking").unwrap_or_default().to_string();
                    if !text.is_empty() {
                        response.reasoning_parts.push(text.clone());
                        sink.reasoning_delta(text.clone()).await;
                        progress = true;
                    }
                    response.thinking_blocks.insert(
                        index,
                        StreamedThinking::Thinking {
                            text,
                            signature: json_str(block, "signature").unwrap_or_default().to_string(),
                        },
                    );
                }
                Some("redacted_thinking") => {
                    let data = json_str(block, "data").unwrap_or_default().to_string();
                    progress = !data.is_empty();
                    response
                        .thinking_blocks
                        .insert(index, StreamedThinking::Redacted { data });
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            let delta = body.get("delta").unwrap_or(&serde_json::Value::Null);
            match json_str(delta, "type") {
                Some("text_delta") => {
                    if let Some(text) = json_str(delta, "text").filter(|text| !text.is_empty()) {
                        response.content.push_str(text);
                        sink.text_delta(text.to_string()).await;
                        progress = true;
                    }
                }
                Some("thinking_delta") => {
                    if let Some(thinking) =
                        json_str(delta, "thinking").filter(|thinking| !thinking.is_empty())
                    {
                        response.reasoning_parts.push(thinking.to_string());
                        sink.reasoning_delta(thinking.to_string()).await;
                        if let StreamedThinking::Thinking { text, .. } = response
                            .thinking_blocks
                            .entry(index)
                            .or_insert_with(|| StreamedThinking::Thinking {
                                text: String::new(),
                                signature: String::new(),
                            })
                        {
                            text.push_str(thinking);
                        }
                        progress = true;
                    }
                }
                Some("signature_delta") => {
                    if let Some(fragment) =
                        json_str(delta, "signature").filter(|fragment| !fragment.is_empty())
                    {
                        if let StreamedThinking::Thinking { signature, .. } = response
                            .thinking_blocks
                            .entry(index)
                            .or_insert_with(|| StreamedThinking::Thinking {
                                text: String::new(),
                                signature: String::new(),
                            })
                        {
                            signature.push_str(fragment);
                        }
                        progress = true;
                    }
                }
                Some("input_json_delta") => {
                    if let Some(partial) =
                        json_str(delta, "partial_json").filter(|partial| !partial.is_empty())
                    {
                        response
                            .tool_call_parts
                            .entry(index)
                            .or_default()
                            .input_json
                            .push_str(partial);
                        progress = true;
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            // A closed `tool_use` block is announced while the stream is
            // still open so callers can overlap read-only tool I/O with the
            // rest of generation. Not semantic progress: no new model output.
            if let Some(call) = response.ready_tool_call(index) {
                sink.tool_call_ready(call).await;
            }
        }
        "message_delta" => {
            if let Some(stop_reason) = body
                .get("delta")
                .and_then(|delta| json_str(delta, "stop_reason"))
            {
                response.stop_reason = Some(stop_reason.to_string());
                response.terminal = true;
                progress = true;
            }
            if let Some(usage) = body.get("usage") {
                update_anthropic_usage(&mut response.usage, usage);
            }
        }
        "message_stop" => {
            response.terminal = true;
            progress = true;
        }
        "error" => {
            let reason = body
                .get("error")
                .and_then(|error| json_str(error, "message"))
                .unwrap_or("Anthropic streaming error");
            return Err(LlmError::RequestFailed {
                provider: response.provider.clone(),
                reason: reason.to_string(),
            });
        }
        _ => {}
    }
    Ok(progress)
}

pub(super) fn update_anthropic_usage(usage: &mut AnthropicUsage, value: &serde_json::Value) {
    let read = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .map(|tokens| tokens.min(u32::MAX as u64) as u32)
    };
    if let Some(tokens) = read("input_tokens") {
        usage.input_tokens = tokens;
    }
    if let Some(tokens) = read("output_tokens") {
        usage.output_tokens = tokens;
    }
    if let Some(tokens) = read("cache_creation_input_tokens") {
        usage.cache_creation_input_tokens = tokens;
    }
    if let Some(tokens) = read("cache_read_input_tokens") {
        usage.cache_read_input_tokens = tokens;
    }
}

/// Map Anthropic's `stop_reason` through the crate-wide finish-token table
/// (`refusal` → `ContentFilter`, `max_tokens` → `Length`, `pause_turn` →
/// `Unknown`, …) so the provider's own word survives exactly. A missing
/// `stop_reason` is `Unknown` (or `ToolUse` when tool calls arrived) — never a
/// guessed clean `Stop`.
pub(super) fn map_anthropic_finish_reason(
    reason: Option<&str>,
    has_tool_calls: bool,
) -> FinishReason {
    match reason.and_then(map_provider_finish_token) {
        None if has_tool_calls => FinishReason::ToolUse,
        None => FinishReason::Unknown,
        reported => resolve_finish_reason(reported, has_tool_calls),
    }
}
