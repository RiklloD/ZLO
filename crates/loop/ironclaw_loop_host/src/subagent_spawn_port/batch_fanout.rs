//! Concurrent fan-out for a run of consecutive `spawn_subagent` calls inside
//! one capability batch.
//!
//! The agent loop routes every batch that contains a spawn through
//! [`LoopCapabilityPort::invoke_capability_batch`], because
//! `requires_ordered_batch_invocation` vetoes its generic per-call parallel
//! path. That veto stays: per-call parallel invocation cannot coalesce
//! blocking spawns onto one shared batch gate, and it cannot unwind siblings
//! when one of them fails. The concurrency lives here instead.

use std::collections::HashMap;

use futures::StreamExt;
use ironclaw_host_api::turn::TurnGateRef;
use ironclaw_host_api::{
    resolution::{Resolution, Suspension},
    result_meta::LoopRef,
};
use ironclaw_loop_contracts::{AgentLoopHostError, AgentLoopHostErrorKind, LoopRequest};

#[cfg(doc)]
use ironclaw_loop_contracts::LoopCapabilityPort;

use super::{
    PreparedSpawn, SpawnCompensationState, SpawnSubagentArgs, SubagentSpawnCapabilityPort,
};

/// One invocation of a consecutive run of spawns inside a batch, after the
/// serial admission pass: either already resolved without side effects, or
/// prepared and waiting for the concurrent child-creation pass.
pub(super) enum SpawnRunEntry<'a> {
    Resolved(Resolution),
    Prepared {
        invocation: &'a LoopRequest,
        prepared: PreparedSpawn<'a>,
        args: SpawnSubagentArgs,
        provider_call_id: String,
    },
}

/// Per-batch spawn inputs computed by the pre-decode pass.
pub(super) struct SpawnBatchPlan {
    pub(super) spawn_args: HashMap<usize, SpawnSubagentArgs>,
    pub(super) spawn_input_denials: HashMap<usize, Resolution>,
    /// Shared gate for every spawn when the batch has two or more decoded
    /// spawns.
    pub(super) batch_gate: Option<TurnGateRef>,
    pub(super) stop_on_first_suspension: bool,
}

/// Upper bound on child spawns whose creation work runs at once within one
/// batch. Matches the agent loop's parallel capability window; the per-turn
/// spawn cap (`max_spawn_per_turn`) still bounds the total.
pub(super) const MAX_CONCURRENT_BATCH_SPAWNS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpawnRunOutcome {
    Continue,
    Stopped,
}

impl SubagentSpawnCapabilityPort {
    /// Runs one maximal run of consecutive `spawn_subagent` invocations.
    ///
    /// Three passes keep the sequential batch's observable contract while
    /// creating children concurrently:
    ///
    /// 1. **Serial admission**, in invocation order: decode denials,
    ///    authorization, slot reservation, and read-only prechecks. Slots
    ///    therefore go to the earliest invocations, exactly as before, and no
    ///    slot can be over-granted (the reservation itself is atomic).
    /// 2. **Concurrent child creation** for every admitted spawn, bounded by
    ///    [`MAX_CONCURRENT_BATCH_SPAWNS`]. Every future runs to completion and
    ///    is never dropped mid-flight, so each one leaves either a committed
    ///    spawn with a recorded compensation, or its own rolled-back partial
    ///    effects.
    /// 3. **In-order merge.** The first failure, by invocation order, rolls
    ///    back every committed spawn of the batch in reverse invocation order
    ///    and is returned. A stop-on-suspension boundary rolls back the
    ///    committed spawns after it, which a sequential batch would never have
    ///    run.
    ///
    /// When the batch has two or more decoded spawns they all share the
    /// batch gate, and no spawn resolution can hit the stop boundary. That
    /// is why running them concurrently is safe. With fewer, at most one
    /// child is created and the passes degrade to the old sequential path.
    pub(super) async fn invoke_spawn_run(
        &self,
        run: &[LoopRequest],
        first_index: usize,
        plan: &mut SpawnBatchPlan,
        resolutions: &mut Vec<Resolution>,
        batch_compensations: &mut Vec<SpawnCompensationState>,
    ) -> Result<SpawnRunOutcome, AgentLoopHostError> {
        let mut entries = Vec::with_capacity(run.len());
        for (offset, invocation) in run.iter().enumerate() {
            let index = first_index + offset;
            if let Some(denial) = plan.spawn_input_denials.remove(&index) {
                entries.push(SpawnRunEntry::Resolved(denial));
                continue;
            }
            let admitted = match self.authorize_spawn(invocation).await {
                Ok(Ok(provider_call_id)) => match plan.spawn_args.remove(&index) {
                    Some(args) => self
                        .prepare_spawn(&args, plan.batch_gate.clone())
                        .await
                        .map(|prepared| (prepared, args, provider_call_id)),
                    None => Err(AgentLoopHostError::new(
                        AgentLoopHostErrorKind::Invalid,
                        "subagent spawn args missing from pre-decode pass",
                    )),
                },
                Ok(Err(outcome)) => {
                    entries.push(SpawnRunEntry::Resolved(outcome));
                    continue;
                }
                Err(error) => Err(error),
            };
            match admitted {
                Ok((Ok(prepared), args, provider_call_id)) => {
                    entries.push(SpawnRunEntry::Prepared {
                        invocation,
                        prepared,
                        args,
                        provider_call_id,
                    });
                }
                Ok((Err(rejection), _, _)) => entries.push(SpawnRunEntry::Resolved(rejection)),
                Err(error) => {
                    // Nothing in this run has side effects yet; dropping the
                    // entries releases their reserved slots.
                    drop(entries);
                    self.rollback_batch_compensation(batch_compensations).await;
                    return Err(error);
                }
            }
        }

        // Collect the futures first so no closure is held across the await
        // (keeps the `async_trait` future provably `Send`).
        let pending = entries
            .into_iter()
            .map(|entry| self.finish_spawn_run_entry(entry))
            .collect::<Vec<_>>();
        let finished = futures::stream::iter(pending)
            .buffered(MAX_CONCURRENT_BATCH_SPAWNS)
            .collect::<Vec<_>>()
            .await;

        let mut first_error = None;
        let mut stopped = false;
        // Committed spawns past the error/stop boundary, in invocation order.
        let mut beyond_boundary = Vec::new();
        for (result, compensation) in finished {
            if first_error.is_some() || stopped {
                if result.is_ok()
                    && let Some(compensation) = compensation
                {
                    beyond_boundary.push(compensation);
                }
                continue;
            }
            match result {
                // A failed spawn already rolled back its own partial effects.
                Err(error) => first_error = Some(error),
                Ok(resolution) => {
                    if let Some(compensation) = compensation {
                        batch_compensations.push(compensation);
                    }
                    let parks = resolution.parks();
                    let coalesced = is_batch_await_dependent(&resolution, plan.batch_gate.as_ref());
                    resolutions.push(resolution);
                    // `parks()`, not `is_suspension()` (H1): the batch stops on
                    // any parking resolution (gate or suspension), except a
                    // coalesced batch-await-dependent which accrues siblings.
                    stopped = parks && plan.stop_on_first_suspension && !coalesced;
                }
            }
        }

        if let Some(error) = first_error {
            batch_compensations.append(&mut beyond_boundary);
            self.rollback_batch_compensation(batch_compensations).await;
            return Err(error);
        }
        self.rollback_batch_compensation(&mut beyond_boundary).await;
        Ok(if stopped {
            SpawnRunOutcome::Stopped
        } else {
            SpawnRunOutcome::Continue
        })
    }

    async fn finish_spawn_run_entry(
        &self,
        entry: SpawnRunEntry<'_>,
    ) -> (
        Result<Resolution, AgentLoopHostError>,
        Option<SpawnCompensationState>,
    ) {
        match entry {
            SpawnRunEntry::Resolved(resolution) => (Ok(resolution), None),
            SpawnRunEntry::Prepared {
                invocation,
                prepared,
                args,
                provider_call_id,
            } => {
                let mut compensation = SpawnCompensationState::default();
                let result = self
                    .finish_prepared_spawn(
                        prepared,
                        invocation,
                        args,
                        provider_call_id,
                        &mut compensation,
                    )
                    .await;
                (result, Some(compensation))
            }
        }
    }
}

/// A blocking spawn coalesced onto the batch gate: the
/// `Suspended(DependentRun)` whose preserved loop-gate origin is the shared
/// batch gate. It parks, but the batch keeps accruing siblings onto it.
fn is_batch_await_dependent(resolution: &Resolution, batch_gate: Option<&TurnGateRef>) -> bool {
    matches!(
        resolution,
        Resolution::Suspended(Suspension::DependentRun { waypoint, .. })
            if batch_gate.is_some_and(|batch_gate| {
                waypoint.origin.as_ref().map(LoopRef::as_str) == Some(batch_gate.as_str())
            })
    )
}
