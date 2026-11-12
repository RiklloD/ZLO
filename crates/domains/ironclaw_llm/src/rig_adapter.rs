//! Generic adapter that bridges rig-core's `CompletionModel` trait to IronClaw's `LlmProvider`.
//!
//! This lets us use any rig-core provider (OpenAI, Anthropic, Ollama, etc.) as an
//! `Arc<dyn LlmProvider>` without changing any of the agent, reasoning, or tool code.

use crate::config::CacheRetention;
use async_trait::async_trait;
use futures::StreamExt;
use rig::OneOrMany;
use rig::completion::{
    AssistantContent, CompletionModel, CompletionRequest as RigRequest, GetTokenUsage,
    ToolDefinition as RigToolDefinition, Usage as RigUsage,
};
use rig::message::{
    DocumentSourceKind, Image, ImageDetail, ImageMediaType, Message as RigMessage, MimeType,
    ToolChoice as RigToolChoice, ToolFunction, ToolResult as RigToolResult, ToolResultContent,
    UserContent,
};
use rig::streaming::{StreamedAssistantContent, StreamingCompletionResponse};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use std::collections::HashSet;
use std::str::FromStr;

use crate::error::LlmError;
use crate::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, CompletionResponseFormat,
    CompletionStreamSink, FinishReason, LlmProvider, ReasoningDetail as IronReasoningDetail,
    ReasoningDetails as IronReasoningDetails, ToolCall as IronToolCall, ToolCompletionRequest,
    ToolCompletionResponse, ToolDefinition as IronToolDefinition, map_provider_finish_token,
    normalized_model_override, prompt_cache_key_from_metadata, resolve_finish_reason,
    strip_unsupported_completion_params, strip_unsupported_tool_params,
};
use crate::tool_schema::{ToolSchemaPolicy, shape_tool_schema};
#[cfg(test)]
use crate::tool_schema::{normalize_schema_strict, serialize_json_capped};
use ironclaw_common::llm_costs as costs;

mod convert;
mod errors;
mod models_endpoint;
mod provider_impl;
mod request;

#[cfg(test)]
mod tests;

use self::convert::*;
use self::errors::*;
pub(crate) use self::models_endpoint::*;
pub(crate) use self::request::effective_cache_retention;
use self::request::*;

/// Adapter that wraps a rig-core `CompletionModel` and implements `LlmProvider`.
pub struct RigAdapter<M: CompletionModel> {
    model: M,
    provider_id: String,
    model_name: String,
    input_cost: Decimal,
    output_cost: Decimal,
    /// Prompt cache retention policy (Anthropic only).
    /// When not `CacheRetention::None`, emits explicit `cache_control`
    /// breakpoints (last tool via `additional_params.tools`, plus rig's
    /// system/last-message markers under `Short`) and the top-level
    /// automatic-caching marker — see `build_rig_request` and issue #6984.
    /// Also controls the cost multiplier for cache-creation tokens.
    cache_retention: CacheRetention,
    /// Parameter names that this provider does not support (e.g., `"temperature"`).
    /// These are stripped from requests before sending to avoid 400 errors.
    unsupported_params: HashSet<String>,
    /// Default additional parameters merged into every request.
    /// Used by providers that need extra top-level fields (e.g., Ollama `think: true`).
    default_additional_params: Option<serde_json::Value>,
    /// Provider-specific fallback used when the caller omits `max_tokens`.
    /// Anthropic requires this field on every request; other rig-backed
    /// providers preserve their existing omission semantics by leaving it unset.
    default_max_tokens: Option<u32>,
    /// Whether this provider has been live-validated for rig-core streaming.
    /// Untested rig-backed providers retain the buffered trait fallback.
    native_streaming: bool,
    /// Whether this rig-core provider serializes `CompletionRequest::output_schema`.
    ///
    /// rig-core 0.36's dedicated DeepSeek and OpenRouter request paths accept
    /// the field in the shared request type but omit it from their wire
    /// payloads. Keep that limitation explicit at the adapter boundary rather
    /// than silently dispatching a request whose structured-output contract is
    /// lost.
    structured_output_supported: bool,
    /// Whether this rig-core path is an OpenAI-compatible Chat Completions
    /// adapter that can preserve native `json_object` mode.
    json_object_supported: bool,
    /// Whether this rig-core path is an OpenAI-compatible Chat Completions
    /// adapter that should carry the derived `prompt_cache_key` metadata
    /// value (`crate::provider::PROMPT_CACHE_KEY_METADATA`) onto the wire.
    /// `prompt_cache_key` is an OpenAI-specific concept — Anthropic-protocol
    /// requests use `cache_control` instead — so factories enable this only
    /// for generic OpenAI-compatible, DeepSeek, and OpenRouter clients. See
    /// `prompt_cache_key`.
    prompt_cache_key_supported: bool,
    /// Optional model-discovery endpoint. When set, [`LlmProvider::list_models`]
    /// issues a `GET` instead of returning the empty default. rig-core's
    /// `CompletionModel` does not expose model discovery, so this is wired
    /// explicitly per protocol (OpenAI-compatible, Anthropic, Ollama).
    models_endpoint: Option<ModelsEndpoint>,
    /// How this protocol carries a routed reasoning effort; `None`: it has
    /// no effort knob and the metadata is ignored.
    effort_style: Option<crate::reasoning_effort::EffortStyle>,
}

impl<M: CompletionModel> RigAdapter<M> {
    /// Create a new adapter wrapping the given rig-core model.
    pub fn new(model: M, model_name: impl Into<String>) -> Self {
        let name = model_name.into();
        let (input_cost, output_cost) =
            costs::model_cost(&name).unwrap_or_else(costs::default_cost);
        Self {
            model,
            provider_id: name.clone(),
            model_name: name,
            input_cost,
            output_cost,
            cache_retention: CacheRetention::None,
            unsupported_params: HashSet::new(),
            default_additional_params: None,
            default_max_tokens: None,
            native_streaming: false,
            structured_output_supported: false,
            json_object_supported: false,
            prompt_cache_key_supported: false,
            models_endpoint: None,
            effort_style: None,
        }
    }

    /// Send a routed reasoning effort the way this protocol spells it.
    pub(crate) fn with_effort_style(mut self, style: crate::reasoning_effort::EffortStyle) -> Self {
        self.effort_style = Some(style);
        self
    }

    /// Merge the effort the router put in `metadata` into the request, when
    /// the model takes that level.
    fn apply_reasoning_effort(
        &self,
        rig_req: &mut RigRequest,
        metadata: &std::collections::HashMap<String, String>,
        model_override: Option<&str>,
    ) {
        let Some(style) = self.effort_style else {
            return;
        };
        let Some(effort) = metadata.get(crate::provider_routing::REASONING_EFFORT_METADATA) else {
            return;
        };
        let shape = crate::reasoning_effort::RequestShape {
            temperature: rig_req.temperature.is_some(),
            forced_tool: matches!(
                rig_req.tool_choice,
                Some(RigToolChoice::Required | RigToolChoice::Specific { .. })
            ),
            structured_output: rig_req.output_schema.is_some()
                || rig_req
                    .additional_params
                    .as_ref()
                    .is_some_and(|p| p.get("response_format").is_some()),
        };
        let model = model_override.unwrap_or(&self.model_name);
        let params =
            crate::reasoning_effort::params(style, &self.provider_id, model, effort, shape);
        merge_additional_params(rig_req, params.as_ref());
    }

    /// Set the stable provider identity used in typed provider errors.
    ///
    /// The model name and provider id are distinct payload fields. Keeping this
    /// explicit prevents a missing-model error from reporting the model slug as
    /// its provider.
    pub(crate) fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }

    /// Mark whether this rig-core provider preserves the request's structured
    /// output schema on the wire.
    pub(crate) fn with_structured_output_support(mut self, supported: bool) -> Self {
        self.structured_output_supported = supported;
        self
    }

    /// Mark this adapter as an OpenAI-compatible Chat Completions path that
    /// can serialize the native JSON-object response mode.
    pub(crate) fn with_json_object_support(mut self, supported: bool) -> Self {
        self.json_object_supported = supported;
        self
    }

    /// Mark this adapter as an OpenAI-compatible Chat Completions path that
    /// should carry the derived `prompt_cache_key` metadata value onto the
    /// wire. Must stay `false` for Anthropic-protocol adapters (and any
    /// other non-OpenAI-shape rig client) — `prompt_cache_key` is an OpenAI
    /// concept, unrelated to Anthropic's `cache_control` markers.
    pub(crate) fn with_prompt_cache_key_support(mut self, supported: bool) -> Self {
        self.prompt_cache_key_supported = supported;
        self
    }

    /// Enable model discovery for [`LlmProvider::list_models`].
    ///
    /// Without this, `list_models` falls back to the trait default (an empty
    /// list). The provider factories build a protocol-specific [`ModelsEndpoint`]
    /// (URL, auth scheme, response shape) so the "Fetch models" UI returns the
    /// provider's catalog.
    pub(crate) fn with_model_listing(mut self, endpoint: ModelsEndpoint) -> Self {
        self.models_endpoint = Some(endpoint);
        self
    }

    /// Set Anthropic prompt cache retention policy.
    ///
    /// Controls both cache injection and cost tracking:
    /// - `None` — no caching, no surcharge (1.0×).
    /// - `Short` — 5-minute TTL via `{"type": "ephemeral"}`, 1.25× write surcharge.
    /// - `Long` — 1-hour TTL via `{"type": "ephemeral", "ttl": "1h"}`, 2.0× write surcharge.
    ///
    /// Cache injection combines explicit breakpoints with Anthropic's
    /// automatic caching: `build_rig_request` marks the last tool definition
    /// (via `additional_params.tools`) and injects the top-level
    /// `cache_control` field, while `Short` retention additionally enables
    /// rig's typed system/last-message breakpoints at provider construction
    /// (`create_anthropic_from_registry`). See issue #6984.
    ///
    /// If the configured model does not support caching (e.g. claude-2),
    /// a warning is logged once at construction and caching is disabled.
    pub fn with_cache_retention(mut self, retention: CacheRetention) -> Self {
        self.cache_retention = effective_cache_retention(retention, &self.model_name);
        self
    }

    /// Set the list of unsupported parameter names for this provider.
    ///
    /// Parameters in this set are stripped from requests before sending.
    /// Supported parameter names: `"temperature"`, `"max_tokens"`,
    /// `"stop_sequences"`, `"prompt_cache_key"`.
    pub fn with_unsupported_params(mut self, params: Vec<String>) -> Self {
        self.unsupported_params = params.into_iter().collect();
        self
    }

    /// Set default additional parameters merged into every request.
    ///
    /// These are injected into rig-core's `additional_params` field, which gets
    /// `#[serde(flatten)]`'d into the provider's request payload. Use this for
    /// provider-specific top-level fields like Ollama's `think: true`.
    pub fn with_additional_params(mut self, params: serde_json::Value) -> Self {
        self.default_additional_params = Some(params);
        self
    }

    /// Set a provider-required fallback for requests that omit `max_tokens`.
    pub(crate) fn with_default_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = Some(max_tokens);
        self
    }

    fn max_tokens_or_default(&self, max_tokens: Option<u32>) -> Option<u32> {
        max_tokens.or(self.default_max_tokens)
    }

    /// Strip unsupported fields from a `CompletionRequest` in place.
    fn strip_unsupported_completion_params(&self, req: &mut CompletionRequest) {
        strip_unsupported_completion_params(&self.unsupported_params, req);
    }

    /// Strip unsupported fields from a `ToolCompletionRequest` in place.
    fn strip_unsupported_tool_params(&self, req: &mut ToolCompletionRequest) {
        strip_unsupported_tool_params(&self.unsupported_params, req);
    }

    /// Resolve the derived prompt-cache-key value for this request, gated on
    /// `prompt_cache_key_supported` (OpenAI-compatible Chat Completions only,
    /// never Anthropic) and the `unsupported_params` kill switch — an
    /// operator can list `"prompt_cache_key"` there when a server rejects
    /// the field.
    fn prompt_cache_key(
        &self,
        metadata: &std::collections::HashMap<String, String>,
    ) -> Option<String> {
        if !self.prompt_cache_key_supported {
            return None;
        }
        prompt_cache_key_from_metadata(&self.unsupported_params, metadata)
    }

    fn ensure_structured_output_supported(
        &self,
        requested: Option<&CompletionResponseFormat>,
    ) -> Result<(), LlmError> {
        if matches!(requested, Some(CompletionResponseFormat::JsonSchema(_)))
            && !self.structured_output_supported
        {
            return Err(LlmError::InvalidRequest {
                provider: self.provider_id.clone(),
                reason: "structured output is not supported by this rig-core provider path"
                    .to_string(),
            });
        }
        Ok(())
    }

    async fn drain_streaming_response(
        &self,
        mut stream: StreamingCompletionResponse<M::StreamingResponse>,
        sink: std::sync::Arc<dyn CompletionStreamSink>,
    ) -> Result<DrainedStreamingResponse, LlmError> {
        let mut streamed_reasoning = StreamedReasoningAccumulator::default();
        while let Some(chunk) = stream.next().await {
            match chunk.map_err(|e| map_rig_error_for(&self.provider_id, &self.model_name, e))? {
                StreamedAssistantContent::Text(text) if !text.text.is_empty() => {
                    sink.text_delta(text.text).await;
                }
                StreamedAssistantContent::Reasoning(reasoning) => {
                    streamed_reasoning.push_reasoning(&reasoning);
                }
                StreamedAssistantContent::ReasoningDelta { id, reasoning } => {
                    sink.reasoning_delta(reasoning.clone()).await;
                    streamed_reasoning.push_delta(id, reasoning);
                }
                _ => {}
            }
        }

        // rig-core sets `stream.response` only when the provider yields its
        // terminal frame. Its absence means the stream simply stopped
        // producing items — a dropped connection or an aborted generation —
        // which is the crate's established incomplete-stream condition, not a
        // completed call. Raising it here, before a response object exists,
        // is what lets retry and failover see a failure instead of a success.
        //
        // What this actually detects, in rig-core 0.36: **Ollama only**. Its
        // stream yields `FinalResponse` solely inside `if response.done`, so a
        // stream that stops early genuinely leaves `stream.response` unset.
        // Anthropic and Gemini, plus the shared OpenAI-compatible stream driver
        // used by OpenAI, OpenRouter, and DeepSeek, all yield a usage-only
        // `FinalResponse` after the SSE loop — including after the plain
        // `Err(StreamEnded) => break` arm. On those five a server that closes
        // mid-answer still sets `stream.response` and this check does not fire.
        // Catching that needs a terminal-observed signal rig-core does not
        // carry; it is a known limitation of this adapter, not a claim it makes.
        let Some(terminal_frame) = stream.response.as_ref() else {
            return Err(LlmError::StreamInterrupted {
                provider: self.model_name.clone(),
                reason: "stream ended before the provider's terminal frame".to_string(),
            });
        };
        let usage = terminal_frame.token_usage().unwrap_or_default();
        let raw_terminal_frame = serialize_raw_response(terminal_frame);
        let cache_creation_input_tokens = extract_cache_creation(raw_terminal_frame.as_ref());
        let provider_finish = extract_finish_reason(raw_terminal_frame.as_ref());
        // Note: in rig-core 0.36 only Ollama's terminal frame still carries a
        // finish reason (`done_reason`). The other five define
        // `StreamingResponse` as usage-only, so their per-chunk
        // `finish_reason`/`stop_reason` is dropped before it reaches this
        // adapter and `provider_finish` is `None` — shape inference is the
        // documented fallback there.
        let (text, tool_calls, finish, reasoning, reasoning_details) =
            extract_response(&stream.choice, &usage, provider_finish);
        let (streamed_reasoning, streamed_reasoning_details) = streamed_reasoning.finish();

        Ok(DrainedStreamingResponse {
            text,
            tool_calls,
            finish,
            reasoning: reasoning.or(streamed_reasoning),
            reasoning_details: reasoning_details.or(streamed_reasoning_details),
            usage,
            cache_creation_input_tokens,
        })
    }
}
