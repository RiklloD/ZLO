/// Test double substituting the production `LoopCapabilityPort` produced by
/// `HostRuntimeLoopCapabilityPortFactory` (`crates/loop/ironclaw_loop_host/src/capability_port/factory.rs`).
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::resolution::{Resolution, ResolutionBatch};
use ironclaw_loop_contracts::{
    AgentLoopHostError, CapabilityCallCandidate, LoopCapabilityPort, LoopRequest, LoopRequestBatch,
    ProviderToolCall, ProviderToolCallCapabilityIds, ProviderToolDefinition,
    VisibleCapabilityRequest, VisibleCapabilitySurface,
};

pub(crate) struct RecordingDelegatingCapabilityPort {
    pub(crate) inner: Arc<dyn LoopCapabilityPort>,
    pub(crate) invocations: Arc<Mutex<Vec<LoopRequest>>>,
}

#[async_trait]
impl LoopCapabilityPort for RecordingDelegatingCapabilityPort {
    // Forwarded, not defaulted: the trait default is the fail-closed `true`,
    // which would make the harness serialize every batch (and refuse every
    // streamed read pre-execution) where the production port does not.
    fn requires_ordered_batch_invocation(&self, invocations: &[LoopRequest]) -> bool {
        self.inner.requires_ordered_batch_invocation(invocations)
    }

    fn tool_definitions(&self) -> Result<Vec<ProviderToolDefinition>, AgentLoopHostError> {
        self.inner.tool_definitions()
    }

    fn provider_tool_call_capability_ids(
        &self,
        tool_call: &ProviderToolCall,
    ) -> Result<ProviderToolCallCapabilityIds, AgentLoopHostError> {
        self.inner.provider_tool_call_capability_ids(tool_call)
    }

    fn validate_provider_tool_call(
        &self,
        tool_call: &ProviderToolCall,
    ) -> Result<(), AgentLoopHostError> {
        self.inner.validate_provider_tool_call(tool_call)
    }

    async fn register_provider_tool_call(
        &self,
        request: ironclaw_loop_contracts::RegisterProviderToolCallRequest,
    ) -> Result<CapabilityCallCandidate, AgentLoopHostError> {
        self.inner.register_provider_tool_call(request).await
    }

    async fn visible_capabilities(
        &self,
        request: VisibleCapabilityRequest,
    ) -> Result<VisibleCapabilitySurface, AgentLoopHostError> {
        self.inner.visible_capabilities(request).await
    }

    async fn invoke_capability(
        &self,
        request: LoopRequest,
    ) -> Result<Resolution, AgentLoopHostError> {
        self.invocations.lock().unwrap().push(request.clone());
        self.inner.invoke_capability(request).await
    }

    async fn invoke_capability_batch(
        &self,
        request: LoopRequestBatch,
    ) -> Result<ResolutionBatch, AgentLoopHostError> {
        self.invocations
            .lock()
            .unwrap()
            .extend(request.invocations.iter().cloned());
        self.inner.invoke_capability_batch(request).await
    }
}
