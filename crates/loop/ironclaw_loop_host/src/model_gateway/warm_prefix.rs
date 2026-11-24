//! Live warm-prefix recording and cache-forked replay.
//!
//! The provider prompt cache is keyed on the exact leading bytes of a
//! request — `tools[]` first, then the system prompt, then the message
//! history. A cache-forked compaction only hits that cache when it replays
//! those bytes unchanged, so the gateway records the exact provider-bound
//! request of every successful live loop call (post conversion, observation
//! masking, and redaction) in a bounded in-memory [`WarmPrefixCache`], and
//! replays a cut of it for [`HostManagedModelGateway::stream_cache_fork`]
//! with only the summarization instruction appended at the tail.
//!
//! The cache is a cache: it never holds durable state, eviction loses
//! nothing, and a miss makes the compaction take its standalone path. A
//! recording is only ever served back to the run that made it (`run_id`),
//! so a thread id reused across scopes can never fork another scope's
//! conversation.
//!
//! [`HostManagedModelGateway::stream_cache_fork`]: crate::HostManagedModelGateway::stream_cache_fork

use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex, PoisonError},
};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_llm::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmProvider, Role,
    ToolCompletionRequest, ToolCompletionResponse, ToolDefinition,
    contains_codex_text_tool_call_syntax,
};
use ironclaw_loop_contracts::{
    ModelProfileId, SystemInferenceCacheFork, SystemInferenceContextMessage,
    SystemInferenceContextRole, SystemInferenceWarmPrefixId,
};
use ironclaw_turns::{LoopMessageRef, TurnRunId};
use tracing::debug;

use super::{
    ModelCallCacheUsage, PromptCacheCallScope, map_provider_completion_error,
    missing_route_snapshot_error, redaction::redact_completion_request, request_model_override,
    resolve_fallback_route, response_to_host_reply, slot_for_model_profile,
    snapshot_from_host_request, system_prompt_cache_signature, tool_definitions_cache_signature,
    validate_provider_model_binding_matches_route, with_model_diagnostic_evidence,
};
use crate::{
    HostManagedCacheForkRequest, HostManagedModelError, HostManagedModelErrorKind,
    HostManagedModelResponse, HostManagedModelRouteSnapshot,
};

/// Where one host message ends in the provider-bound message list.
///
/// `provider_message_count` is the number of leading provider messages that
/// cover this host message and everything before it, or `None` when the
/// host message sits inside a converted unit that extends past it (a tool
/// round whose later results belong to the same provider turn) — cutting
/// there would split an assistant tool call from its results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WarmPrefixBoundary {
    pub(super) content_ref: LoopMessageRef,
    pub(super) provider_message_count: Option<usize>,
}

/// Bounded, in-memory cache of the latest live provider request per thread.
///
/// Shared between the model gateway that records it and the compaction
/// warm-prefix source that reads it
/// ([`crate::WarmPrefixCacheCompactionSource`]). Least-recently-recorded
/// threads are evicted first; recordings above the byte cap are not kept.
#[derive(Debug)]
pub struct WarmPrefixCache {
    max_entries: usize,
    max_recording_bytes: usize,
    entries: Mutex<VecDeque<Arc<WarmPrefixRecording>>>,
}

impl WarmPrefixCache {
    /// Threads tracked at once.
    pub const DEFAULT_MAX_ENTRIES: usize = 32;
    /// Largest single recording kept (message text plus tool schemas).
    pub const DEFAULT_MAX_RECORDING_BYTES: usize = 4 * 1024 * 1024;

    pub fn new() -> Self {
        Self::with_limits(Self::DEFAULT_MAX_ENTRIES, Self::DEFAULT_MAX_RECORDING_BYTES)
    }

    pub fn with_limits(max_entries: usize, max_recording_bytes: usize) -> Self {
        Self {
            max_entries,
            max_recording_bytes,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Number of threads with a recording.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub(crate) fn latest(&self, thread_id: &ThreadId) -> Option<Arc<WarmPrefixRecording>> {
        self.lock()
            .iter()
            .find(|recording| &recording.thread_id == thread_id)
            .cloned()
    }

    fn record(&self, recording: WarmPrefixRecording) {
        if self.max_entries == 0 {
            return;
        }
        let mut entries = self.lock();
        // A newer request for the thread always replaces the older one, even
        // when the newer one is too large to keep: an older prefix is stale.
        entries.retain(|existing| existing.thread_id != recording.thread_id);
        if recording.approximate_bytes() > self.max_recording_bytes {
            debug!(
                max_recording_bytes = self.max_recording_bytes,
                "warm prefix recording exceeds the cache byte cap; not kept"
            );
            return;
        }
        entries.push_back(Arc::new(recording));
        while entries.len() > self.max_entries {
            entries.pop_front();
        }
    }

    /// Look up the recording a fork names and validate the fork against it.
    pub(crate) fn admit_fork(
        &self,
        request: &HostManagedCacheForkRequest,
    ) -> Result<(Arc<WarmPrefixRecording>, usize), HostManagedModelError> {
        let recording = self.latest(&request.thread_id).ok_or_else(|| {
            HostManagedModelError::safe(
                HostManagedModelErrorKind::Unavailable,
                "warm prefix is no longer recorded",
            )
        })?;
        let cut = recording.admit_fork(request)?;
        Ok((recording, cut))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<WarmPrefixRecording>>> {
        // A panicked writer leaves at worst a stale cache entry; every reader
        // re-validates a recording before replaying it.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for WarmPrefixCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The exact provider request of one successful live loop call.
#[derive(Debug)]
pub(crate) struct WarmPrefixRecording {
    pub(crate) prefix_id: SystemInferenceWarmPrefixId,
    thread_id: ThreadId,
    pub(crate) run_id: TurnRunId,
    model_profile_id: ModelProfileId,
    resolved_model_route: Option<HostManagedModelRouteSnapshot>,
    fallback_index: u32,
    /// Messages, model, sampling options, and metadata (including the
    /// thread-scoped `prompt_cache_key`) exactly as sent.
    completion: CompletionRequest,
    /// Provider `tools[]` exactly as sent; empty for a text-only call.
    tools: Vec<ToolDefinition>,
    boundaries: Vec<WarmPrefixBoundary>,
}

impl WarmPrefixRecording {
    fn approximate_bytes(&self) -> usize {
        let messages = self
            .completion
            .messages
            .iter()
            .map(|message| message.content.len())
            .fold(0_usize, usize::saturating_add);
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                tool.name
                    .len()
                    .saturating_add(tool.description.len())
                    .saturating_add(tool.parameters.to_string().len())
            })
            .fold(0_usize, usize::saturating_add);
        messages.saturating_add(tools)
    }

    pub(crate) fn fallback_index(&self) -> u32 {
        self.fallback_index
    }

    pub(crate) fn provider_model(&self) -> Option<&str> {
        self.completion.model.as_deref()
    }

    /// The provider message count that ends right after the last of
    /// `covered` — the compacted durable messages — provided every one of
    /// them is in the recorded prefix and the cut does not split a tool round.
    pub(crate) fn cut_covering(&self, covered: &[LoopMessageRef]) -> Option<usize> {
        if covered.is_empty() {
            return None;
        }
        let required = covered
            .iter()
            .map(LoopMessageRef::as_str)
            .collect::<HashSet<_>>();
        let mut seen = HashSet::with_capacity(required.len());
        let mut last_covered = None;
        for boundary in &self.boundaries {
            if required.contains(boundary.content_ref.as_str()) {
                seen.insert(boundary.content_ref.as_str());
                last_covered = Some(boundary);
            }
        }
        if seen.len() != required.len() {
            debug!(
                required = required.len(),
                present = seen.len(),
                "warm prefix does not contain every compacted message"
            );
            return None;
        }
        let cut = last_covered?.provider_message_count?;
        (cut >= 2 && cut <= self.completion.messages.len()).then_some(cut)
    }

    fn is_valid_cut(&self, cut: usize) -> bool {
        cut >= 2
            && cut <= self.completion.messages.len()
            && self
                .boundaries
                .iter()
                .any(|boundary| boundary.provider_message_count == Some(cut))
    }

    /// The role-preserving text view of the first `cut` provider messages:
    /// what the compaction task scans, and what a fork request must echo back.
    /// `None` when the prefix cannot be represented faithfully as text (no
    /// leading system prompt, or multimodal parts).
    pub(crate) fn scan_view(
        &self,
        cut: usize,
    ) -> Option<(String, Vec<SystemInferenceContextMessage>)> {
        let messages = self.completion.messages.get(..cut)?;
        let (system, history) = messages.split_first()?;
        if system.role != Role::System || !system.content_parts.is_empty() {
            return None;
        }
        let mut view = Vec::with_capacity(history.len());
        for message in history {
            if !message.content_parts.is_empty() {
                return None;
            }
            let (role, content) = match message.role {
                Role::System | Role::HostReminder => {
                    (SystemInferenceContextRole::System, message.content.clone())
                }
                Role::User => (SystemInferenceContextRole::User, message.content.clone()),
                Role::Tool => (SystemInferenceContextRole::Tool, message.content.clone()),
                Role::Assistant => (
                    SystemInferenceContextRole::Assistant,
                    assistant_scan_text(message),
                ),
            };
            view.push(SystemInferenceContextMessage { role, content });
        }
        Some((system.content.clone(), view))
    }

    pub(crate) fn cache_fork(&self, cut: usize) -> Option<SystemInferenceCacheFork> {
        Some(SystemInferenceCacheFork {
            prefix_id: self.prefix_id.clone(),
            replayed_message_count: u32::try_from(cut).ok()?,
        })
    }

    /// Validate a fork request against this recording and return the cut.
    pub(crate) fn admit_fork(
        &self,
        request: &HostManagedCacheForkRequest,
    ) -> Result<usize, HostManagedModelError> {
        let mismatch = |what: &'static str| {
            debug!(what, "cache fork request does not match the warm prefix");
            HostManagedModelError::safe(
                HostManagedModelErrorKind::Unavailable,
                "warm prefix no longer matches the cache fork request",
            )
        };
        if self.prefix_id != request.fork.prefix_id {
            return Err(mismatch("prefix_id"));
        }
        if self.run_id != request.run_id || self.thread_id != request.thread_id {
            return Err(mismatch("run"));
        }
        // Same model, same route, same fallback position: anything else
        // cannot hit the warm KV cache.
        if self.model_profile_id != request.model_profile_id
            || self.resolved_model_route != request.resolved_model_route
            || self.fallback_index != request.fallback_index
        {
            return Err(mismatch("model"));
        }
        let cut = usize::try_from(request.fork.replayed_message_count).map_err(|error| {
            debug!(%error, "cache fork message count does not fit the platform");
            mismatch("message_count")
        })?;
        if !self.is_valid_cut(cut) {
            return Err(mismatch("message_count"));
        }
        // The text the compaction task scanned must be exactly the text of
        // the bytes about to be replayed.
        let (system_prompt, context_messages) =
            self.scan_view(cut).ok_or_else(|| mismatch("view"))?;
        if system_prompt != request.system_prompt || context_messages != request.context_messages {
            return Err(mismatch("view"));
        }
        Ok(cut)
    }
}

/// The route resolved now must still be the model and fallback position the
/// warm prefix was recorded on; anything else cannot reuse its KV cache.
pub(super) fn ensure_same_model(
    recording: &WarmPrefixRecording,
    selected: &ironclaw_llm::ModelFallbackRoute,
) -> Result<(), HostManagedModelError> {
    if recording.provider_model() == Some(selected.model.as_str())
        && recording.fallback_index() == selected.fallback_index
    {
        return Ok(());
    }
    debug!("cache fork route no longer resolves to the warm prefix model");
    Err(HostManagedModelError::safe(
        HostManagedModelErrorKind::Unavailable,
        "warm prefix was recorded on a different model route",
    ))
}

fn assistant_scan_text(message: &ChatMessage) -> String {
    let Some(tool_calls) = message
        .tool_calls
        .as_ref()
        .filter(|calls| !calls.is_empty())
    else {
        return message.content.clone();
    };
    let mut text = message.content.clone();
    for call in tool_calls {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[tool call ");
        text.push_str(&call.name);
        text.push_str("] ");
        text.push_str(&call.arguments.to_string());
    }
    text
}

/// Everything needed to record a live call once the provider accepts it.
pub(super) struct WarmPrefixCapture {
    cache: Arc<WarmPrefixCache>,
    thread_id: ThreadId,
    run_id: TurnRunId,
    model_profile_id: ModelProfileId,
    resolved_model_route: Option<HostManagedModelRouteSnapshot>,
    fallback_index: u32,
    boundaries: Vec<WarmPrefixBoundary>,
}

impl WarmPrefixCapture {
    /// `None` when the call cannot seed a fork: no durable thread, or a
    /// prompt that cannot be cut at host-message boundaries.
    pub(super) fn new(
        cache: &Arc<WarmPrefixCache>,
        thread_id: Option<&ThreadId>,
        run_id: TurnRunId,
        model_profile_id: &ModelProfileId,
        resolved_model_route: Option<&HostManagedModelRouteSnapshot>,
        fallback_index: u32,
        boundaries: Option<Vec<WarmPrefixBoundary>>,
    ) -> Option<Self> {
        Some(Self {
            cache: Arc::clone(cache),
            thread_id: thread_id?.clone(),
            run_id,
            model_profile_id: model_profile_id.clone(),
            resolved_model_route: resolved_model_route.cloned(),
            fallback_index,
            boundaries: boundaries?,
        })
    }

    pub(super) fn record_tool_request(&self, request: &ToolCompletionRequest) {
        let completion = CompletionRequest {
            messages: request.messages.clone(),
            model: request.model.clone(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop_sequences: request.stop_sequences.clone(),
            response_format: request.response_format.clone(),
            metadata: request.metadata.clone(),
        };
        self.record(completion, request.tools.clone());
    }

    pub(super) fn record_completion(&self, completion: CompletionRequest) {
        self.record(completion, Vec::new());
    }

    fn record(&self, completion: CompletionRequest, tools: Vec<ToolDefinition>) {
        let Ok(prefix_id) =
            SystemInferenceWarmPrefixId::new(format!("wp-{}", uuid::Uuid::new_v4()))
        else {
            // silent-ok: a uuid always satisfies the id grammar; a failure
            // only costs this call's warm prefix (compaction stays standalone).
            debug!("warm prefix id could not be minted");
            return;
        };
        self.cache.record(WarmPrefixRecording {
            prefix_id,
            thread_id: self.thread_id.clone(),
            run_id: self.run_id,
            model_profile_id: self.model_profile_id.clone(),
            resolved_model_route: self.resolved_model_route.clone(),
            fallback_index: self.fallback_index,
            completion,
            tools,
            boundaries: self.boundaries.clone(),
        });
    }
}

/// Remap pre-coalescing boundaries onto the final provider message list,
/// where the leading run of system messages is merged into one message.
pub(super) fn coalesced_boundaries(
    boundaries: Vec<WarmPrefixBoundary>,
    leading_system_messages: usize,
) -> Vec<WarmPrefixBoundary> {
    boundaries
        .into_iter()
        .map(|boundary| WarmPrefixBoundary {
            provider_message_count: boundary.provider_message_count.and_then(|count| {
                (count > leading_system_messages).then(|| {
                    count - leading_system_messages + usize::from(leading_system_messages > 0)
                })
            }),
            content_ref: boundary.content_ref,
        })
        .collect()
}

/// Replay `recording` through `cut` with `tail_instruction` appended as the
/// only new message. Tools stay advertised byte-for-byte with the live
/// `tool_choice` (changing it would invalidate provider message caches), but
/// no capability port is attached: any tool call in the output is rejected.
pub(super) async fn complete_cache_fork<P>(
    provider: &P,
    recording: &WarmPrefixRecording,
    cut: usize,
    tail_instruction: String,
    cache_scope: Option<PromptCacheCallScope>,
) -> Result<HostManagedModelResponse, HostManagedModelError>
where
    P: LlmProvider + ?Sized,
{
    let mut tail = CompletionRequest::new(vec![ChatMessage::user(tail_instruction)]);
    let redaction_count = redact_completion_request(&mut tail);
    if redaction_count > 0 {
        debug!(
            redaction_count,
            "cache fork redacted provider-bound tail instruction content"
        );
    }
    let tail_message = tail.messages.pop().ok_or_else(|| {
        HostManagedModelError::safe(
            HostManagedModelErrorKind::Unavailable,
            "cache fork tail instruction is missing",
        )
    })?;
    let mut completion = recording.completion.clone();
    completion.messages.truncate(cut);
    completion.messages.push(tail_message);
    let system_prompt_hash = system_prompt_cache_signature(&completion.messages);

    if recording.tools.is_empty() {
        let response = provider
            .complete(completion)
            .await
            .map_err(|error| map_provider_completion_error(error, None))?;
        if let Some(scope) = cache_scope.as_ref() {
            scope.record(
                ModelCallCacheUsage::from_completion_response(&response),
                tool_definitions_cache_signature(&[]),
                system_prompt_hash,
            );
        }
        log_fork_usage(response.cache_read_input_tokens, response.input_tokens);
        return response_to_host_reply(response);
    }

    let tool_names = recording
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let request =
        ToolCompletionRequest::from_completion_request(completion, recording.tools.clone());
    let response = provider
        .complete_with_tools(request)
        .await
        .map_err(|error| map_provider_completion_error(error, None))?;
    if let Some(scope) = cache_scope.as_ref() {
        scope.record(
            ModelCallCacheUsage::from_tool_response(&response),
            tool_definitions_cache_signature(&tool_names),
            system_prompt_hash,
        );
    }
    log_fork_usage(response.cache_read_input_tokens, response.input_tokens);
    tool_free_reply(response)
}

fn log_fork_usage(cache_read_input_tokens: u32, input_tokens: u32) {
    debug!(
        cache_read_input_tokens,
        input_tokens, "cache-forked inference completed on the warm prefix"
    );
}

pub(super) async fn llm_provider_cache_fork<P>(
    gateway: &super::LlmProviderModelGateway<P>,
    request: HostManagedCacheForkRequest,
) -> Result<HostManagedModelResponse, HostManagedModelError>
where
    P: LlmProvider + ?Sized + Send + Sync,
{
    let (recording, cut) = gateway.warm_prefix_cache.admit_fork(&request)?;
    let route = gateway
        .policy
        .route_for(&request.model_profile_id)
        .ok_or_else(|| {
            HostManagedModelError::safe(
                HostManagedModelErrorKind::PolicyDenied,
                "model profile is not permitted",
            )
        })?;
    let model_override = request_model_override(
        route,
        gateway.provider.as_ref(),
        request
            .resolved_model_route
            .as_ref()
            .map(|snapshot| snapshot.model_id()),
    )?;
    let selected = resolve_fallback_route(
        gateway.provider.as_ref(),
        request.fallback_index,
        &model_override,
    )?;
    ensure_same_model(&recording, &selected)?;
    let result = complete_cache_fork(
        gateway.provider.as_ref(),
        &recording,
        cut,
        request.tail_instruction,
        Some(gateway.prompt_cache_scope(request.run_id)),
    )
    .await;
    with_model_diagnostic_evidence(result, recording.fallback_index(), selected.model)
}

pub(super) async fn routed_cache_fork<P>(
    gateway: &super::RoutedLlmProviderModelGateway<P>,
    request: HostManagedCacheForkRequest,
) -> Result<HostManagedModelResponse, HostManagedModelError>
where
    P: super::ModelRouteProviderPool + ?Sized + Send + Sync,
{
    let (recording, cut) = gateway.warm_prefix_cache.admit_fork(&request)?;
    let slot = slot_for_model_profile(&request.model_profile_id)?;
    let request_snapshot = request
        .resolved_model_route
        .as_ref()
        .ok_or_else(missing_route_snapshot_error)?;
    let policy_mode = gateway.validate_route_snapshot(slot, request_snapshot)?;
    let snapshot = snapshot_from_host_request(slot, request_snapshot, policy_mode)?;
    let provider = gateway.provider_pool.provider_for_route(&snapshot).await?;
    validate_provider_model_binding_matches_route(snapshot.route(), provider.as_ref())?;
    let selected = resolve_fallback_route(
        provider.as_ref(),
        request.fallback_index,
        snapshot.route().model_id(),
    )?;
    ensure_same_model(&recording, &selected)?;
    let result = complete_cache_fork(
        provider.as_ref(),
        &recording,
        cut,
        request.tail_instruction,
        Some(gateway.prompt_cache_scope(request.run_id)),
    )
    .await;
    with_model_diagnostic_evidence(result, recording.fallback_index(), selected.model)
}

/// The fork exposes tools only to keep the cache key; it never executes one.
fn tool_free_reply(
    response: ToolCompletionResponse,
) -> Result<HostManagedModelResponse, HostManagedModelError> {
    let content = response.content.unwrap_or_default();
    if !response.tool_calls.is_empty()
        || response.finish_reason == FinishReason::ToolUse
        || contains_codex_text_tool_call_syntax(&content)
    {
        return Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidOutput,
            "cache-forked inference attempted a tool call",
        ));
    }
    response_to_host_reply(CompletionResponse {
        content,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
        finish_reason: response.finish_reason,
        reasoning: response.reasoning,
        cache_read_input_tokens: response.cache_read_input_tokens,
        cache_creation_input_tokens: response.cache_creation_input_tokens,
        usage_source: ironclaw_host_api::resource::ModelUsageSource::Provider,
    })
}

#[cfg(test)]
#[path = "warm_prefix/tests.rs"]
mod tests;
