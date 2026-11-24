//! Overlapped streaming read-only pre-execution (P2.1).
//!
//! While the model is still streaming, a host that opted in through
//! [`LoopModelPort::stream_model_observing_capability_calls`] announces each
//! tool call the provider finished streaming. [`StreamingReadOnlyPreExecutor`]
//! starts the provably read-only ones at once, so their I/O overlaps the rest
//! of generation instead of starting after it.
//!
//! Fail-closed on every axis:
//! - only a call [`is_read_only_call`] proves a pure read, on the model call's
//!   surface version and visible capability set, and that the host lets enter
//!   as a concurrent single invocation, is started — never a write, exec,
//!   network call, the `ironclaw.tool_call` bridge, or an unknown id;
//! - nothing starts on a turn that resumes a parked gate;
//! - launches stay within the run's remaining invocation allowance and the
//!   read-only fan-out width, and every launch races the run's cancellation;
//! - a stream error, cancellation, or a response that is not a fully
//!   read-only capability batch (a truncated `Length` stop never becomes one)
//!   aborts every in-flight read and discards every result;
//! - a result is reused only for a final call identical to the announced one,
//!   and only when it is a clean `Done`; anything else is re-dispatched by
//!   `CapabilityStage` through the normal path.
//!
//! Speculation never touches loop state or the transcript: results are held
//! in memory and reach `CapabilityStage` only through [`SpeculativeReads`],
//! which appends them exactly as it would a result it dispatched itself. What
//! the host records while serving an unconfirmed pure read (result staging,
//! activity milestones) is covered by the host's opt-in contract.
//!
//! [`LoopModelPort::stream_model_observing_capability_calls`]:
//!     ironclaw_loop_contracts::LoopModelPort::stream_model_observing_capability_calls

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures::{
    StreamExt,
    channel::mpsc,
    future::{AbortHandle, Abortable, Aborted, BoxFuture, abortable},
    stream::FuturesUnordered,
};
use ironclaw_host_api::{ids::CapabilityId, resolution::Resolution, turn::CapabilityActivityId};
use ironclaw_loop_contracts::{
    AgentLoopHostError, CapabilityCallCandidate, CapabilitySurfaceVersion, LoopModelRequest,
    LoopModelResponse, LoopStreamedCapabilityCallObserver, ParentLoopOutput,
};
use tracing::debug;

use super::capability_batch::resolution_stops_parallel_launch;
use super::pipeline::StageContext;
use super::read_only_batch::{BatchEffectClass, has_no_pending_gate_resume, is_read_only_call};
use crate::state::{InvocationCharge, LoopExecutionState};

/// Upper bound on reads pre-executed for one model call, before the run's
/// remaining invocation allowance narrows it further.
const MAX_SPECULATIVE_READS_PER_MODEL_CALL: usize = 32;

type SpeculativeInvocation = (CapabilityActivityId, Result<Resolution, AgentLoopHostError>);
type InFlightRead<'a> = Abortable<BoxFuture<'a, SpeculativeInvocation>>;

/// Forwards announced candidates into the model stage without blocking the
/// host's stream.
struct ChannelObserver(mpsc::UnboundedSender<CapabilityCallCandidate>);

impl LoopStreamedCapabilityCallObserver for ChannelObserver {
    fn capability_call_ready(&self, candidate: CapabilityCallCandidate) {
        if self.0.unbounded_send(candidate).is_err() {
            // The model stage stopped listening (the call finished or was
            // dropped); the final response still carries this call.
            debug!("streamed capability call arrived after the model stage stopped listening");
        }
    }
}

/// Clean results of reads pre-executed while the model streamed, held until
/// `CapabilityStage` confirms them against the batch it dispatches.
#[derive(Default)]
pub(super) struct SpeculativeReads {
    completed: HashMap<CapabilityActivityId, CompletedRead>,
}

struct CompletedRead {
    candidate: CapabilityCallCandidate,
    resolution: Resolution,
}

impl SpeculativeReads {
    pub(super) fn len(&self) -> usize {
        self.completed.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.completed.is_empty()
    }

    /// For each call of the batch about to dispatch, the pre-executed
    /// resolution to reuse instead of invoking it again. Only a `ReadOnly`
    /// batch harvests; any other batch discards every speculative result.
    pub(super) fn harvest(
        mut self,
        effect_class: BatchEffectClass,
        calls: &[CapabilityCallCandidate],
    ) -> Vec<Option<Resolution>> {
        if effect_class != BatchEffectClass::ReadOnly {
            if !self.completed.is_empty() {
                debug!(
                    discarded = self.completed.len(),
                    "discarding speculative reads for a side-effecting batch"
                );
            }
            return calls.iter().map(|_| None).collect();
        }
        calls
            .iter()
            .map(|call| {
                self.completed
                    .remove(&call.activity_id)
                    .filter(|read| same_call(&read.candidate, call))
                    .map(|read| read.resolution)
            })
            .collect()
    }
}

/// Whether two candidates name the same dispatch: every field the host
/// invocation and the read-only proof depend on. `provider_replay` is
/// provider metadata the final response may enrich, not dispatch identity.
fn same_call(announced: &CapabilityCallCandidate, final_call: &CapabilityCallCandidate) -> bool {
    announced.activity_id == final_call.activity_id
        && announced.surface_version == final_call.surface_version
        && announced.capability_id == final_call.capability_id
        && announced.input_ref == final_call.input_ref
        && announced.effective_capability_ids == final_call.effective_capability_ids
}

/// One model call's pre-execution: admits announced calls, runs the
/// eligible ones on a bounded, abortable task set, and keeps clean results.
/// Dropping it aborts every in-flight read.
pub(super) struct StreamingReadOnlyPreExecutor<'a> {
    ctx: StageContext<'a>,
    surface_version: CapabilitySurfaceVersion,
    visible: HashSet<CapabilityId>,
    launch_allowance: usize,
    max_in_flight: usize,
    seen: HashSet<CapabilityActivityId>,
    queued: VecDeque<CapabilityCallCandidate>,
    running: FuturesUnordered<InFlightRead<'a>>,
    in_flight: HashMap<CapabilityActivityId, (CapabilityCallCandidate, AbortHandle)>,
    completed: HashMap<CapabilityActivityId, CompletedRead>,
}

impl<'a> StreamingReadOnlyPreExecutor<'a> {
    /// `None` when this model call may not pre-execute anything: a gate
    /// resume is pending, the request names no surface, or the run has no
    /// invocation allowance left.
    fn for_model_call(
        ctx: StageContext<'a>,
        state: &LoopExecutionState,
        request: &LoopModelRequest,
    ) -> Option<Self> {
        if !has_no_pending_gate_resume(state) {
            return None;
        }
        let surface_version = request.surface_version.clone()?;
        let visible = request
            .capability_view
            .as_ref()?
            .visible_capability_ids
            .iter()
            .cloned()
            .collect();
        // Probe a copy: the real charge happens in `CapabilityStage`.
        let mut probe = state.budget_ledger;
        let launch_allowance = match probe.try_charge_invocations(
            MAX_SPECULATIVE_READS_PER_MODEL_CALL,
            &ctx.host
                .run_context()
                .resolved_run_profile
                .resource_budget_policy,
        ) {
            InvocationCharge::Charged => MAX_SPECULATIVE_READS_PER_MODEL_CALL,
            InvocationCharge::Partial { admitted } => admitted,
            InvocationCharge::Exhausted => return None,
        };
        Some(Self {
            ctx,
            surface_version,
            visible,
            launch_allowance,
            max_in_flight: BatchEffectClass::ReadOnly.max_parallel_invocations(),
            seen: HashSet::new(),
            queued: VecDeque::new(),
            running: FuturesUnordered::new(),
            in_flight: HashMap::new(),
            completed: HashMap::new(),
        })
    }

    fn admit(&mut self, candidate: CapabilityCallCandidate) {
        if !self.seen.insert(candidate.activity_id) {
            return;
        }
        let eligible = self.launch_allowance > 0
            && is_read_only_call(&candidate)
            && candidate.surface_version == self.surface_version
            && self.visible.contains(&candidate.capability_id)
            && !self.ctx.host.requires_ordered_batch_invocation(&[
                super::capability_invocation_from_candidate(candidate.clone(), None),
            ]);
        if !eligible {
            debug!(
                capability_id = candidate.capability_id.as_str(),
                "streamed capability call is not eligible for read-only pre-execution"
            );
            return;
        }
        self.launch_allowance -= 1;
        if self.running.len() < self.max_in_flight {
            self.launch(candidate);
        } else {
            self.queued.push_back(candidate);
        }
    }

    fn launch(&mut self, candidate: CapabilityCallCandidate) {
        let host = self.ctx.host;
        let activity_id = candidate.activity_id;
        let request = super::capability_invocation_from_candidate(candidate.clone(), None);
        let invoke: BoxFuture<'a, SpeculativeInvocation> =
            Box::pin(async move { (activity_id, host.invoke_capability(request).await) });
        let (invoke, abort) = abortable(invoke);
        debug!(
            capability_id = candidate.capability_id.as_str(),
            "pre-executing streamed read-only capability call"
        );
        self.in_flight.insert(activity_id, (candidate, abort));
        self.running.push(invoke);
    }

    fn record(&mut self, finished: Result<SpeculativeInvocation, Aborted>) {
        // silent-ok: an aborted read was discarded on purpose (unconfirmed
        // by the final response); nothing waits on it.
        let Ok((activity_id, result)) = finished else {
            return;
        };
        if let Some((candidate, _)) = self.in_flight.remove(&activity_id) {
            match result {
                Ok(resolution) if !resolution_stops_parallel_launch(&resolution) => {
                    self.completed.insert(
                        activity_id,
                        CompletedRead {
                            candidate,
                            resolution,
                        },
                    );
                }
                // A parked, cancelled, or failed read is never reused; the
                // capability stage re-dispatches it through the normal path.
                Ok(_) => debug!("speculative read parked or was cancelled; not reused"),
                Err(error) => debug!(
                    error_kind = ?error.kind,
                    "speculative read failed; not reused"
                ),
            }
        }
        if let Some(next) = self.queued.pop_front() {
            self.launch(next);
        }
    }

    /// The response arrived. Keep only reads the final batch confirms: stop
    /// queued launches, abort the unconfirmed in-flight ones, and wait for
    /// the confirmed ones. `None` when the run was cancelled meanwhile.
    async fn settle<C>(
        mut self,
        calls: &[CapabilityCallCandidate],
        cancellation: &mut C,
    ) -> Option<SpeculativeReads>
    where
        C: std::future::Future + Unpin,
    {
        self.queued.clear();
        let confirmed = |id: &CapabilityActivityId, announced: &CapabilityCallCandidate| {
            calls
                .iter()
                .any(|call| call.activity_id == *id && same_call(announced, call))
        };
        for (id, (announced, abort)) in &self.in_flight {
            if !confirmed(id, announced) {
                abort.abort();
            }
        }
        self.completed
            .retain(|id, read| confirmed(id, &read.candidate));
        while !self.running.is_empty() {
            tokio::select! {
                biased;
                finished = self.running.next() => {
                    if let Some(finished) = finished {
                        self.record(finished);
                    }
                }
                _signal = &mut *cancellation => return None,
            }
        }
        Some(SpeculativeReads {
            completed: self.completed,
        })
    }
}

/// One model call raced against Stop, pre-executing streamed read-only
/// calls when the host announces any. `None` means cancellation won; every
/// in-flight read is aborted by the drop.
pub(super) async fn stream_model_with_read_only_pre_execution(
    ctx: StageContext<'_>,
    state: &LoopExecutionState,
    request: LoopModelRequest,
) -> Option<(
    Result<LoopModelResponse, AgentLoopHostError>,
    SpeculativeReads,
)> {
    let Some(mut pre) = StreamingReadOnlyPreExecutor::for_model_call(ctx, state, &request) else {
        let model_call = ctx.host.stream_model(request);
        tokio::pin!(model_call);
        let cancellation = ctx.host.cancellation_requested();
        tokio::pin!(cancellation);
        return tokio::select! {
            biased;
            result = &mut model_call => Some((result, SpeculativeReads::default())),
            _signal = &mut cancellation => None,
        };
    };

    let (sender, mut announced) = mpsc::unbounded();
    let observer: Arc<dyn LoopStreamedCapabilityCallObserver> = Arc::new(ChannelObserver(sender));
    let model_call = ctx
        .host
        .stream_model_observing_capability_calls(request, observer);
    tokio::pin!(model_call);
    let mut cancellation = ctx.host.cancellation_requested();
    let mut listening = true;
    let result = loop {
        tokio::select! {
            biased;
            result = &mut model_call => break result,
            _signal = &mut cancellation => return None,
            candidate = announced.next(), if listening => match candidate {
                Some(candidate) => pre.admit(candidate),
                None => listening = false,
            },
            Some(finished) = pre.running.next(), if !pre.running.is_empty() => {
                pre.record(finished);
            }
        }
    };
    drop(announced);

    let reads = match &result {
        Ok(response) => match &response.output {
            // Only a clean tool-use batch made entirely of provable reads may
            // reuse anything; a truncated stop never reaches this shape.
            ParentLoopOutput::CapabilityCalls(calls)
                if has_no_pending_gate_resume(state) && calls.iter().all(is_read_only_call) =>
            {
                pre.settle(calls, &mut cancellation).await?
            }
            _ => SpeculativeReads::default(),
        },
        Err(_) => SpeculativeReads::default(),
    };
    if !reads.is_empty() {
        debug!(
            reused = reads.len(),
            "streamed read-only calls finished during the model call"
        );
    }
    Some((result, reads))
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::turn::LoopResultRef;
    use ironclaw_loop_contracts::{CapabilityInputRef, CapabilityProgress, resolution};

    use super::*;

    fn read_call(input: &str) -> CapabilityCallCandidate {
        let capability_id = CapabilityId::new("builtin.read_file").expect("valid");
        CapabilityCallCandidate {
            activity_id: CapabilityActivityId::new(),
            surface_version: CapabilitySurfaceVersion::new("surface-v1").expect("valid"),
            capability_id: capability_id.clone(),
            input_ref: CapabilityInputRef::new(input).expect("valid"),
            effective_capability_ids: vec![capability_id],
            provider_replay: None,
        }
    }

    fn done() -> Resolution {
        resolution::completed(
            LoopResultRef::new("result:speculative").expect("valid"),
            "summary".to_string(),
            CapabilityProgress::MadeProgress,
            false,
            0,
            None,
            None,
        )
    }

    fn reads_for(call: &CapabilityCallCandidate) -> SpeculativeReads {
        let mut reads = SpeculativeReads::default();
        reads.completed.insert(
            call.activity_id,
            CompletedRead {
                candidate: call.clone(),
                resolution: done(),
            },
        );
        reads
    }

    #[test]
    fn harvest_reuses_only_an_identical_call_of_a_read_only_batch() {
        let call = read_call("input:a");
        assert!(
            reads_for(&call).harvest(BatchEffectClass::ReadOnly, std::slice::from_ref(&call))[0]
                .is_some()
        );
        // A side-effecting batch discards everything (fail closed).
        assert!(
            reads_for(&call).harvest(BatchEffectClass::SideEffecting, std::slice::from_ref(&call))
                [0]
            .is_none()
        );
        // Same activity, different input: the announced call was not the one
        // the final response carries.
        let mut changed = call.clone();
        changed.input_ref = CapabilityInputRef::new("input:b").expect("valid");
        assert!(reads_for(&call).harvest(BatchEffectClass::ReadOnly, &[changed])[0].is_none());
        // Different effective targets are a different dispatch too.
        let mut widened = call.clone();
        widened
            .effective_capability_ids
            .push(CapabilityId::new("builtin.grep").expect("valid"));
        assert!(reads_for(&call).harvest(BatchEffectClass::ReadOnly, &[widened])[0].is_none());
    }
}
