//! Model request/response DTOs, the host-managed prompt bundle contracts and
//! authority, and the [`LoopModelPort`]/[`LoopPromptPort`] traits.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_host_api::resource::ModelUsageSource;
use serde::{Deserialize, Serialize};

use crate::instruction_bundle::InstructionBundleFingerprint;
use crate::refs::ModelProfileId;
use crate::skill_context::SkillName;
use ironclaw_host_api::turn::{CapabilityActivityId, LoopMessageRef, TurnRunId};

use super::capability::ProviderToolCallReplay;
use super::context::{LoopContextCompactionMetadata, LoopInputCursor};
use super::error::{AgentLoopHostError, AgentLoopHostErrorKind};
use super::refs::{
    CapabilityInputRef, CapabilitySurfaceVersion, LoopCheckpointStateRef, LoopInlineMessageBody,
    LoopPromptBundleRef,
};
use super::run_context::LoopRunContext;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopModelCapabilityView {
    /// Final capability IDs visible to this model call after the loop driver has
    /// applied its strategy to the host-owned capability surface.
    pub visible_capability_ids: Vec<CapabilityId>,
}

/// Provider tool-choice constraint attached by the loop's model strategy for
/// ONE model call. Absent means the provider default ("auto"). The host
/// translates the constraint into the provider's forcing vocabulary;
/// providers without named-tool forcing degrade to their "some tool
/// required" mode rather than dropping the constraint silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopModelToolChoice {
    /// The model must invoke exactly this capability on this call. The
    /// capability must already be visible in the request's capability view.
    ForcedCapability { capability_id: CapabilityId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopModelRequest {
    pub messages: Vec<LoopModelMessage>,
    #[serde(default)]
    pub inline_messages: Vec<LoopInlineMessage>,
    pub surface_version: Option<CapabilitySurfaceVersion>,
    pub model_preference: Option<ModelProfileId>,
    /// Zero-based index into the host-resolved ordered fallback chain.
    #[serde(default)]
    pub fallback_index: u32,
    /// Zero-based agent-loop iteration that issued this provider call.
    #[serde(default)]
    pub iteration: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_view: Option<LoopModelCapabilityView>,
    /// Strategy-imposed provider tool-choice constraint for this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<LoopModelToolChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopModelMessage {
    pub role: String,
    pub content_ref: LoopMessageRef,
}

/// Prompt construction mode requested by an agent-loop driver.
///
/// `TextOnly` builds a prompt from transcript/context message refs and is the
/// only mode supported by `ironclaw_turns`'s `HostManagedLoopPromptPort`
/// today. `CodeAct` is reserved for a future checkpoint/tool-aware prompt
/// bundle flow and is rejected by the text-only host port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    TextOnly,
    #[serde(rename = "codeact")]
    CodeAct,
}

impl PromptMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TextOnly => "text_only",
            Self::CodeAct => "codeact",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopInlineMessageRole {
    /// Host instruction that must remain in the provider-cached leading block.
    LeadingSystem,
    /// Host-supplied task input that must precede persisted thread messages.
    LeadingUser,
    System,
    User,
    Assistant,
}

impl LoopInlineMessageRole {
    pub fn is_leading(self) -> bool {
        matches!(self, Self::LeadingSystem | Self::LeadingUser)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopInlineMessage {
    pub role: LoopInlineMessageRole,
    pub safe_body: LoopInlineMessageBody,
}

/// Request for a host-managed prompt bundle.
///
/// The optional cursor and checkpoint refs are run-scoped and are validated by
/// host ports before context is loaded. `max_messages` is a host budget hint;
/// zero is accepted only for inline-only context-free prompts, and oversized
/// values may be clamped by the implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopPromptBundleRequest {
    pub mode: PromptMode,
    pub context_cursor: Option<LoopInputCursor>,
    pub surface_version: Option<CapabilitySurfaceVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_view: Option<LoopModelCapabilityView>,
    pub checkpoint_state_ref: Option<LoopCheckpointStateRef>,
    pub max_messages: Option<u32>,
    #[serde(default)]
    pub inline_messages: Vec<LoopInlineMessage>,
}

/// Prompt bundle returned to a driver.
///
/// The bundle carries model-message references rather than raw prompt text.
/// Drivers pass these refs to [`LoopModelPort`], allowing the host to resolve
/// content under the same run scope and policy checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopPromptBundle {
    pub bundle_ref: LoopPromptBundleRef,
    pub messages: Vec<LoopModelMessage>,
    pub surface_version: Option<CapabilitySurfaceVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_message_index: Vec<LoopContextCompactionMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_window_truncation: Option<crate::host::context::LoopContextWindowTruncation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_fingerprint: Option<InstructionBundleFingerprint>,
    #[serde(default)]
    pub identity_message_count: u32,
    #[serde(default)]
    pub instruction_snippet_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPromptBundleGrant {
    pub bundle_ref: LoopPromptBundleRef,
    pub messages: Vec<LoopModelMessage>,
    pub surface_version: Option<CapabilitySurfaceVersion>,
    pub instruction_fingerprint: Option<InstructionBundleFingerprint>,
    pub diagnostic_metadata: LoopPromptDiagnosticMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopPromptDiagnosticMetadata {
    pub identity_message_count: u32,
    pub instruction_snippet_count: u32,
    pub active_skills: Vec<SkillName>,
}

#[derive(Clone, Default)]
pub struct LoopPromptBundleAuthority {
    inner: Arc<Mutex<LoopPromptBundleAuthorityState>>,
}

#[derive(Default)]
struct LoopPromptBundleAuthorityState {
    latest_by_run: HashMap<TurnRunId, LoopPromptBundleGrant>,
}

impl LoopPromptBundleAuthority {
    pub fn shared() -> Self {
        static AUTHORITY: OnceLock<LoopPromptBundleAuthority> = OnceLock::new();
        AUTHORITY.get_or_init(Self::default).clone()
    }

    pub fn issue_bundle(
        &self,
        context: &LoopRunContext,
        bundle: &LoopPromptBundle,
    ) -> Result<(), AgentLoopHostError> {
        self.issue_bundle_with_diagnostic_metadata(context, bundle, None)
    }

    pub fn issue_bundle_with_diagnostic_metadata(
        &self,
        context: &LoopRunContext,
        bundle: &LoopPromptBundle,
        diagnostic_metadata: Option<LoopPromptDiagnosticMetadata>,
    ) -> Result<(), AgentLoopHostError> {
        if !bundle.bundle_ref.is_for_run(context) {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::ScopeMismatch,
                "prompt bundle ref is not scoped to this loop run",
            ));
        }

        let mut state = self.lock_state()?;
        let diagnostic_metadata = diagnostic_metadata
            .or_else(|| {
                state
                    .latest_by_run
                    .get(&context.run_id)
                    .filter(|grant| grant.bundle_ref == bundle.bundle_ref)
                    .map(|grant| grant.diagnostic_metadata.clone())
            })
            .unwrap_or_default();
        state.latest_by_run.insert(
            context.run_id,
            LoopPromptBundleGrant {
                bundle_ref: bundle.bundle_ref.clone(),
                messages: bundle.messages.clone(),
                surface_version: bundle.surface_version.clone(),
                instruction_fingerprint: bundle.instruction_fingerprint.clone(),
                diagnostic_metadata,
            },
        );
        Ok(())
    }

    pub fn authorize_latest_model_request(
        &self,
        context: &LoopRunContext,
        messages: &[LoopModelMessage],
        surface_version: &Option<CapabilitySurfaceVersion>,
    ) -> Result<LoopPromptBundleGrant, AgentLoopHostError> {
        let grant = self
            .lock_state()?
            .latest_by_run
            .remove(&context.run_id)
            .ok_or_else(|| {
                AgentLoopHostError::new(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "model request has no host-built prompt bundle",
                )
            })?;

        if !grant.bundle_ref.is_for_run(context) {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::ScopeMismatch,
                "prompt bundle ref is not scoped to this loop run",
            ));
        }
        if grant.messages != messages {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "model request messages do not match the host-built prompt bundle",
            ));
        }
        if &grant.surface_version != surface_version {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::StaleSurface,
                "model request surface version does not match the host-built prompt bundle",
            ));
        }

        Ok(grant)
    }

    /// Evict any prompt-bundle grant issued for `run_id`.
    ///
    /// [`Self::authorize_latest_model_request`] already removes a grant on the
    /// success path, but a run that aborts or errors *between* issuing a bundle
    /// and authorizing its model request would otherwise leave its grant in this
    /// process-lifetime map forever. Terminal/abort handling calls this so the
    /// map stays scoped to live runs. Idempotent: evicting a run with no pending
    /// grant is a no-op.
    pub fn evict_run(&self, run_id: TurnRunId) -> Result<(), AgentLoopHostError> {
        self.lock_state()?.latest_by_run.remove(&run_id);
        Ok(())
    }

    fn lock_state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, LoopPromptBundleAuthorityState>, AgentLoopHostError> {
        self.inner.lock().map_err(|_| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                "prompt bundle authority is unavailable",
            )
        })
    }
}

/// Host boundary for building prompt bundles before model invocation.
///
/// Implementations own context loading, scoping, prompt-shape policy, and
/// milestone emission. Drivers should not assemble raw prompt strings when a
/// prompt port is available.
#[async_trait]
pub trait LoopPromptPort: Send + Sync {
    async fn build_prompt_bundle(
        &self,
        request: LoopPromptBundleRequest,
    ) -> Result<LoopPromptBundle, AgentLoopHostError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopModelResponse {
    pub chunks: Vec<ModelStreamChunk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safe_reasoning_deltas: Vec<String>,
    pub output: ParentLoopOutput,
    pub effective_model_profile_id: ModelProfileId,
    /// Provider-reported token usage for this call. `None` when the gateway
    /// could not surface real numbers (replay test stubs, providers without
    /// a usage object); downstream budget accounting falls back to the
    /// reservation estimate in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LoopModelUsage>,
}

/// Token usage reported by a provider for a single model call. The accountant
/// uses this to record actual USD spend instead of the conservative
/// reservation estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopModelUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens read from the provider's server-side prompt cache (e.g. Anthropic
    /// cache reads). A subset of `input_tokens`, billed at a discount. Zero when
    /// caching is unsupported or on a cache miss.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cache_read_input_tokens: u32,
    /// Tokens written to the provider's server-side prompt cache. Zero when
    /// caching is unsupported or no new prefix was cached.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cache_creation_input_tokens: u32,
    /// Whether a provider billed this usage. A response-cache replay keeps the
    /// replayed counts (they still size the context) but reports
    /// [`ModelUsageSource::ResponseCache`], so spend accounting charges
    /// nothing for it. Omitted on the wire for the default `provider`, so
    /// usage recorded before this field existed reads back as billed.
    ///
    /// Per-call provenance: on a cumulative total built with
    /// [`Self::add_assign`] it is `provider` as soon as any merged call was.
    #[serde(default, skip_serializing_if = "ModelUsageSource::is_provider")]
    pub usage_source: ModelUsageSource,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

impl LoopModelUsage {
    /// Add an optional supplemental call to an optional cumulative snapshot.
    pub fn merge_optional(cumulative: Option<Self>, supplemental: Option<Self>) -> Option<Self> {
        let Some(supplemental) = supplemental else {
            return cumulative;
        };
        let Some(mut cumulative) = cumulative else {
            return Some(supplemental);
        };
        cumulative.add_assign(&supplemental);
        Some(cumulative)
    }

    /// Accumulate another call's usage into this running per-run total.
    pub fn add_assign(&mut self, other: &LoopModelUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        if other.usage_source.is_provider() {
            self.usage_source = ModelUsageSource::Provider;
        }
    }

    /// Total billable tokens (input + output). Cache tokens are already counted
    /// within `input_tokens` by every provider that reports them, so they are
    /// not added again here.
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStreamChunk {
    pub safe_text_delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentLoopOutput {
    AssistantReply(AssistantReply),
    CapabilityCalls(Vec<CapabilityCallCandidate>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantReply {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityCallCandidate {
    /// Stable activity identity assigned before capability dispatch. Hosts use
    /// this as the runtime invocation identity, and tokenless gate checkpoints
    /// persist it so terminal events can close the same activity.
    pub activity_id: CapabilityActivityId,
    pub surface_version: CapabilitySurfaceVersion,
    pub capability_id: CapabilityId,
    pub input_ref: CapabilityInputRef,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective_capability_ids: Vec<CapabilityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_replay: Option<ProviderToolCallReplay>,
}

/// Receives capability-call candidates a host validated and registered while
/// the model response was still streaming.
///
/// A candidate is speculative: the response carrying it may still fail, be
/// cancelled, truncate, or be replaced by a retry. The loop may pre-execute
/// a provably read-only candidate, but acts on its result only after the
/// final [`ParentLoopOutput::CapabilityCalls`] carries an identical
/// candidate (same activity, surface version, capability ids, input ref).
/// Synchronous and non-blocking so announcing never stalls the stream.
pub trait LoopStreamedCapabilityCallObserver: Send + Sync {
    fn capability_call_ready(&self, candidate: CapabilityCallCandidate);
}

#[async_trait]
pub trait LoopModelPort: Send + Sync {
    async fn stream_model(
        &self,
        request: LoopModelRequest,
    ) -> Result<LoopModelResponse, AgentLoopHostError>;

    /// [`Self::stream_model`], announcing each tool call the provider
    /// finishes streaming to `observer` before the response completes.
    ///
    /// The default ignores the observer, so a port that does not override
    /// this never enables speculative pre-execution. Overriding it is the
    /// host's opt-in, and asserts that (1) an announced candidate is exactly
    /// the one the final response would carry for that call (registration is
    /// idempotent per input ref), and (2) invoking a pure-read capability on
    /// it before the response is accepted leaves no external effect and no
    /// durable record beyond what an unconfirmed read may leave. Decorators
    /// must forward this method to keep the opt-in; one that does not
    /// degrades to no speculation, never to a different result.
    async fn stream_model_observing_capability_calls(
        &self,
        request: LoopModelRequest,
        observer: Arc<dyn LoopStreamedCapabilityCallObserver>,
    ) -> Result<LoopModelResponse, AgentLoopHostError> {
        drop(observer);
        self.stream_model(request).await
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, ThreadId};

    use super::*;
    use crate::snapshot::ResolvedRunProfile;
    use ironclaw_host_api::turn::{RunProfileId, RunProfileVersion, TurnId, TurnScope};

    #[test]
    fn usage_source_is_optional_on_the_wire_and_per_call() {
        // Usage recorded before the field existed reads back as billed.
        let legacy: LoopModelUsage =
            serde_json::from_value(serde_json::json!({"input_tokens": 4, "output_tokens": 2}))
                .unwrap();
        assert_eq!(legacy.usage_source, ModelUsageSource::Provider);
        assert!(
            serde_json::to_value(legacy)
                .unwrap()
                .get("usage_source")
                .is_none(),
            "the billed default stays off the wire"
        );
        let replay = LoopModelUsage {
            input_tokens: 4,
            output_tokens: 2,
            usage_source: ModelUsageSource::ResponseCache,
            ..LoopModelUsage::default()
        };
        assert_eq!(
            serde_json::to_value(replay).unwrap()["usage_source"],
            "response_cache"
        );
        // A run total of only replays is a replay; one billed call makes it billed.
        let only_replays = LoopModelUsage::merge_optional(None, Some(replay)).unwrap();
        assert_eq!(only_replays.usage_source, ModelUsageSource::ResponseCache);
        assert_eq!(only_replays.input_tokens, 4);
        let mixed = LoopModelUsage::merge_optional(Some(replay), Some(legacy)).unwrap();
        assert_eq!(mixed.usage_source, ModelUsageSource::Provider);
        assert_eq!(mixed.total_tokens(), 12);
    }

    fn test_context() -> LoopRunContext {
        let scope = TurnScope::new(
            TenantId::new("tenant-prompt-authority").unwrap(),
            Some(AgentId::new("agent-prompt-authority").unwrap()),
            Some(ProjectId::new("project-prompt-authority").unwrap()),
            ThreadId::new("thread-prompt-authority").unwrap(),
        );
        let resolved_run_profile = ResolvedRunProfile::legacy_compatibility(
            RunProfileId::interactive_default(),
            RunProfileVersion::new(1),
            true,
        );
        LoopRunContext::new(scope, TurnId::new(), TurnRunId::new(), resolved_run_profile)
    }

    fn bundle_for(context: &LoopRunContext) -> LoopPromptBundle {
        LoopPromptBundle {
            bundle_ref: LoopPromptBundleRef::for_run(context, "token").expect("valid bundle ref"),
            messages: Vec::new(),
            surface_version: None,
            compaction_message_index: Vec::new(),
            recent_window_truncation: None,
            instruction_fingerprint: None,
            identity_message_count: 0,
            instruction_snippet_count: 0,
        }
    }

    #[test]
    fn evict_run_drops_a_grant_never_authorized() {
        // Regression: a run that issues a bundle but aborts before authorizing
        // its model request must not leak its grant in the process-lifetime map.
        let authority = LoopPromptBundleAuthority::default();
        let context = test_context();
        authority
            .issue_bundle(&context, &bundle_for(&context))
            .expect("issue bundle");

        authority.evict_run(context.run_id).expect("evict run");
        // Idempotent: evicting an already-absent run is a no-op.
        authority
            .evict_run(context.run_id)
            .expect("evict run again");

        // With the grant evicted, authorization fails closed exactly as if no
        // bundle had ever been issued for the run.
        let error = authority
            .authorize_latest_model_request(&context, &[], &None)
            .expect_err("evicted grant must not authorize a model request");
        assert_eq!(error.kind, AgentLoopHostErrorKind::InvalidInvocation);
    }

    struct ReplyPort;

    #[async_trait]
    impl LoopModelPort for ReplyPort {
        async fn stream_model(
            &self,
            _request: LoopModelRequest,
        ) -> Result<LoopModelResponse, AgentLoopHostError> {
            Ok(LoopModelResponse {
                chunks: Vec::new(),
                safe_reasoning_deltas: Vec::new(),
                output: ParentLoopOutput::AssistantReply(AssistantReply {
                    content: "done".to_string(),
                }),
                effective_model_profile_id: ModelProfileId::new("model").expect("profile"),
                usage: None,
            })
        }
    }

    #[derive(Default)]
    struct CountingObserver(std::sync::atomic::AtomicUsize);

    impl LoopStreamedCapabilityCallObserver for CountingObserver {
        fn capability_call_ready(&self, _candidate: CapabilityCallCandidate) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A port that does not opt in streams exactly as before and never
    /// announces a call, so existing ports enable no speculation.
    #[tokio::test]
    async fn observing_stream_defaults_to_plain_stream_without_announcing() {
        let observer = Arc::new(CountingObserver::default());
        let request = LoopModelRequest {
            messages: Vec::new(),
            inline_messages: Vec::new(),
            surface_version: None,
            model_preference: None,
            fallback_index: 0,
            iteration: 0,
            capability_view: None,
            tool_choice: None,
        };
        let response = ReplyPort
            .stream_model_observing_capability_calls(request, observer.clone())
            .await
            .expect("default delegates to stream_model");
        assert!(matches!(
            response.output,
            ParentLoopOutput::AssistantReply(ref reply) if reply.content == "done"
        ));
        assert_eq!(observer.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
