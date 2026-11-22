//! Read-only capability batch classification.
//!
//! `CapabilityStage` writes a `BeforeSideEffect` checkpoint before dispatching
//! a batch. That checkpoint is the durable evidence that an external effect
//! may have been attempted: the scheduler's lease-expiry recovery fails a run
//! whose newest checkpoint is `BeforeSideEffect` instead of requeueing it,
//! and `CheckpointStage::write_before_model_batched` refuses to batch the next
//! `BeforeModel` after one. For a batch of pure reads none of that protects
//! anything — re-running a read is idempotent and leaves no external trace —
//! yet it cost two synchronous durable writes per exploration turn.
//!
//! This module decides, fail-closed, when a batch is provably read-only. The
//! loop sees only capability identities (the visible surface carries no
//! effect declarations), so the proof is a reviewed allowlist of host
//! capabilities whose contract is a pure read of host state. Anything not on
//! it — extensions, MCP tools, writes, exec, network, the `ironclaw.tool_call`
//! dispatch bridge, an unknown id — keeps the full checkpoint discipline.

use ironclaw_host_api::capability::{TOOL_DESCRIBE_CAPABILITY_ID, TOOL_SEARCH_CAPABILITY_ID};
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_loop_contracts::CapabilityCallCandidate;

use crate::state::LoopExecutionState;

/// Host capabilities whose contract is a pure, idempotent read with no
/// external side effect. Re-running one after a crash cannot duplicate an
/// effect, so a batch made only of these needs no `BeforeSideEffect` marker.
///
/// Every entry must stay read-only by contract; widening this list is a
/// durability decision (see the module docs), not a convenience. Builtin
/// filesystem reads are also on the reviewed ungated loop-run allowlist in
/// `ironclaw_host_api::capability::UNGATED_LOOP_RUN_CAPABILITIES`.
const READ_ONLY_CAPABILITY_IDS: &[&str] = &[
    "builtin.read_file",
    "builtin.list_dir",
    "builtin.glob",
    "builtin.grep",
    // Bounded views of an already-stored durable result.
    "builtin.result_read",
    // Catalog discovery and schema lookup; neither dispatches anything.
    TOOL_SEARCH_CAPABILITY_ID,
    TOOL_DESCRIBE_CAPABILITY_ID,
];

/// Concurrent single invocations for a batch that may have side effects.
const MAX_PARALLEL_CAPABILITY_INVOCATIONS: usize = 4;

/// Concurrent single invocations for a provably read-only batch. Wide read
/// fan-out (grep/glob/read_file across a tree) is the common exploration
/// shape, and reads neither contend for a shared effect nor need ordering.
const MAX_PARALLEL_READ_ONLY_CAPABILITY_INVOCATIONS: usize = 12;

/// The effect class of one capability batch, decided once before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchEffectClass {
    /// Every call is a pure read: no `BeforeSideEffect` checkpoint, wider
    /// parallel fan-out.
    ReadOnly,
    /// Anything else, including anything the loop cannot prove read-only.
    SideEffecting,
}

impl BatchEffectClass {
    /// Classify the batch about to be dispatched; see [`is_read_only_batch`].
    pub(super) fn classify(state: &LoopExecutionState, calls: &[CapabilityCallCandidate]) -> Self {
        if is_read_only_batch(state, calls) {
            Self::ReadOnly
        } else {
            Self::SideEffecting
        }
    }

    /// Whether the batch needs a durable `BeforeSideEffect` checkpoint before
    /// dispatch.
    pub(super) fn requires_before_side_effect_checkpoint(self) -> bool {
        matches!(self, Self::SideEffecting)
    }

    /// Upper bound on concurrently launched single invocations.
    pub(super) fn max_parallel_invocations(self) -> usize {
        match self {
            Self::ReadOnly => MAX_PARALLEL_READ_ONLY_CAPABILITY_INVOCATIONS,
            Self::SideEffecting => MAX_PARALLEL_CAPABILITY_INVOCATIONS,
        }
    }
}

fn is_read_only_capability(capability_id: &CapabilityId) -> bool {
    READ_ONLY_CAPABILITY_IDS.contains(&capability_id.as_str())
}

/// Whether one model-emitted call is provably read-only: the invoked
/// capability AND every capability it resolves to must be on the read-only
/// allowlist, so a bridge or alias that fans out to a writer fails closed.
fn is_read_only_call(call: &CapabilityCallCandidate) -> bool {
    is_read_only_capability(&call.capability_id)
        && call
            .effective_capability_ids
            .iter()
            .all(is_read_only_capability)
}

/// Whether the batch about to be dispatched is provably read-only.
///
/// Fail-closed on every axis:
/// - an empty batch is not "read-only" (nothing proves anything);
/// - any pending approval, auth, or external-tool resume means this batch
///   resolves a durable gate record, which is itself a host-side mutation;
/// - every call must be [`is_read_only_call`].
fn is_read_only_batch(state: &LoopExecutionState, calls: &[CapabilityCallCandidate]) -> bool {
    !calls.is_empty()
        && state.pending_approval_resume.is_none()
        && state.pending_auth_resume.is_none()
        && state.pending_external_tool_resume.is_none()
        && calls.iter().all(is_read_only_call)
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::turn::CapabilityActivityId;
    use ironclaw_loop_contracts::{CapabilityInputRef, CapabilitySurfaceVersion};

    use super::*;

    fn call(capability_id: &str, effective: &[&str]) -> CapabilityCallCandidate {
        CapabilityCallCandidate {
            activity_id: CapabilityActivityId::new(),
            surface_version: CapabilitySurfaceVersion::new("surface-v1").expect("valid"),
            capability_id: CapabilityId::new(capability_id).expect("valid"),
            input_ref: CapabilityInputRef::new("input:read-only").expect("valid"),
            effective_capability_ids: effective
                .iter()
                .map(|id| CapabilityId::new(*id).expect("valid"))
                .collect(),
            provider_replay: None,
        }
    }

    fn state() -> LoopExecutionState {
        LoopExecutionState::initial_for_run(&crate::test_support::test_run_context(
            "read-only-batch",
        ))
    }

    #[test]
    fn pure_read_batch_is_read_only() {
        let calls = vec![
            call("builtin.read_file", &["builtin.read_file"]),
            call("builtin.glob", &["builtin.glob"]),
            call("builtin.grep", &[]),
            call("builtin.list_dir", &["builtin.list_dir"]),
            call("builtin.result_read", &["builtin.result_read"]),
            call(TOOL_SEARCH_CAPABILITY_ID, &[TOOL_SEARCH_CAPABILITY_ID]),
            call(TOOL_DESCRIBE_CAPABILITY_ID, &[TOOL_DESCRIBE_CAPABILITY_ID]),
        ];
        assert!(is_read_only_batch(&state(), &calls));
    }

    #[test]
    fn any_side_effecting_or_unknown_call_keeps_the_batch_side_effecting() {
        for other in [
            "builtin.write_file",
            "builtin.shell",
            "builtin.http",
            "builtin.spawn_subagent",
            "custom.unknown_tool",
            "ironclaw.tool_call",
        ] {
            let calls = vec![
                call("builtin.read_file", &["builtin.read_file"]),
                call(other, &[other]),
            ];
            assert!(
                !is_read_only_batch(&state(), &calls),
                "{other} must keep the BeforeSideEffect checkpoint"
            );
        }
    }

    #[test]
    fn a_read_only_alias_resolving_to_a_writer_is_not_read_only() {
        let calls = vec![call("builtin.read_file", &["builtin.write_file"])];
        assert!(!is_read_only_batch(&state(), &calls));
        // The dispatch bridge targeting a reader is still the bridge.
        let calls = vec![call("ironclaw.tool_call", &["builtin.read_file"])];
        assert!(!is_read_only_batch(&state(), &calls));
    }

    /// A batch that resumes a parked gate resolves a durable gate record, so
    /// it is never treated as read-only even when every call is a read.
    #[test]
    fn a_pending_gate_resume_keeps_a_read_batch_side_effecting() {
        let read = call("builtin.read_file", &["builtin.read_file"]);
        let mut resuming = state();
        resuming.pending_external_tool_resume = Some(crate::state::PendingExternalToolResume {
            gate_ref: ironclaw_host_api::turn::LoopGateRef::new("gate:read-only-resume")
                .expect("valid"),
            capability_id: read.capability_id.clone(),
            activity_id: read.activity_id,
            surface_version: read.surface_version.clone(),
            input_ref: read.input_ref.clone(),
            effective_capability_ids: read.effective_capability_ids.clone(),
            provider_replay: None,
            disposition: None,
        });
        assert!(!is_read_only_batch(&resuming, std::slice::from_ref(&read)));
        assert!(is_read_only_batch(&state(), &[read]));
    }

    #[test]
    fn an_empty_batch_is_not_read_only() {
        assert!(!is_read_only_batch(&state(), &[]));
        assert_eq!(
            BatchEffectClass::classify(&state(), &[]),
            BatchEffectClass::SideEffecting
        );
    }

    #[test]
    fn effect_class_drives_checkpoint_and_fan_out() {
        let read_only = BatchEffectClass::classify(
            &state(),
            &[call("builtin.read_file", &["builtin.read_file"])],
        );
        assert_eq!(read_only, BatchEffectClass::ReadOnly);
        assert!(!read_only.requires_before_side_effect_checkpoint());
        assert_eq!(read_only.max_parallel_invocations(), 12);

        let side_effecting = BatchEffectClass::classify(
            &state(),
            &[call("builtin.write_file", &["builtin.write_file"])],
        );
        assert_eq!(side_effecting, BatchEffectClass::SideEffecting);
        assert!(side_effecting.requires_before_side_effect_checkpoint());
        assert_eq!(side_effecting.max_parallel_invocations(), 4);
    }
}
