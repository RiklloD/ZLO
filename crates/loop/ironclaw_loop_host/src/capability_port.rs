use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ironclaw_approvals::{ApprovalStoreError, GateRecordStorePort};
use ironclaw_capabilities::{ReplayPayload, ReplayPayloadStoreError, ReplayPayloadStorePort};
use ironclaw_host_api::{
    approval::sha256_digest_token,
    capability::{CapabilitySet, EffectKind},
    capability_surface::CapabilitySurfacePolicy,
    dispatch::{
        CapabilityDisplayOutputPreview, DispatchFailureDetail, DispatchInputIssue,
        DispatchInputIssueCode, RuntimeDispatchErrorKind, provider_diagnostic_model_cause,
    },
    gate_record::GateRecord,
    ids::{
        ApprovalRequestId, CapabilityId, CorrelationId, ExtensionId, GateRef, InvocationId,
        ProviderToolName,
    },
    invocation::InvocationOrigin,
    mount::MountView,
    resolution::{Resolution, ResolutionBatch},
    resource::{ResourceEstimate, ResourceScope},
    result_meta::{FailureKind, ModelDiagnostic},
    runtime::RuntimeKind,
    scope::{ExecutionContext, Principal},
};
use ironclaw_host_runtime::{
    CapabilityFailureDisposition, HostRuntime, HostRuntimeError, IdempotencyKey,
    RuntimeBlockedReason, RuntimeCapabilityFailure, RuntimeCapabilityOutcome,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityApprovalResume, CapabilityAuthResume,
    CapabilityDeniedReasonKind, CapabilityDescriptorView, CapabilityFailureDetail,
    CapabilityInputIssue, CapabilityInputRef, CapabilityResumeToken, ContentDigest,
    LoopCapabilityPort, LoopHostMilestone, LoopHostMilestoneKind, LoopHostMilestoneSink,
    LoopProcessRef, LoopRequest, LoopRequestBatch, LoopRunContext, LoopSafeSummary,
    ModelVisibleToolObservation, ProviderToolCall, ProviderToolCallCapabilityIds,
    ProviderToolCallReplay, ProviderToolDefinition, RegisterProviderToolCallRequest,
    VisibleCapabilityRequest, VisibleCapabilitySurface,
    resolution::{self, GatedResolution},
};
use ironclaw_turns::{CapabilityActivityId, LoopGateRef, LoopResultRef};
use serde_json::Value;
use tokio::sync::Notify;

mod dispatch_records;
mod factory;
mod failure_preview;
mod gate_persistence;
mod gate_stores;
mod invocation_context;
mod invoke_dispatch;
mod loop_port_impl;
mod outcome;
mod port;
mod provider_input;
mod provider_tool_names;
mod provider_validation;
mod result_write;
mod runtime_dispatch;
mod surface_snapshot;

#[cfg(test)]
mod tests;

use self::provider_input::{
    normalize_provider_arguments, prepare_provider_arguments,
    prepare_provider_arguments_with_detail, schema_contains_external_ref,
};
use self::provider_validation::{
    PROVIDER_TOOL_NAME_MAX_BYTES, validate_provider_arguments, validate_provider_tool_call,
};
use self::surface_snapshot::{
    RuntimeSurfaceCapabilitySnapshot, SurfaceCapabilitySnapshot, SurfaceSnapshot,
    SyntheticSurfaceCapabilitySnapshot,
};

use self::dispatch_records::*;
pub use self::factory::{
    DecoratingLoopCapabilityPortFactory, HostRuntimeLoopCapabilityPortFactory,
    LoopCapabilityPortDecorator, LoopCapabilityPortFactory,
};
use self::failure_preview::*;
use self::gate_stores::*;
pub use self::invocation_context::loop_driver_execution_extension_id;
use self::invocation_context::*;
use self::outcome::*;
pub use self::port::HostRuntimeLoopCapabilityPort;
use self::port::*;
use self::provider_tool_names::*;
pub use self::result_write::{CapabilityResultWrite, CapabilityWriteResult, DurablePersistence};
use self::runtime_dispatch::*;

const PROVIDER_TOOL_NAME_DIGEST_BYTES: usize = 32;
const PROVIDER_TOOL_CALL_INPUT_REF_PREFIX: &str = "input:provider-tool-";

/// Observes a capability invocation's resolved input (arguments) as the host
/// loop executes it, for trajectory capture by downstream consumers (benchmark
/// harnesses, debuggers, UI). `call_id` is the capability input ref.
///
/// The capability port emits input events. Result-writer implementations may
/// emit the matching result event through [`Self::on_capability_result`].
///
/// Best-effort and side-effect-free. The callback fires inline on the
/// per-capability hot path, so an implementation **must never block** (do I/O,
/// contend on a lock): hand the event to a non-blocking queue and return. A
/// callback that panics is caught at the call site and the event is dropped —
/// it cannot unwind or fail the run — but it must not rely on that.
pub trait CapabilityTrajectoryObserver: std::fmt::Debug + Send + Sync {
    /// A model tool call resolved to a capability invocation: `capability_id` is
    /// the resolved capability (e.g. `builtin.shell`), `arguments` the tool-call
    /// input JSON resolved from the input ref. This fires before schema
    /// normalization/coercion, so `arguments` is the raw model-emitted input
    /// (what the trajectory should record), not the post-validation execution
    /// payload.
    fn on_capability_input(
        &self,
        call_id: &str,
        capability_id: &str,
        arguments: &serde_json::Value,
    );

    /// A capability completed and staged `output` for the model. The default
    /// keeps existing input-only observers source-compatible.
    fn on_capability_result(
        &self,
        _call_id: &str,
        _capability_id: &str,
        _output: &serde_json::Value,
    ) {
    }
}

#[async_trait]
pub trait LoopCapabilityInputResolver: Send + Sync {
    async fn resolve_capability_input(
        &self,
        run_context: &LoopRunContext,
        input_ref: &CapabilityInputRef,
    ) -> Result<serde_json::Value, AgentLoopHostError>;

    async fn register_provider_tool_call_input(
        &self,
        _run_context: &LoopRunContext,
        _tool_call: &ProviderToolCall,
    ) -> Result<CapabilityInputRef, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "provider tool-call input registration is not supported",
        ))
    }

    /// Record the display-preview input for a provider tool call under
    /// `input_ref`, keyed for display by the resolved dotted `capability_id`
    /// (e.g. `nearai.web_search`) — NOT the provider tool name
    /// (`nearai__web_search`), which is a lossy, digest-suffixed encoding that
    /// both renders badly and defeats the per-tool summary/subtitle matchers.
    ///
    /// `ProviderToolCallInputResolver` decorates this trait and owns the
    /// canonical (digest-based) `input_ref`; it stages the arguments itself and
    /// does NOT delegate `register_provider_tool_call_input` to the inner
    /// resolver, so it forwards this hook to `inner` instead. The caller
    /// (`register_provider_tool_call`) drives it after registration because that
    /// is where the resolved `capability_id` and the canonical `input_ref` are
    /// both in hand. Default no-op: only resolvers that own a display-preview
    /// store implement it.
    fn record_provider_tool_call_display_input(
        &self,
        _run_context: &LoopRunContext,
        _input_ref: &CapabilityInputRef,
        _capability_id: &CapabilityId,
        _tool_call: &ProviderToolCall,
    ) {
    }
}

struct ProviderToolCallInputResolver {
    inner: Arc<dyn LoopCapabilityInputResolver>,
    provider_inputs: Mutex<HashMap<String, serde_json::Value>>,
}

impl ProviderToolCallInputResolver {
    fn new(inner: Arc<dyn LoopCapabilityInputResolver>) -> Self {
        Self {
            inner,
            provider_inputs: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl LoopCapabilityInputResolver for ProviderToolCallInputResolver {
    async fn resolve_capability_input(
        &self,
        run_context: &LoopRunContext,
        input_ref: &CapabilityInputRef,
    ) -> Result<serde_json::Value, AgentLoopHostError> {
        if let Some(input) = self
            .provider_inputs
            .lock()
            .map_err(|_| {
                AgentLoopHostError::new(
                    AgentLoopHostErrorKind::Unavailable,
                    "provider tool-call input store is unavailable",
                )
            })?
            .get(input_ref.as_str())
            .cloned()
        {
            return Ok(input);
        }
        self.inner
            .resolve_capability_input(run_context, input_ref)
            .await
    }

    async fn register_provider_tool_call_input(
        &self,
        run_context: &LoopRunContext,
        tool_call: &ProviderToolCall,
    ) -> Result<CapabilityInputRef, AgentLoopHostError> {
        let input_ref = provider_tool_call_input_ref(run_context, tool_call)?;
        let mut provider_inputs = self.provider_inputs.lock().map_err(|_| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                "provider tool-call input store is unavailable",
            )
        })?;
        if let Some(existing) = provider_inputs.get(input_ref.as_str()) {
            if existing != &tool_call.arguments {
                return Err(AgentLoopHostError::new(
                    AgentLoopHostErrorKind::Internal,
                    "provider tool-call input ref collision",
                ));
            }
        } else {
            provider_inputs.insert(input_ref.as_str().to_string(), tool_call.arguments.clone());
        }
        Ok(input_ref)
    }

    fn record_provider_tool_call_display_input(
        &self,
        run_context: &LoopRunContext,
        input_ref: &CapabilityInputRef,
        capability_id: &CapabilityId,
        tool_call: &ProviderToolCall,
    ) {
        // This decorator bypasses the inner `register_provider_tool_call_input`,
        // so forward the display-recording side effect to `inner` (the resolver
        // that owns the display-preview store).
        self.inner.record_provider_tool_call_display_input(
            run_context,
            input_ref,
            capability_id,
            tool_call,
        );
    }
}

#[async_trait]
pub trait LoopCapabilityResultWriter: Send + Sync {
    /// Write the result of a completed capability invocation.
    ///
    /// Returns metadata for the staged output: the result ref, serialized byte
    /// length for per-capability byte accounting, and an optional normalized
    /// content digest for future output-aware progress detection.
    async fn write_capability_result(
        &self,
        write: CapabilityResultWrite<'_>,
    ) -> Result<CapabilityWriteResult, AgentLoopHostError>;

    async fn update_capability_result(
        &self,
        _run_context: &LoopRunContext,
        _result_ref: &LoopResultRef,
        _output: serde_json::Value,
    ) -> Result<u64, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "capability result updates are not supported by this writer",
        ))
    }

    async fn delete_capability_result(
        &self,
        _run_context: &LoopRunContext,
        _result_ref: &LoopResultRef,
    ) -> Result<(), AgentLoopHostError> {
        Ok(())
    }

    /// Note that the invocation `invocation_id` has started executing with the
    /// input staged under `input_ref`. Links the two so the still-running
    /// activity frame can surface the input (inline argument + parameters)
    /// before the result lands — the input was recorded under `input_ref` at
    /// registration, but the activity projection only knows the `invocation_id`.
    /// Default no-op: only writers that own a display-preview store implement it.
    fn record_running_invocation(
        &self,
        _run_context: &LoopRunContext,
        _invocation_id: InvocationId,
        _input_ref: &CapabilityInputRef,
    ) {
    }

    /// Stage a display preview for a FAILED capability invocation so the UI can
    /// render the specific failure detail (e.g. invalid-input field issues)
    /// instead of only the bare error kind. `summary` is a bounded,
    /// host-authored string (see `capability_failure_display_summary`).
    /// Default no-op: only writers that own a display-preview store implement
    /// it. Async so implementers can durably persist the failure preview the
    /// same way `write_capability_result` persists success previews.
    async fn stage_capability_failure_preview(
        &self,
        _run_context: &LoopRunContext,
        _invocation_id: InvocationId,
        _capability_id: &CapabilityId,
        _summary: &str,
    ) {
    }
}
