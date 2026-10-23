//! Decorators over the two local-memory seams the loop already uses.
//!
//! - [`SupermemoryPromptContext`] wraps the `MemoryPromptContextService` that
//!   feeds `memory_snippets`: Supermemory recall for callers it is on for, the
//!   wrapped local service otherwise.
//! - [`SupermemoryInteractionRecorder`] wraps the after-turn `MemoryService`
//!   writer: Supermemory capture for callers it is on for, the wrapped local
//!   writer otherwise. Every other `MemoryService` call delegates.
//!
//! With no key set both are pass-throughs, so local memory behaves as before.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_loop_contracts::{
    AgentLoopHostError, LoopContextSnippet, MemoryPromptContextLoad, MemoryPromptContextRequest,
    MemoryPromptContextService, MemoryRetrievalDegradation, MemoryRetrievalFailureKind,
    MemoryRetrievalLane, memory_snippet_display_ref,
};
use ironclaw_memory::{
    MemoryInteractionRole, MemoryInvocation, MemoryService, MemoryServiceContextRequest,
    MemoryServiceContextSnippet, MemoryServiceError, MemoryServiceProfileReadResponse,
    MemoryServiceRecordRequest, MemoryServiceRecordResponse, memory_context_disabled,
};
use ironclaw_prompt_envelope::{EnvelopeSource, EnvelopeTrust, wrap_untrusted_with_limit};
use ironclaw_safety::redact_model_input_text;

use crate::binding::SupermemoryCaller;
use crate::service::Recall;
use crate::service::Supermemory;

/// Byte cap per recalled snippet, envelope included.
const MAX_RECALL_SNIPPET_BYTES: usize = 1024;

pub struct SupermemoryPromptContext {
    supermemory: Arc<Supermemory>,
    local: Option<Arc<dyn MemoryPromptContextService>>,
}

impl SupermemoryPromptContext {
    pub fn new(
        supermemory: Arc<Supermemory>,
        local: Option<Arc<dyn MemoryPromptContextService>>,
    ) -> Self {
        Self { supermemory, local }
    }

    /// The prompt-context service to wire: always present, because a key can
    /// be set after startup.
    pub fn wrap(
        supermemory: &Arc<Supermemory>,
        local: Option<Arc<dyn MemoryPromptContextService>>,
    ) -> Option<Arc<dyn MemoryPromptContextService>> {
        Some(Arc::new(Self::new(Arc::clone(supermemory), local)))
    }

    async fn local(
        &self,
        request: MemoryPromptContextRequest,
    ) -> Result<MemoryPromptContextLoad, AgentLoopHostError> {
        match &self.local {
            Some(local) => local.load_memory_snippets(request).await,
            None => Ok(MemoryPromptContextLoad::default()),
        }
    }
}

#[async_trait]
impl MemoryPromptContextService for SupermemoryPromptContext {
    async fn load_memory_snippets(
        &self,
        request: MemoryPromptContextRequest,
    ) -> Result<MemoryPromptContextLoad, AgentLoopHostError> {
        if memory_context_disabled(request.context_profile_id.as_str()) {
            return self.local(request).await;
        }
        let caller =
            SupermemoryCaller::from_turn_scope(&request.scope, request.actor.user_id.clone());
        match self.supermemory.recall(&caller, &request.query).await {
            Ok(Some(recall)) => Ok(recall_load(&recall, request.max_snippets)),
            Ok(None) => self.local(request).await,
            Err(error) => {
                tracing::debug!(
                    kind = error.kind(),
                    "supermemory recall unavailable; using local memory"
                );
                self.local(request).await
            }
        }
    }
}

/// Guidance first, then the recalled lines packed into at most
/// `max_snippets - 1` untrusted-enveloped snippets of [`MAX_RECALL_SNIPPET_BYTES`].
fn recall_load(recall: &Recall, max_snippets: usize) -> MemoryPromptContextLoad {
    let mut snippets = Vec::new();
    if let Ok(guidance) = LoopContextSnippet::from_untrusted_memory(
        memory_snippet_display_ref(["supermemory", "guidance", recall.primary.as_str()]),
        recall.guidance.clone(),
    ) {
        snippets.push(guidance);
    }
    let now = Utc::now();
    let lines: Vec<String> = recall
        .items
        .iter()
        .filter_map(|item| clean_line(&item.render(&recall.primary, now)))
        .collect();
    let budget = payload_budget();
    let mut body = String::new();
    let mut bodies = Vec::new();
    for line in lines {
        let line = truncate(&line, budget);
        if !body.is_empty() && body.len() + 1 + line.len() > budget {
            bodies.push(std::mem::take(&mut body));
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    if !body.is_empty() {
        bodies.push(body);
    }
    for (index, body) in bodies
        .into_iter()
        .take(max_snippets.saturating_sub(snippets.len()))
        .enumerate()
    {
        let Ok(envelope) = wrap_untrusted_with_limit(
            EnvelopeSource::Memory,
            EnvelopeTrust::Untrusted,
            &body,
            MAX_RECALL_SNIPPET_BYTES,
        ) else {
            continue;
        };
        let index = index.to_string();
        let reference =
            memory_snippet_display_ref(["supermemory", recall.primary.as_str(), &index, &body]);
        if let Ok(snippet) =
            LoopContextSnippet::from_untrusted_memory(reference, envelope.into_string())
        {
            snippets.push(snippet);
        }
    }
    let degradations = if recall.degraded {
        vec![MemoryRetrievalDegradation::new(
            MemoryRetrievalLane::LongTerm,
            MemoryRetrievalFailureKind::Unavailable,
        )]
    } else {
        Vec::new()
    };
    MemoryPromptContextLoad {
        snippets,
        degradations,
    }
}

/// Control characters stripped, credentials redacted; `None` when empty.
fn clean_line(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|ch| !ch.is_control()).collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| redact_model_input_text(cleaned).into_text())
}

/// Payload bytes left inside one envelope.
fn payload_budget() -> usize {
    const PROBE: &str = "x";
    wrap_untrusted_with_limit(
        EnvelopeSource::Memory,
        EnvelopeTrust::Untrusted,
        PROBE,
        MAX_RECALL_SNIPPET_BYTES,
    )
    .map(|probe| MAX_RECALL_SNIPPET_BYTES.saturating_sub(probe.byte_len() - PROBE.len()))
    .unwrap_or(MAX_RECALL_SNIPPET_BYTES / 2) // silent-ok: pure computation over a constant probe
}

fn truncate(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub struct SupermemoryInteractionRecorder {
    supermemory: Arc<Supermemory>,
    local: Option<Arc<dyn MemoryService>>,
}

impl SupermemoryInteractionRecorder {
    pub fn new(supermemory: Arc<Supermemory>, local: Option<Arc<dyn MemoryService>>) -> Self {
        Self { supermemory, local }
    }

    /// The after-turn writer to wire: always present, because a key can be
    /// set after startup.
    pub fn wrap(
        supermemory: &Arc<Supermemory>,
        local: Option<Arc<dyn MemoryService>>,
    ) -> Option<Arc<dyn MemoryService>> {
        Some(Arc::new(Self::new(Arc::clone(supermemory), local)))
    }
}

#[async_trait]
impl MemoryService for SupermemoryInteractionRecorder {
    async fn profile_read(
        &self,
        invocation: MemoryInvocation,
    ) -> Result<MemoryServiceProfileReadResponse, MemoryServiceError> {
        match &self.local {
            Some(local) => local.profile_read(invocation).await,
            None => Err(MemoryServiceError::unavailable()),
        }
    }

    async fn read_long_term(
        &self,
        invocation: MemoryInvocation,
        request: MemoryServiceContextRequest,
    ) -> Result<Vec<MemoryServiceContextSnippet>, MemoryServiceError> {
        match &self.local {
            Some(local) => local.read_long_term(invocation, request).await,
            None => Err(MemoryServiceError::unavailable()),
        }
    }

    async fn read_short_term(
        &self,
        invocation: MemoryInvocation,
        request: MemoryServiceContextRequest,
    ) -> Result<Vec<MemoryServiceContextSnippet>, MemoryServiceError> {
        match &self.local {
            Some(local) => local.read_short_term(invocation, request).await,
            None => Err(MemoryServiceError::unavailable()),
        }
    }

    async fn record_interaction(
        &self,
        invocation: MemoryInvocation,
        request: MemoryServiceRecordRequest,
    ) -> Result<MemoryServiceRecordResponse, MemoryServiceError> {
        let caller = SupermemoryCaller::from_resource_scope(&invocation.scope);
        if !self.supermemory.is_active(&caller).await {
            return match &self.local {
                Some(local) => local.record_interaction(invocation, request).await,
                None => Ok(MemoryServiceRecordResponse { recorded: false }),
            };
        }
        let joined = |role: MemoryInteractionRole| {
            request
                .messages
                .iter()
                .filter(|message| message.role == role)
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        let thread_key = invocation
            .scope
            .thread_id
            .as_ref()
            .map(|thread| thread.as_str().to_string())
            .unwrap_or_else(|| invocation.scope.user_id.as_str().to_string());
        let recorded = self
            .supermemory
            .capture(
                &caller,
                &thread_key,
                &joined(MemoryInteractionRole::User),
                &joined(MemoryInteractionRole::Assistant),
            )
            .await
            .map_err(MemoryServiceError::unavailable_from)?;
        Ok(MemoryServiceRecordResponse { recorded })
    }
}
