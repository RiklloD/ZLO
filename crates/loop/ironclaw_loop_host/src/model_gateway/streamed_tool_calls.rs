//! Mid-stream registration of pure-read provider tool calls (P2.1).
//!
//! When the loop opts in through
//! `LoopModelPort::stream_model_observing_capability_calls`, the provider
//! stream sink hands every tool call the provider finished streaming to a
//! [`StreamedToolCallBridge`]. The bridge registers the call exactly as the
//! final response will (`tool_response_to_host`) and announces the resulting
//! candidate, so the loop can start the read while the model keeps
//! generating.
//!
//! What makes an announced candidate identical to the final one:
//! - `provider_turn_id` is anchored on the response's **first** tool call, and
//!   the bridge caches the first call it saw, so call #1, #2, … all derive the
//!   id the final pass derives. A stale anchor (a retry or failover streamed
//!   different calls) yields a different `input_ref`, so nothing matches and
//!   nothing is reused — never a false match.
//! - registration is idempotent per `input_ref` (the input ref is a digest of
//!   run, provider, model, turn, call id, tool name, and normalized arguments;
//!   the registration store reuses the activity id per ref), so the final
//!   pass gets back the same activity identity.
//!
//! Fail-closed on side effects: only a call whose provider capability and
//! every effective capability is on [`STREAMABLE_READ_CAPABILITY_IDS`] is
//! registered at all, checked *before* registration, so a write, exec,
//! network, bridge, or discovery call is never touched mid-stream. What a
//! registration leaves behind for an unconfirmed read is in-memory and keyed
//! by `input_ref` (staged arguments, the registration record, the display
//! preview): no durable row, no event. Every failure is a debug log and a
//! skipped announcement — the final response stays authoritative.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ironclaw_host_api::ids::{CapabilityId, ProviderToolName};
use ironclaw_llm::ToolCall;
use ironclaw_loop_contracts::{
    CapabilityCallCandidate, LoopCapabilityPort, LoopStreamedCapabilityCallObserver,
    RegisterProviderToolCallRequest,
};
use tracing::debug;

use crate::HostManagedModelStreamSink;

use super::{
    ProviderReplayIdentity, provider_calls_are_advertised_or_resolvable,
    provider_tool_call_from_llm, provider_turn_id,
};

/// Capabilities the bridge may register while the model still streams: pure
/// reads of host state whose registration and invocation leave only
/// in-memory, per-`input_ref` staging behind.
///
/// A strict subset of the loop's read-only allowlist
/// (`ironclaw_agent_loop`'s `read_only_batch`): catalog discovery
/// (`ironclaw.tool_search` / `ironclaw.tool_describe`) is left out because
/// serving it mutates the turn's tool-disclosure state. Drift between the two
/// lists only ever narrows speculation.
const STREAMABLE_READ_CAPABILITY_IDS: &[&str] = &[
    "builtin.read_file",
    "builtin.list_dir",
    "builtin.glob",
    "builtin.grep",
    "builtin.result_read",
];

fn is_streamable_read(capability_id: &CapabilityId) -> bool {
    STREAMABLE_READ_CAPABILITY_IDS.contains(&capability_id.as_str())
}

fn is_streamable_read_call<'a>(
    capability_id: &CapabilityId,
    mut effective_capability_ids: impl Iterator<Item = &'a CapabilityId>,
) -> bool {
    is_streamable_read(capability_id) && effective_capability_ids.all(is_streamable_read)
}

/// The text consumer of a streaming call made only so its tool calls can be
/// observed. Declines text, which also keeps provider retry and failover free
/// to replace a failed attempt.
pub(super) struct StreamedCallsOnlySink;

#[async_trait]
impl HostManagedModelStreamSink for StreamedCallsOnlySink {
    fn accepts_safe_text_updates(&self) -> bool {
        false
    }

    async fn safe_text_update(&self, _safe_text: String) {}
}

/// Registers and announces the pure-read tool calls of one tool-capable
/// provider call. Built per call; never outlives it.
pub(super) struct StreamedToolCallBridge {
    capabilities: Arc<dyn LoopCapabilityPort>,
    observer: Arc<dyn LoopStreamedCapabilityCallObserver>,
    advertised_tool_names: HashSet<ProviderToolName>,
    provider_turn_scope: String,
    replay_identity: ProviderReplayIdentity,
    /// The first call the provider streamed; anchors `provider_turn_id` for
    /// every later one, mirroring the final pass.
    anchor: Mutex<Option<ToolCall>>,
}

impl StreamedToolCallBridge {
    pub(super) fn new(
        capabilities: Arc<dyn LoopCapabilityPort>,
        observer: Arc<dyn LoopStreamedCapabilityCallObserver>,
        advertised_tool_names: HashSet<ProviderToolName>,
        provider_turn_scope: String,
        replay_identity: ProviderReplayIdentity,
    ) -> Self {
        Self {
            capabilities,
            observer,
            advertised_tool_names,
            provider_turn_scope,
            replay_identity,
            anchor: Mutex::new(None),
        }
    }

    fn anchored_provider_turn_id(&self, call: &ToolCall) -> String {
        let mut anchor = match self.anchor.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let first = anchor.get_or_insert_with(|| call.clone());
        provider_turn_id(&self.provider_turn_scope, first)
    }

    /// One streamed call: announce it when it is a registrable pure read.
    pub(super) async fn tool_call_ready(&self, call: ToolCall) {
        // Anchor before any filtering: the final pass anchors on the first
        // call whatever it is.
        let turn_id = self.anchored_provider_turn_id(&call);
        if let Some(candidate) = self.register_pure_read(call, turn_id).await {
            self.observer.capability_call_ready(candidate);
        }
    }

    async fn register_pure_read(
        &self,
        call: ToolCall,
        provider_turn_id: String,
    ) -> Option<CapabilityCallCandidate> {
        // `response_reasoning` is unknown mid-stream; it is replay metadata,
        // not part of the input ref, so the final candidate still matches.
        let provider_call = match provider_tool_call_from_llm(
            call,
            None,
            provider_turn_id,
            &self.replay_identity,
        ) {
            Ok(provider_call) => provider_call,
            Err(error) => {
                debug!(
                    reason = error.safe_summary.as_str(),
                    "streamed provider tool call is not registrable"
                );
                return None;
            }
        };
        if !provider_calls_are_advertised_or_resolvable(
            &self.advertised_tool_names,
            self.capabilities.as_ref(),
            std::slice::from_ref(&provider_call),
        ) {
            return None;
        }
        // The pure-read proof comes before every registration side effect.
        match self
            .capabilities
            .provider_tool_call_capability_ids(&provider_call)
        {
            Ok(ids)
                if is_streamable_read_call(
                    &ids.provider_capability_id,
                    ids.effective_capability_ids.iter(),
                ) => {}
            Ok(_) => {
                debug!(
                    tool_name = provider_call.name.as_str(),
                    "streamed provider tool call is not a pure read; left for the final response"
                );
                return None;
            }
            Err(error) => {
                debug!(
                    tool_name = provider_call.name.as_str(),
                    reason = error.safe_summary.as_str(),
                    "streamed provider tool call did not resolve to a capability"
                );
                return None;
            }
        }
        if let Err(error) = self
            .capabilities
            .validate_provider_tool_call(&provider_call)
        {
            debug!(
                tool_name = provider_call.name.as_str(),
                reason = error.safe_summary.as_str(),
                "streamed provider tool call failed validation; left for the final response"
            );
            return None;
        }
        let candidate = match self
            .capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(provider_call))
            .await
        {
            Ok(candidate) => candidate,
            Err(error) => {
                debug!(
                    reason = error.safe_summary.as_str(),
                    "streamed provider tool call registration failed; left for the final response"
                );
                return None;
            }
        };
        // A decorator may resolve the registered call differently; the
        // announced candidate must still prove a pure read.
        is_streamable_read_call(
            &candidate.capability_id,
            candidate.effective_capability_ids.iter(),
        )
        .then_some(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> CapabilityId {
        CapabilityId::new(raw).expect("valid capability id")
    }

    #[test]
    fn only_pure_reads_resolving_to_pure_reads_are_streamable() {
        let read = id("builtin.read_file");
        assert!(is_streamable_read_call(&read, std::iter::empty()));
        assert!(is_streamable_read_call(&read, [id("builtin.grep")].iter()));
        // A writer, exec, or discovery call never registers mid-stream.
        for raw in [
            "builtin.write_file",
            "builtin.shell",
            "ironclaw.tool_search",
            "ironclaw.tool_describe",
            "ironclaw.tool_call",
        ] {
            assert!(
                !is_streamable_read_call(&id(raw), std::iter::empty()),
                "{raw}"
            );
        }
        // A read alias that fans out to a writer fails closed.
        assert!(!is_streamable_read_call(
            &read,
            [id("builtin.write_file")].iter()
        ));
    }
}
