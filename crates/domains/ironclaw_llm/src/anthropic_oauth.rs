//! Anthropic Messages API provider (direct HTTP, native SSE streaming).
//!
//! One implementation serves both Anthropic credentials:
//!
//! - **Console API key** (`ANTHROPIC_API_KEY`): `x-api-key` +
//!   `anthropic-version`.
//! - **`claude login` OAuth token**: `Authorization: Bearer` plus the OAuth
//!   beta flag, re-read from the OS credential store once on a 401.
//!
//! rig-core's Anthropic client is deliberately not used for either: it
//! hardcodes `x-api-key` (rejected for OAuth tokens), and its stream drops the
//! provider `stop_reason` and synthesizes a terminal frame after EOF, so a
//! truncated answer is indistinguishable from a completed one. Here a stream
//! is complete only after `message_stop` or an explicit stop reason, the exact
//! stop reason survives, and the idle deadline is re-armed only by semantic
//! model progress.
//!
//! Pattern follows `nearai_chat.rs`: direct HTTP calls via `reqwest::Client`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::anthropic_thinking::{AnthropicThinking, thinking_for_request};
use crate::config::{CacheRetention, RegistryProviderConfig};
use crate::error::{LlmError, ProductionModelAdapter};
use crate::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, CompletionResponseFormat,
    CompletionStreamSink, ContentPart, LlmProvider, ReasoningDetail, ReasoningDetails, Role,
    ToolCall, ToolCompletionRequest, ToolCompletionResponse, ToolDefinition,
    strip_unsupported_completion_params, strip_unsupported_tool_params,
};
use ironclaw_common::llm_costs as costs;

mod stream;
#[cfg(test)]
use self::stream::AnthropicStreamingToolCall;
use self::stream::{AnthropicStreamingResponse, ingest_anthropic_event};

/// Read a fresh `claude login` OAuth token from the OS credential store.
///
/// Mirrors `ironclaw::config::ClaudeCodeConfig::extract_oauth_token` but is
/// inlined here so this crate doesn't depend on the main binary. Used to
/// retry once after a 401 if the user's OAuth token has been rotated by
/// Claude Code's background refresh.
fn refresh_claude_oauth_token() -> Option<String> {
    if cfg!(target_os = "macos") {
        match std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                if let Ok(json) = String::from_utf8(output.stdout) {
                    return parse_oauth_access_token(json.trim());
                }
            }
            _ => {}
        }
    }
    if let Some(home) = dirs::home_dir() {
        let creds_path = home.join(".claude").join(".credentials.json");
        if let Ok(json) = std::fs::read_to_string(&creds_path) {
            return parse_oauth_access_token(&json);
        }
    }
    None
}

fn parse_oauth_access_token(json: &str) -> Option<String> {
    let creds: serde_json::Value = serde_json::from_str(json).ok()?;
    let token = creds["claudeAiOauth"]["accessToken"].as_str()?;
    if !token.starts_with("sk-ant-oat") {
        tracing::debug!("Ignoring credential store token with unexpected prefix");
        return None;
    }
    Some(token.to_string())
}
/// Map an HTTP error status + response body to a context-length error when it
/// indicates the prompt exceeded the model's context window.
///
/// Returns `Some(LlmError::ContextLengthExceeded { .. })` for HTTP 413 or for
/// an HTTP 400 whose body matches a context-overflow pattern, and `None`
/// otherwise. Delegates to the shared `crate::error::context_length_error`
/// helper so detection stays consistent across direct-HTTP providers.
#[cfg(test)]
fn context_length_error_for_status(status_code: u16, response_text: &str) -> Option<LlmError> {
    crate::error::context_length_error(status_code, response_text)
}

const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// OAuth beta requires 2023-06-01; the 2024-10-22 version is not valid with the beta flag.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Required beta flag to enable OAuth Bearer auth on api.anthropic.com.
/// Without this header, the API returns 401 "OAuth authentication is currently not supported."
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Provider identity the OAuth transport has always reported.
const OAUTH_PROVIDER_ID: &str = "anthropic_oauth";

/// How requests authenticate against the Messages API.
enum AnthropicAuth {
    /// `claude login` OAuth token, wrapped in a `RwLock` so it can be updated
    /// after a successful credential-store refresh (fixes #1136: stale token
    /// reuse after expiry).
    OAuth(std::sync::RwLock<SecretString>),
    /// Anthropic Console API key, sent as `x-api-key`.
    ApiKey(SecretString),
}

/// The credential one HTTP attempt carries.
enum AttemptCredential {
    OAuth(SecretString),
    ApiKey(SecretString),
}

impl AttemptCredential {
    fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = match self {
            Self::OAuth(token) => builder
                .bearer_auth(token.expose_secret())
                .header("anthropic-beta", ANTHROPIC_OAUTH_BETA),
            Self::ApiKey(key) => builder.header("x-api-key", key.expose_secret()),
        };
        builder
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("Content-Type", "application/json")
    }
}

/// Anthropic Messages API provider (API key or OAuth Bearer).
pub(crate) struct AnthropicProvider {
    client: Client,
    streaming_client: Client,
    /// Bounds response headers and the gap between semantic stream progress.
    /// There is no total wall-clock cap on a streaming response.
    stream_idle_timeout: Duration,
    auth: AnthropicAuth,
    /// Identity reported in typed errors (`anthropic_oauth` for OAuth, the
    /// registry provider id for API keys).
    provider_id: String,
    adapter: ProductionModelAdapter,
    model: String,
    base_url: Option<String>,
    active_model: std::sync::RwLock<String>,
    /// Parameter names that this provider does not support.
    unsupported_params: HashSet<String>,
    /// Anthropic prompt-cache retention; drives the explicit `cache_control`
    /// breakpoints (system prompt, last tool, last message block) on every
    /// request, buffered and streamed. See #6984.
    cache_retention: CacheRetention,
    /// `GET {base}/v1/models` discovery (API-key credential only; the OAuth
    /// beta does not serve the models endpoint).
    models_endpoint: Option<crate::rig_adapter::ModelsEndpoint>,
}

/// Everything one `LlmProvider` entry point contributes to a Messages
/// request, after the model override is resolved and unsupported params are
/// stripped.
struct MessagesRequestInput {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    tools: Vec<ToolDefinition>,
    tool_choice: Option<String>,
    response_format: Option<CompletionResponseFormat>,
    metadata: HashMap<String, String>,
}

impl AnthropicProvider {
    /// OAuth (`claude login`) credential: Bearer auth with the OAuth beta.
    pub(crate) fn new_oauth(
        config: &RegistryProviderConfig,
        request_timeout_secs: u64,
    ) -> Result<Self, LlmError> {
        let token = config
            .oauth_token
            .clone()
            .ok_or_else(|| LlmError::AuthFailed {
                provider: OAUTH_PROVIDER_ID.to_string(),
            })?;
        Self::build(
            config,
            request_timeout_secs,
            AnthropicAuth::OAuth(std::sync::RwLock::new(token)),
            (
                OAUTH_PROVIDER_ID.to_string(),
                ProductionModelAdapter::AnthropicOauth,
            ),
            None,
        )
    }

    /// Console API-key credential: `x-api-key` auth, model discovery wired.
    pub(crate) fn new_api_key(
        config: &RegistryProviderConfig,
        request_timeout_secs: u64,
    ) -> Result<Self, LlmError> {
        let api_key = config.api_key.clone().ok_or_else(|| LlmError::AuthFailed {
            provider: config.provider_id.clone(),
        })?;
        // Anthropic model discovery: `GET {base}/v1/models` with `x-api-key` +
        // `anthropic-version`.
        let discovery_base = versioned_base_url(config.base_url.as_str());
        let models_endpoint = crate::rig_adapter::ModelsEndpoint {
            provider_id: config.provider_id.clone(),
            url: format!("{discovery_base}/models"),
            auth: crate::rig_adapter::ModelsAuth::AnthropicKey {
                api_key: api_key.expose_secret().to_string(),
                version: ANTHROPIC_API_VERSION.to_string(),
            },
            shape: crate::rig_adapter::ModelsShape::OpenAiData,
            extra_headers: reqwest::header::HeaderMap::new(),
        };
        Self::build(
            config,
            request_timeout_secs,
            AnthropicAuth::ApiKey(api_key),
            (
                config.provider_id.clone(),
                ProductionModelAdapter::Anthropic,
            ),
            Some(models_endpoint),
        )
    }

    fn build(
        config: &RegistryProviderConfig,
        request_timeout_secs: u64,
        auth: AnthropicAuth,
        (provider_id, adapter): (String, ProductionModelAdapter),
        models_endpoint: Option<crate::rig_adapter::ModelsEndpoint>,
    ) -> Result<Self, LlmError> {
        // The proxy-aware builder bypasses a system proxy for a loopback
        // (self-hosted Anthropic-compatible) endpoint; remote hosts keep the
        // default proxy behavior.
        let target = if config.base_url.is_empty() {
            ANTHROPIC_DEFAULT_BASE_URL
        } else {
            config.base_url.as_str()
        };
        let client = crate::url_check::build_http_client(
            &provider_id,
            target,
            crate::config::hardened_client_builder(request_timeout_secs),
        )?;
        let streaming_client = crate::url_check::build_http_client(
            &provider_id,
            target,
            crate::config::hardened_streaming_client_builder(),
        )?;
        let base_url = (!config.base_url.is_empty()).then(|| config.base_url.clone());
        let cache_retention =
            crate::rig_adapter::effective_cache_retention(config.cache_retention, &config.model);

        Ok(Self {
            client,
            streaming_client,
            stream_idle_timeout: Duration::from_secs(request_timeout_secs),
            auth,
            provider_id,
            adapter,
            model: config.model.clone(),
            base_url,
            active_model: std::sync::RwLock::new(config.model.clone()),
            unsupported_params: config.unsupported_params.iter().cloned().collect(),
            cache_retention,
            models_endpoint,
        })
    }

    fn api_url(&self) -> String {
        let base = self
            .base_url
            .as_deref()
            .unwrap_or(ANTHROPIC_DEFAULT_BASE_URL)
            .trim_end_matches('/');
        format!("{base}/v1/messages")
    }

    fn current_credential(&self) -> AttemptCredential {
        match &self.auth {
            AnthropicAuth::ApiKey(key) => AttemptCredential::ApiKey(key.clone()),
            AnthropicAuth::OAuth(token) => AttemptCredential::OAuth(match token.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }),
        }
    }

    /// Update the stored OAuth token after a successful credential-store
    /// refresh. API keys are never rotated in place.
    fn update_token(&self, new_token: SecretString) {
        if let AnthropicAuth::OAuth(token) = &self.auth {
            match token.write() {
                Ok(mut guard) => *guard = new_token,
                Err(poisoned) => *poisoned.into_inner() = new_token,
            }
        }
    }

    /// For an OAuth credential, wait for Claude Code's asynchronous
    /// credential-store write to settle (fixes the race in #1136) and re-read
    /// the token. API keys have nothing to refresh.
    async fn refreshed_oauth_token(&self) -> Option<SecretString> {
        if !matches!(self.auth, AnthropicAuth::OAuth(_)) {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        refresh_claude_oauth_token().map(SecretString::from)
    }

    fn http_error(&self, status: u16, body: &str, retry_after: Option<Duration>) -> LlmError {
        crate::error::map_provider_http_error(crate::error::ProviderHttpError {
            adapter: self.adapter,
            model: &self.active_model_name(),
            status,
            body,
            retry_after,
        })
    }

    fn completion_input(&self, mut req: CompletionRequest) -> MessagesRequestInput {
        let model = req
            .take_model_override()
            .unwrap_or_else(|| self.active_model_name());
        strip_unsupported_completion_params(&self.unsupported_params, &mut req);
        MessagesRequestInput {
            model,
            messages: req.messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            tools: Vec::new(),
            tool_choice: None,
            response_format: req.response_format,
            metadata: req.metadata,
        }
    }

    fn tool_input(&self, mut req: ToolCompletionRequest) -> MessagesRequestInput {
        let model = req
            .take_model_override()
            .unwrap_or_else(|| self.active_model_name());
        strip_unsupported_tool_params(&self.unsupported_params, &mut req);
        MessagesRequestInput {
            model,
            messages: req.messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            tools: req.tools,
            tool_choice: req.tool_choice,
            response_format: req.response_format,
            metadata: req.metadata,
        }
    }

    /// Build one Messages request. All four entry points go through here so
    /// buffered and streamed calls carry identical cache breakpoints,
    /// thinking, and effort.
    fn build_request(
        &self,
        input: MessagesRequestInput,
        stream: bool,
    ) -> Result<AnthropicRequest, LlmError> {
        let MessagesRequestInput {
            model,
            messages,
            max_tokens,
            temperature,
            tools,
            tool_choice,
            response_format,
            metadata,
        } = input;
        let max_tokens = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let has_tools = !tools.is_empty();
        let forced_tool = has_tools
            && tool_choice
                .as_deref()
                .is_some_and(|choice| !matches!(choice, "auto" | "none"));
        let mut output_config =
            anthropic_output_config(response_format.as_ref(), &self.provider_id)?;

        // A routed reasoning effort (`output_config.effort`, plus adaptive
        // thinking when the request shape allows it) — the same mapping the
        // picker advertises for the Anthropic protocol.
        let effort = metadata
            .get(crate::provider_routing::REASONING_EFFORT_METADATA)
            .and_then(|effort| {
                crate::reasoning_effort::params(
                    crate::reasoning_effort::EffortStyle::Anthropic,
                    &self.provider_id,
                    &model,
                    effort,
                    crate::reasoning_effort::RequestShape {
                        temperature: temperature.is_some(),
                        forced_tool,
                        structured_output: output_config.is_some(),
                    },
                )
            });
        let effort_thinking = effort
            .as_ref()
            .is_some_and(|params| params.get("thinking").is_some());
        if let Some(level) = effort
            .as_ref()
            .and_then(|params| params["output_config"]["effort"].as_str())
        {
            output_config
                .get_or_insert_with(AnthropicOutputConfig::default)
                .effort = Some(level.to_string());
        }
        let adaptive = || AnthropicThinking::Adaptive {
            display: Some("summarized"),
        };
        let thinking = if has_tools {
            // Tool turns think only when the router asked for an effort and
            // the tool loop in progress can replay the signed thinking block
            // Anthropic requires at the head of its last assistant message.
            (effort_thinking && thinking_replay_safe(&messages)).then(adaptive)
        } else if effort_thinking {
            Some(adaptive())
        } else {
            thinking_for_request(&model, max_tokens, temperature, false)
        };

        let (system, messages) = convert_messages(messages, thinking.is_some());
        let mut request = AnthropicRequest {
            stream,
            model,
            messages,
            system: system.map(AnthropicSystem::Text),
            max_tokens,
            temperature,
            thinking,
            tools: has_tools.then(|| convert_anthropic_tools(tools)),
            tool_choice: convert_anthropic_tool_choice(tool_choice),
            output_config,
        };
        apply_cache_breakpoints(&mut request, self.cache_retention);
        Ok(request)
    }

    async fn send_request<R: for<'de> Deserialize<'de>>(
        &self,
        body: &AnthropicRequest,
    ) -> Result<R, LlmError> {
        let url = self.api_url();
        tracing::debug!(provider = %self.provider_id, "Sending request to Anthropic: {}", url);

        let credential = self.current_credential();
        let response = credential
            .apply(self.client.post(&url))
            .json(body)
            .send()
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: self.provider_id.clone(),
                reason: e.to_string(),
            })?;

        let status = response.status();
        let response = if status.as_u16() == 401 {
            // OAuth tokens from `claude login` expire in ~8-12h. Attempt to
            // re-extract a fresh token from the OS credential store
            // (macOS Keychain / Linux credentials file) before giving up.
            let Some(fresh_token) = self.refreshed_oauth_token().await else {
                return Err(LlmError::AuthFailed {
                    provider: self.provider_id.clone(),
                });
            };
            let retry = AttemptCredential::OAuth(fresh_token.clone())
                .apply(self.client.post(&url))
                .json(body)
                .send()
                .await
                .map_err(|e| LlmError::RequestFailed {
                    provider: self.provider_id.clone(),
                    reason: e.to_string(),
                })?;
            if retry.status().is_success() {
                // Persist the refreshed token so subsequent requests don't
                // hit 401 again (fixes #1136).
                self.update_token(fresh_token);
                tracing::info!("Anthropic OAuth token refreshed from credential store");
            } else {
                tracing::warn!(
                    "Anthropic OAuth 401 retry with refreshed token also failed ({})",
                    retry.status()
                );
            }
            retry
        } else {
            response
        };

        let status = response.status();
        if !status.is_success() {
            // Parse Retry-After header before consuming the body.
            let retry_after = crate::retry::retry_after_for_status(
                status.as_u16(),
                response.headers().get("retry-after"),
            );
            let response_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
            return Err(self.http_error(status.as_u16(), &response_text, retry_after));
        }

        let response_text = response.text().await.map_err(|e| LlmError::RequestFailed {
            provider: self.provider_id.clone(),
            reason: format!("Failed to read response body: {}", e),
        })?;

        tracing::debug!(
            "Anthropic response: status={}, bytes={}",
            status,
            response_text.len()
        );

        serde_json::from_str(&response_text).map_err(|e| {
            let truncated = ironclaw_common::truncate_for_preview(&response_text, 512);
            LlmError::InvalidResponse {
                provider: self.provider_id.clone(),
                reason: format!("JSON parse error: {}. Raw: {}", e, truncated),
            }
        })
    }

    /// One absolute deadline for response headers / the next semantic
    /// progress, `stream_idle_timeout` from now.
    fn progress_deadline(&self) -> Result<tokio::time::Instant, LlmError> {
        tokio::time::Instant::now()
            .checked_add(self.stream_idle_timeout)
            .ok_or_else(|| LlmError::RequestFailed {
                provider: self.provider_id.clone(),
                reason: "stream progress deadline overflowed".to_string(),
            })
    }

    async fn send_streaming_request(
        &self,
        body: &AnthropicRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<AnthropicStreamingResponse, LlmError> {
        let url = self.api_url();
        let mut deadline = self.progress_deadline()?;
        let mut response = self
            .send_streaming_http_request(&url, body, &self.current_credential(), deadline)
            .await?;

        if response.status().as_u16() == 401 && matches!(self.auth, AnthropicAuth::OAuth(_)) {
            drop(response);
            // Match the buffered request path: allow Claude Code's
            // asynchronous credential-store refresh to settle, then retry
            // exactly once.
            let Some(fresh_token) = self.refreshed_oauth_token().await else {
                return Err(LlmError::AuthFailed {
                    provider: self.provider_id.clone(),
                });
            };
            deadline = self.progress_deadline()?;
            response = self
                .send_streaming_http_request(
                    &url,
                    body,
                    &AttemptCredential::OAuth(fresh_token.clone()),
                    deadline,
                )
                .await?;
            if response.status().is_success() {
                self.update_token(fresh_token);
                tracing::info!("Anthropic OAuth token refreshed from credential store");
            }
        }

        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::retry::retry_after_for_status(
                status.as_u16(),
                response.headers().get("retry-after"),
            );
            if status.as_u16() == 401 {
                return Err(LlmError::AuthFailed {
                    provider: self.provider_id.clone(),
                });
            }
            // silent-ok: the bounded provider error body is diagnostic only;
            // status-based classification remains authoritative if reading it fails.
            let response_body = tokio::time::timeout(
                Duration::from_secs(5),
                crate::error::read_bounded_provider_error_body(response),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
            let response_text = String::from_utf8_lossy(&response_body);
            return Err(self.http_error(status.as_u16(), response_text.as_ref(), retry_after));
        }

        let mut events = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| error.to_string()))
            .eventsource();
        let mut streamed = AnthropicStreamingResponse::for_provider(self.provider_id.clone());
        loop {
            let next = tokio::time::timeout_at(deadline, events.next())
                .await
                .map_err(|_| LlmError::StreamInterrupted {
                    provider: self.provider_id.clone(),
                    reason: format!(
                        "SSE stream made no model progress for {} seconds",
                        self.stream_idle_timeout.as_secs()
                    ),
                })?;
            let Some(event) = next else {
                break;
            };
            let event = event.map_err(|error| LlmError::StreamInterrupted {
                provider: self.provider_id.clone(),
                reason: format!("Failed to read SSE stream: {error}"),
            })?;
            // Only semantic progress re-arms the deadline: `ping`,
            // `message_start`, and usage-only frames do not keep a stalled
            // stream alive.
            if ingest_anthropic_event(&mut streamed, &event.event, &event.data, sink.as_ref())
                .await?
            {
                deadline = self.progress_deadline()?;
            }
            if streamed.terminal {
                break;
            }
        }
        if !streamed.terminal {
            return Err(LlmError::StreamInterrupted {
                provider: self.provider_id.clone(),
                reason: "stream ended before message_stop or a stop reason".to_string(),
            });
        }
        streamed.finish()
    }

    async fn send_streaming_http_request(
        &self,
        url: &str,
        body: &AnthropicRequest,
        credential: &AttemptCredential,
        deadline: tokio::time::Instant,
    ) -> Result<reqwest::Response, LlmError> {
        tokio::time::timeout_at(
            deadline,
            credential
                .apply(self.streaming_client.post(url))
                .header("Accept", "text/event-stream")
                .json(body)
                .send(),
        )
        .await
        .map_err(|_| LlmError::RequestFailed {
            provider: self.provider_id.clone(),
            reason: format!(
                "timed out waiting {}s for streaming response headers",
                self.stream_idle_timeout.as_secs()
            ),
        })?
        .map_err(|e| LlmError::RequestFailed {
            provider: self.provider_id.clone(),
            reason: e.to_string(),
        })
    }
}

/// `{base}/v1` for an Anthropic base URL that may or may not already carry
/// the version segment.
fn versioned_base_url(base_url: &str) -> String {
    let base = if base_url.trim().is_empty() {
        ANTHROPIC_DEFAULT_BASE_URL
    } else {
        base_url.trim().trim_end_matches('/')
    };
    if base.ends_with("/v1") || base.contains("/v1/") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// Whether a thinking-enabled request can satisfy Anthropic's tool-loop rule:
/// when the conversation is mid tool loop (the last assistant message made
/// tool calls and no new user message followed), that assistant message must
/// start with its signed thinking block. History from a non-thinking turn, or
/// from another provider, carries none — thinking stays off for that request.
fn thinking_replay_safe(messages: &[ChatMessage]) -> bool {
    let Some(last_assistant) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return true;
    };
    if messages[last_assistant + 1..]
        .iter()
        .any(|m| m.role == Role::User)
    {
        return true;
    }
    let message = &messages[last_assistant];
    if message.tool_calls.as_ref().is_none_or(Vec::is_empty) {
        return true;
    }
    message
        .reasoning_details
        .as_ref()
        .is_some_and(|details| !replayable_thinking_blocks(details).is_empty())
}

/// The thinking blocks a later request may replay: signed thinking text and
/// redacted thinking. Unsigned text and summaries are display-only.
fn replayable_thinking_blocks(details: &ReasoningDetails) -> Vec<AnthropicContentBlock> {
    details
        .content
        .iter()
        .filter_map(|detail| match detail {
            ReasoningDetail::Text {
                text,
                signature: Some(signature),
            } if !signature.is_empty() => Some(AnthropicContentBlock::Thinking {
                thinking: text.clone(),
                signature: signature.clone(),
            }),
            ReasoningDetail::Redacted { data } if !data.is_empty() => {
                Some(AnthropicContentBlock::RedactedThinking { data: data.clone() })
            }
            _ => None,
        })
        .collect()
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_id(&self) -> String {
        self.provider_id.clone()
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        match self.models_endpoint.as_ref() {
            Some(endpoint) => endpoint.fetch_models().await,
            None => Ok(Vec::new()),
        }
    }

    fn cache_write_multiplier(&self) -> Decimal {
        match self.cache_retention {
            CacheRetention::None => Decimal::ONE,
            CacheRetention::Short => Decimal::new(125, 2), // 1.25× (125% of input rate)
            CacheRetention::Long => Decimal::TWO,          // 2.0×  (200% of input rate)
        }
    }

    fn cache_read_discount(&self) -> Decimal {
        if self.cache_retention != CacheRetention::None {
            Decimal::TEN // 90% discount (cost = input_rate / 10)
        } else {
            Decimal::ONE
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let request = self.build_request(self.completion_input(req), false)?;
        let response: AnthropicResponse = self.send_request(&request).await?;
        let extracted = extract_response_content(&response);
        Ok(CompletionResponse {
            content: extracted.content.unwrap_or_default(),
            finish_reason: stream::map_anthropic_finish_reason(
                response.stop_reason.as_deref(),
                false,
            ),
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            reasoning: extracted.reasoning,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            usage_source: ironclaw_host_api::resource::ModelUsageSource::Provider,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        })
    }

    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<CompletionResponse, LlmError> {
        let request = self.build_request(self.completion_input(req), true)?;
        let response = self.send_streaming_request(&request, sink).await?;
        Ok(CompletionResponse {
            finish_reason: response.finish_reason(),
            content: response.content,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            reasoning: response.reasoning,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            usage_source: ironclaw_host_api::resource::ModelUsageSource::Provider,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        })
    }

    async fn complete_with_tools(
        &self,
        req: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let request = self.build_request(self.tool_input(req), false)?;
        let response: AnthropicResponse = self.send_request(&request).await?;
        let extracted = extract_response_content(&response);
        Ok(ToolCompletionResponse {
            finish_reason: stream::map_anthropic_finish_reason(
                response.stop_reason.as_deref(),
                !extracted.tool_calls.is_empty(),
            ),
            content: extracted.content,
            tool_calls: extracted.tool_calls,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            reasoning: extracted.reasoning,
            reasoning_details: extracted.reasoning_details,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        })
    }

    async fn complete_with_tools_streaming(
        &self,
        req: ToolCompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let request = self.build_request(self.tool_input(req), true)?;
        let response = self.send_streaming_request(&request, sink).await?;
        Ok(ToolCompletionResponse {
            finish_reason: response.finish_reason(),
            content: (!response.content.is_empty()).then_some(response.content),
            tool_calls: response.tool_calls,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
            reasoning: response.reasoning,
            reasoning_details: response.reasoning_details,
        })
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        let model = self.active_model_name();
        costs::model_cost(&model).unwrap_or_else(costs::default_cost)
    }

    fn active_model_name(&self) -> String {
        match self.active_model.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        match self.active_model.write() {
            Ok(mut guard) => {
                *guard = model.to_string();
            }
            Err(poisoned) => {
                *poisoned.into_inner() = model.to_string();
            }
        }
        Ok(())
    }
}

// --- Anthropic Messages API types ---

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    stream: bool,
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<AnthropicSystem>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
}

#[derive(Debug, Default, Serialize)]
struct AnthropicOutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<AnthropicOutputFormat>,
    /// Routed reasoning effort (`low` … `max`) for models that take one.
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct AnthropicOutputFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
    schema: serde_json::Value,
}

fn anthropic_output_config(
    response_format: Option<&CompletionResponseFormat>,
    provider: &str,
) -> Result<Option<AnthropicOutputConfig>, LlmError> {
    match response_format {
        None => Ok(None),
        Some(CompletionResponseFormat::JsonSchema(response_format)) => {
            Ok(Some(AnthropicOutputConfig {
                format: Some(AnthropicOutputFormat {
                    format_type: "json_schema",
                    schema: response_format.schema.clone(),
                }),
                effort: None,
            }))
        }
        Some(CompletionResponseFormat::JsonObject) => Err(LlmError::InvalidRequest {
            provider: provider.to_string(),
            reason:
                "native JSON-object response mode is not supported by the Anthropic Messages API"
                    .to_string(),
        }),
    }
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

/// Anthropic system prompt: a plain string (legacy wire shape, kept when
/// caching is off) or content blocks so the last block can carry a
/// `cache_control` breakpoint.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AnthropicSystem {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

#[derive(Debug, Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    block_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
}

/// Anthropic content can be a simple string or a list of content blocks.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    #[serde(rename = "image")]
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    /// A signed thinking block replayed verbatim at the head of a tool-loop
    /// assistant message (Anthropic rejects the turn otherwise).
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

impl AnthropicContentBlock {
    /// Stamp a `cache_control` breakpoint on this block.
    fn set_cache_control(&mut self, marker: serde_json::Value) {
        match self {
            Self::Text { cache_control, .. }
            | Self::Image { cache_control, .. }
            | Self::ToolUse { cache_control, .. }
            | Self::ToolResult { cache_control, .. } => *cache_control = Some(marker),
            // Thinking blocks cannot carry `cache_control`; they are never the
            // trailing block of a message (tool_use always follows them).
            Self::Thinking { .. } | Self::RedactedThinking { .. } => {}
        }
    }
}

/// Inline base64 image source for an Anthropic `image` content block.
#[derive(Debug, Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: &'static str,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    choice_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        #[serde(default)]
        data: Option<String>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct AnthropicUsage {
    #[serde(default)]
    pub(super) input_tokens: u32,
    #[serde(default)]
    pub(super) output_tokens: u32,
    #[serde(default)]
    pub(super) cache_creation_input_tokens: u32,
    #[serde(default)]
    pub(super) cache_read_input_tokens: u32,
}

/// Build Anthropic `image` content blocks from a user message's multimodal
/// parts. Only inline base64 `data:` images are forwarded (the Anthropic
/// messages API also accepts `url` sources, but the model gateway always emits
/// `data:` URLs); anything else is skipped so the text still reaches the model.
fn user_image_blocks(parts: &[ContentPart]) -> Vec<AnthropicContentBlock> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ImageUrl { image_url } => {
                let (media_type, data) = image_url.decode_data_url()?;
                Some(AnthropicContentBlock::Image {
                    cache_control: None,
                    source: AnthropicImageSource {
                        source_type: "base64",
                        media_type: media_type.to_string(),
                        data: data.to_string(),
                    },
                })
            }
            ContentPart::Text { .. } => None,
        })
        .collect()
}

/// Place the explicit Anthropic `cache_control` breakpoints (issue #6984):
/// the system prompt, the last tool definition, and the last content block of
/// the last message. Mirrors pi's placement so the tool/system prefix and the
/// growing conversation each cache independently. All markers carry the same
/// TTL, satisfying Anthropic's longer-TTL-first ordering rule. No-op when
/// retention is `None`, preserving the legacy wire shape (plain-string
/// system, no markers).
fn apply_cache_breakpoints(
    request: &mut AnthropicRequest,
    retention: crate::config::CacheRetention,
) {
    let Some(marker) = retention.cache_control_json() else {
        return;
    };

    // convert_messages only ever produces the string form, but restore any
    // other value untouched rather than dropping it on the floor.
    request.system = match request.system.take() {
        Some(AnthropicSystem::Text(text)) => {
            Some(AnthropicSystem::Blocks(vec![AnthropicSystemBlock {
                block_type: "text",
                text,
                cache_control: Some(marker.clone()),
            }]))
        }
        other => other,
    };

    if let Some(tools) = request.tools.as_mut()
        && let Some(last) = tools.last_mut()
    {
        last.cache_control = Some(marker.clone());
    }

    if let Some(last_message) = request.messages.last_mut() {
        match &mut last_message.content {
            // Empty text blocks cannot carry cache_control (API rejects
            // them), so an empty trailing message keeps the string form.
            AnthropicContent::Text(text) if !text.is_empty() => {
                last_message.content =
                    AnthropicContent::Blocks(vec![AnthropicContentBlock::Text {
                        text: std::mem::take(text),
                        cache_control: Some(marker),
                    }]);
            }
            AnthropicContent::Text(_) => {}
            AnthropicContent::Blocks(blocks) => {
                if let Some(last_block) = blocks.last_mut() {
                    last_block.set_cache_control(marker);
                }
            }
        }
    }
}

/// Anthropic takes native JSON Schema `input_schema`s with optional fields,
/// so tools get the flatten-only policy: top-level `oneOf`/`anyOf`/`allOf`
/// (which the API rejects) are flattened, but optional fields are never
/// rewritten into OpenAI strict-mode required-nullable form.
fn convert_anthropic_tools(tools: Vec<ToolDefinition>) -> Vec<AnthropicTool> {
    tools
        .into_iter()
        .map(|tool| {
            let mut description = tool.description;
            let input_schema = crate::tool_schema::shape_tool_schema(
                crate::tool_schema::ToolSchemaPolicy::FlattenOnly,
                &tool.parameters,
                &mut description,
            );
            AnthropicTool {
                name: tool.name,
                description,
                input_schema,
                // Markers are applied per-request by `apply_cache_breakpoints`;
                // the legacy shape carries none.
                cache_control: None,
            }
        })
        .collect()
}

fn convert_anthropic_tool_choice(choice: Option<String>) -> Option<AnthropicToolChoice> {
    choice.map(|choice| match choice.as_str() {
        "auto" => AnthropicToolChoice {
            choice_type: "auto".to_string(),
            name: None,
        },
        "required" => AnthropicToolChoice {
            choice_type: "any".to_string(),
            name: None,
        },
        "none" => AnthropicToolChoice {
            choice_type: "none".to_string(),
            name: None,
        },
        specific => AnthropicToolChoice {
            choice_type: "tool".to_string(),
            name: Some(specific.to_string()),
        },
    })
}

/// Convert ChatMessage list to Anthropic format.
///
/// Extracts system messages to the top-level `system` parameter (Anthropic
/// doesn't allow system messages in the `messages` array). Tool-call/tool-result
/// pairs are converted to content blocks. When `replay_thinking` is set (the
/// request enables thinking), assistant tool-call messages lead with the
/// signed thinking blocks captured from the response that produced them.
fn convert_messages(
    messages: Vec<ChatMessage>,
    replay_thinking: bool,
) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut anthropic_msgs: Vec<AnthropicMessage> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if !msg.content.is_empty() {
                    system_parts.push(msg.content);
                }
            }
            Role::HostReminder => {
                // Host guidance rides the conversation tail, so it lands right
                // after the tool results of the iteration that produced it
                // (#6985). Anthropic rejects consecutive user messages — the
                // same constraint the tool-result arm below merges for — so
                // fold the reminder into the trailing user message. Appending
                // keeps any tool_result blocks first in that message, which
                // Anthropic also requires.
                match anthropic_msgs.last_mut() {
                    Some(last) if last.role == "user" => match &mut last.content {
                        AnthropicContent::Text(text) => {
                            text.push_str("\n\n");
                            text.push_str(&msg.content);
                        }
                        AnthropicContent::Blocks(blocks) => {
                            blocks.push(AnthropicContentBlock::Text {
                                text: msg.content,
                                cache_control: None,
                            });
                        }
                    },
                    _ => anthropic_msgs.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: AnthropicContent::Text(msg.content),
                    }),
                }
            }
            Role::User => {
                let content = match user_image_blocks(&msg.content_parts) {
                    // Text-only (or no inline images): keep the compact string form.
                    blocks if blocks.is_empty() => AnthropicContent::Text(msg.content),
                    // Multimodal: text block first (when present), then images.
                    image_blocks => {
                        let mut blocks = Vec::with_capacity(1 + image_blocks.len());
                        if !msg.content.is_empty() {
                            blocks.push(AnthropicContentBlock::Text {
                                text: msg.content,
                                cache_control: None,
                            });
                        }
                        blocks.extend(image_blocks);
                        AnthropicContent::Blocks(blocks)
                    }
                };
                anthropic_msgs.push(AnthropicMessage {
                    role: "user".to_string(),
                    content,
                });
            }
            Role::Assistant => {
                if let Some(tool_calls) = msg.tool_calls {
                    // Assistant message with tool calls → content blocks
                    let mut blocks: Vec<AnthropicContentBlock> = match &msg.reasoning_details {
                        Some(details) if replay_thinking => replayable_thinking_blocks(details),
                        _ => Vec::new(),
                    };
                    if !msg.content.is_empty() {
                        blocks.push(AnthropicContentBlock::Text {
                            text: msg.content,
                            cache_control: None,
                        });
                    }
                    for tc in tool_calls {
                        blocks.push(AnthropicContentBlock::ToolUse {
                            id: tc.id,
                            name: tc.name,
                            input: tc.arguments,
                            cache_control: None,
                        });
                    }
                    anthropic_msgs.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                } else {
                    anthropic_msgs.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicContent::Text(msg.content),
                    });
                }
            }
            Role::Tool => {
                let Some(tool_call_id) = msg.tool_call_id else {
                    tracing::warn!("Skipping Tool message without tool_call_id");
                    continue;
                };
                // Tool results go into a user message with tool_result blocks
                let block = AnthropicContentBlock::ToolResult {
                    tool_use_id: tool_call_id,
                    content: msg.content,
                    cache_control: None,
                };
                // If the last message is already a user message of *only*
                // tool-result blocks, append to it (Anthropic requires
                // consecutive tool results in one user message). Crucially, do
                // not merge into a multimodal user prompt (text + image
                // blocks) — that would fold a tool result into a different
                // conversational turn.
                if let Some(last) = anthropic_msgs.last_mut()
                    && last.role == "user"
                    && let AnthropicContent::Blocks(ref mut blocks) = last.content
                    && blocks
                        .iter()
                        .all(|b| matches!(b, AnthropicContentBlock::ToolResult { .. }))
                {
                    blocks.push(block);
                    continue;
                }
                anthropic_msgs.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(vec![block]),
                });
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, anthropic_msgs)
}

#[derive(Debug, Default)]
struct ExtractedAnthropicResponse {
    content: Option<String>,
    reasoning: Option<String>,
    /// Signed/redacted thinking blocks, in response order, for replay on the
    /// next tool-loop request.
    reasoning_details: Option<ReasoningDetails>,
    tool_calls: Vec<ToolCall>,
}

/// Extract text content and tool calls from an Anthropic response.
fn extract_response_content(response: &AnthropicResponse) -> ExtractedAnthropicResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut details: Vec<ReasoningDetail> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in &response.content {
        match block {
            AnthropicResponseBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            AnthropicResponseBlock::Thinking {
                thinking,
                summary,
                signature,
            } => {
                if let Some(signature) = signature.as_deref().filter(|s| !s.is_empty()) {
                    details.push(ReasoningDetail::Text {
                        text: thinking.clone().unwrap_or_default(),
                        signature: Some(signature.to_string()),
                    });
                }
                if let Some(reasoning) = summary
                    .as_deref()
                    .filter(|summary| !summary.trim().is_empty())
                    .or_else(|| {
                        thinking
                            .as_deref()
                            .filter(|thinking| !thinking.trim().is_empty())
                    })
                {
                    reasoning_parts.push(reasoning.to_string());
                }
            }
            AnthropicResponseBlock::RedactedThinking { data } => {
                if let Some(data) = data.as_deref().filter(|data| !data.is_empty()) {
                    details.push(ReasoningDetail::Redacted {
                        data: data.to_string(),
                    });
                }
            }
            AnthropicResponseBlock::Other => {}
            AnthropicResponseBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                    reasoning: None,
                    signature: None,
                    arguments_parse_error: None,
                });
            }
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    ExtractedAnthropicResponse {
        content,
        reasoning,
        reasoning_details: (!details.is_empty()).then_some(ReasoningDetails {
            id: None,
            content: details,
        }),
        tool_calls,
    }
}

// The transport/cache-breakpoint test suite lives in its own file so this
// one stays inside the file-size budget: `src/anthropic_oauth/tests.rs`.
#[cfg(test)]
mod tests;
