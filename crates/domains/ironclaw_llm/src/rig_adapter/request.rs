use super::*;

/// Downgrade a requested cache retention to `None` for models without
/// prompt-cache support. Shared by both Anthropic transports so the
/// construction-time decision (rig `prompt_caching` flag, OAuth breakpoint
/// application) always agrees with `with_cache_retention`'s validation. This
/// is the single chokepoint for the downgrade decision and its logging —
/// `with_cache_retention` delegates here instead of duplicating the check.
pub(crate) fn effective_cache_retention(
    retention: CacheRetention,
    model_name: &str,
) -> CacheRetention {
    if retention != CacheRetention::None && !supports_prompt_cache(model_name) {
        tracing::warn!(
            model = %model_name,
            "Prompt caching requested but model does not support it; disabling"
        );
        CacheRetention::None
    } else {
        retention
    }
}

/// Returns `true` if the model supports Anthropic prompt caching.
///
/// Denylist, not allowlist: Anthropic documents only the `claude-2*` and
/// `claude-instant*` families as lacking prompt-cache support. Every other
/// Claude model family — including ones released after this code was
/// written — supports caching by default, so new families never need to be
/// added here to keep caching working.
pub(super) fn supports_prompt_cache(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Strip optional provider prefix (e.g. "anthropic/claude-...")
    let model = lower.strip_prefix("anthropic/").unwrap_or(&lower);
    // Must be a Claude model at all, and not one of the documented
    // unsupported legacy families.
    model.starts_with("claude")
        && !model.starts_with("claude-2")
        && !model.starts_with("claude-instant")
}

/// Serialize the raw provider response to JSON **once** per completion.
///
/// rig-core's unified types drop fields we need (`cache_creation_input_tokens`,
/// the provider's own finish reason), so both are read back out of the raw
/// body. Serializing the whole response — generated text, tool payloads and
/// all — is response-sized work, so every call site does it once and lends the
/// resulting `Value` to each extractor rather than each extractor serializing
/// for itself.
///
/// silent-ok: serializing a body rig already deserialized from the provider
/// cannot fail in practice. If it ever does, the extractors fall back to their
/// documented defaults rather than failing an otherwise-good response — but the
/// branch is logged, because it silently re-enables the guess this machinery
/// removes: a truncated response would then report `Stop`.
pub(super) fn serialize_raw_response<T: Serialize>(raw: &T) -> Option<serde_json::Value> {
    match serde_json::to_value(raw) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(
                %error,
                "raw provider response would not serialize; falling back to \
                 response-shape inference and zero cache-creation tokens"
            );
            None
        }
    }
}

/// Extract `cache_creation_input_tokens` from the serialized provider response.
///
/// Rig-core's unified `Usage` does not surface this field, but Anthropic's raw
/// response includes it at `usage.cache_creation_input_tokens`.
pub(super) fn extract_cache_creation(raw: Option<&serde_json::Value>) -> u32 {
    raw.and_then(|v| v.get("usage")?.get("cache_creation_input_tokens")?.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

/// Extract the provider's own finish/stop reason from the serialized response.
///
/// rig-core's unified `AssistantContent` says nothing about *why* generation
/// stopped, so a `max_tokens` truncation and a content-filter refusal used to
/// be indistinguishable from a clean answer. Every provider states it in its
/// raw body, so read the field the provider actually emits.
///
/// Returns `None` when the response carries no finish-reason field at all
/// (unknown or older provider shape); callers then fall back to inferring it
/// from the response shape. See [`resolve_finish_reason`].
pub(super) fn extract_finish_reason(raw: Option<&serde_json::Value>) -> Option<FinishReason> {
    let token = provider_finish_token(raw?)?;
    map_provider_finish_token(token)
}

/// Locate the provider's finish-reason field across the response shapes rig fronts.
///
/// Paths verified against rig-core 0.36:
/// - OpenAI-shaped (`openai`, `openai_compatible`, `tinfoil`, `azure`,
///   `deepseek`, `openrouter`): `choices[0].finish_reason`
/// - Anthropic-by-key: `stop_reason`
/// - Ollama: `done_reason`
/// - Gemini by API key: `candidates[0].finishReason`, falling back to
///   `promptFeedback.blockReason`
///
/// Gemini blocks in two different places. A candidate's `finishReason` means
/// the *output* was cut off mid-generation. `promptFeedback.blockReason` means
/// the *input* prompt was rejected before generation, and rig's own
/// `GenerateContentResponse` documents the consequence: it returns no
/// candidates at all only when something was wrong with the prompt. Reading
/// just the candidate path therefore found nothing for a blocked prompt and
/// left the caller to infer a clean `Stop` from the response shape. The
/// candidate's own reason still wins when both are present.
pub(super) fn provider_finish_token(value: &serde_json::Value) -> Option<&str> {
    let openai = value
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(|reason| reason.as_str());
    if openai.is_some() {
        return openai;
    }
    if let Some(anthropic) = value.get("stop_reason").and_then(|r| r.as_str()) {
        return Some(anthropic);
    }
    if let Some(ollama) = value.get("done_reason").and_then(|r| r.as_str()) {
        return Some(ollama);
    }
    let gemini_candidate = value
        .get("candidates")
        .and_then(|candidates| candidates.as_array())
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(|reason| reason.as_str());
    if gemini_candidate.is_some() {
        return gemini_candidate;
    }
    // `blockReason` speaks the same `SCREAMING_SNAKE_CASE` vocabulary as
    // `finishReason`, so the shared table classifies it without a second
    // mapping: `SAFETY` / `BLOCKLIST` / `PROHIBITED_CONTENT` / `IMAGE_SAFETY` /
    // `MODEL_ARMOR` are content blocks, `OTHER` and `BLOCK_REASON_UNSPECIFIED`
    // are unknown. An absent `blockReason` still means the provider said
    // nothing, which is the documented shape-inference fallback.
    value
        .get("promptFeedback")
        .and_then(|feedback| feedback.get("blockReason"))
        .and_then(|reason| reason.as_str())
}

/// Merge default additional parameters into the rig-core request.
///
/// Provider-level params (e.g., Ollama's `think: true`) are merged into the
/// request's `additional_params`. Existing keys from `build_rig_request`
/// (e.g., `cache_control`) take precedence over defaults.
pub(super) fn merge_additional_params(
    rig_req: &mut RigRequest,
    defaults: Option<&serde_json::Value>,
) {
    let Some(defaults) = defaults else { return };
    let Some(default_obj) = defaults.as_object() else {
        return;
    };
    match rig_req.additional_params {
        Some(ref mut params) => {
            if let Some(obj) = params.as_object_mut() {
                for (k, v) in default_obj {
                    obj.entry(k).or_insert_with(|| v.clone());
                }
            }
        }
        None => {
            rig_req.additional_params = Some(defaults.clone());
        }
    }
}

/// Build a rig-core CompletionRequest from our internal types.
///
/// When `cache_retention` is not `None`, emits Anthropic cache markers
/// (issue #6984):
///
/// - A top-level `cache_control` field via `additional_params` (rig-core's
///   `AnthropicCompletionRequest` uses `#[serde(flatten)]` on
///   `additional_params`, so the field lands at the request root). This is
///   Anthropic's **automatic caching**: the API places a breakpoint at the
///   last cacheable block and moves it forward as conversations grow.
/// - An explicit breakpoint on the **last tool definition**, so the tool
///   prefix stays cached even when later parts of the prompt change. rig's
///   typed `ToolDefinition` cannot carry `cache_control`, so the last tool
///   is moved into `additional_params.tools` in Anthropic's native shape —
///   rig appends raw `additional_params.tools` entries after the typed
///   tools, preserving order.
///
/// The system-prompt and last-message-block breakpoints are rig's own
/// `prompt_caching` flag, set at provider construction for `Short` retention
/// only: rig's typed markers are always plain 5m ephemeral, and a 5m block
/// marker combined with a 1h automatic marker is rejected by the API (TTL
/// conflict on the last block), so `Long` relies on the automatic marker for
/// the conversation tail.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_rig_request(
    preamble: Option<String>,
    mut history: Vec<RigMessage>,
    mut tools: Vec<RigToolDefinition>,
    tool_choice: Option<RigToolChoice>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    cache_retention: CacheRetention,
) -> Result<RigRequest, LlmError> {
    // rig-core requires at least one message in chat_history
    if history.is_empty() {
        history.push(RigMessage::user("Hello"));
    }

    let chat_history = OneOrMany::many(history).map_err(|e| LlmError::RequestFailed {
        provider: "rig".to_string(),
        reason: format!("Failed to build chat history: {}", e),
    })?;

    let additional_params = match cache_retention.cache_control_json() {
        None => None,
        Some(marker) => {
            let mut params = serde_json::json!({ "cache_control": marker.clone() });
            if let Some(last_tool) = tools.pop() {
                // Anthropic-native tool shape: rig maps `parameters` →
                // `input_schema` only for typed tools; raw entries pass
                // through verbatim.
                params["tools"] = serde_json::json!([{
                    "name": last_tool.name,
                    "description": last_tool.description,
                    "input_schema": last_tool.parameters,
                    "cache_control": marker,
                }]);
            }
            Some(params)
        }
    };

    Ok(RigRequest {
        preamble,
        chat_history,
        documents: Vec::new(),
        tools,
        temperature: temperature.map(round_f32_to_f64),
        max_tokens: max_tokens.map(|t| t as u64),
        tool_choice,
        additional_params,
        model: None,
        output_schema: None,
    })
}

/// Inject a per-request model override into the typed rig request field.
///
/// Rig-core providers use this field in preference to their configured model.
/// Keeping it out of flattened `additional_params` ensures the serialized JSON
/// request has exactly one top-level `model` key.
pub(super) fn inject_model_override(rig_req: &mut RigRequest, model_override: Option<&str>) {
    rig_req.model = model_override.map(str::to_owned);
}

/// Convert the provider-neutral response schema into rig-core's typed schema.
///
/// All four completion paths use the same request field, so keep conversion
/// and its stable error mapping in one helper rather than letting a path drift.
pub(super) fn set_output_schema(
    rig_req: &mut RigRequest,
    response_format: Option<CompletionResponseFormat>,
    provider_id: &str,
    json_object_supported: bool,
) -> Result<(), LlmError> {
    match response_format {
        None => {}
        Some(CompletionResponseFormat::JsonSchema(format)) => {
            // rig-core derives the OpenAI `json_schema.name` from the root
            // schema's `title` and otherwise falls back to
            // `response_schema`.  The provider-neutral contract carries its
            // name separately, so copy that identity into the annotation
            // rig-core reads before handing the schema over.  `title` is a
            // JSON Schema annotation and does not change the schema's
            // validation semantics; overwriting a stale title is required so
            // the durable response-format name remains authoritative.
            let mut schema = format.schema;
            let object = schema
                .as_object_mut()
                .ok_or_else(|| LlmError::InvalidRequest {
                    provider: provider_id.to_string(),
                    reason: "JSON Schema response format must be an object".to_string(),
                })?;
            object.insert("title".to_string(), serde_json::Value::String(format.name));
            rig_req.output_schema =
                Some(
                    serde_json::from_value(schema).map_err(|error| LlmError::InvalidRequest {
                        provider: provider_id.to_string(),
                        reason: format!("invalid JSON Schema response format: {error}"),
                    })?,
                );
        }
        Some(CompletionResponseFormat::JsonObject) => {
            if !json_object_supported {
                return Err(LlmError::InvalidRequest {
                    provider: provider_id.to_string(),
                    reason:
                        "native JSON-object response mode is not supported by this provider path"
                            .to_string(),
                });
            }
            let params = rig_req
                .additional_params
                .get_or_insert_with(|| serde_json::json!({}));
            let object = params
                .as_object_mut()
                .ok_or_else(|| LlmError::InvalidRequest {
                    provider: provider_id.to_string(),
                    reason: "provider additional parameters must be a JSON object".to_string(),
                })?;
            object.insert(
                "response_format".to_string(),
                serde_json::json!({"type": "json_object"}),
            );
        }
    }
    Ok(())
}
