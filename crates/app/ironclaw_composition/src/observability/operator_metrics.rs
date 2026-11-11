//! [`TurnEventSink`] over the operator metrics registry.
//!
//! The adapter lives here, not beside either side: `ironclaw_operator` owns
//! the registry but may not name `ironclaw_turns` (its `BoundaryRule`), and
//! the turn kernel must not name a products crate. Composition is the one
//! layer that sees both — the same placement as `LiveSkillLearnedNotifier`.
//! It holds no policy: it maps a committed lifecycle transition onto the
//! registry's neutral `record_*` calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::Timestamp;
use ironclaw_operator::{OperatorMetrics, TurnRunOutcome};
use ironclaw_turns::{
    TurnBlockedGateKind, TurnError, TurnEventKind, TurnEventSink, TurnLifecycleEvent, TurnRunId,
};

/// Runs whose start is remembered for the duration histogram. Beyond it a run
/// still counts, without a duration sample — never an unbounded map.
const MAX_TRACKED_RUN_STARTS: usize = 10_000;

pub(crate) struct OperatorMetricsTurnEventSink {
    metrics: Arc<OperatorMetrics>,
    started_at: Mutex<HashMap<TurnRunId, Timestamp>>,
}

impl OperatorMetricsTurnEventSink {
    pub(crate) fn new(metrics: Arc<OperatorMetrics>) -> Self {
        Self {
            metrics,
            started_at: Mutex::new(HashMap::new()),
        }
    }

    fn started(&self) -> std::sync::MutexGuard<'_, HashMap<TurnRunId, Timestamp>> {
        match self.started_at.lock() {
            Ok(started) => started,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[async_trait]
impl TurnEventSink for OperatorMetricsTurnEventSink {
    async fn publish(&self, event: TurnLifecycleEvent) -> Result<(), TurnError> {
        let outcome = match event.kind {
            TurnEventKind::Completed => Some(TurnRunOutcome::Completed),
            TurnEventKind::Failed => Some(TurnRunOutcome::Failed),
            TurnEventKind::Cancelled => Some(TurnRunOutcome::Cancelled),
            _ => None,
        };
        match outcome {
            Some(outcome) => {
                let started = self.started().remove(&event.run_id);
                let duration = match (started, event.occurred_at) {
                    (Some(started), Some(ended)) => (ended - started).to_std().ok(),
                    _ => None,
                };
                self.metrics.record_turn_run(outcome, duration);
            }
            None => {
                if let Some(occurred_at) = event.occurred_at {
                    let mut started = self.started();
                    if started.len() < MAX_TRACKED_RUN_STARTS || started.contains_key(&event.run_id)
                    {
                        started.entry(event.run_id).or_insert(occurred_at);
                    }
                }
                if event.kind == TurnEventKind::Blocked
                    && event
                        .blocked_gate
                        .as_ref()
                        .is_some_and(|gate| gate.gate_kind == TurnBlockedGateKind::Approval)
                {
                    self.metrics.record_approval_gate_opened();
                }
            }
        }
        Ok(())
    }
}
