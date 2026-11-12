//! Operator-metrics wiring: the [`TurnEventSink`] adapter over the registry
//! and the one place each metrics decorator is applied.
//!
//! The adapter lives here, not beside either side: `ironclaw_operator` owns
//! the registry but may not name `ironclaw_turns` (its `BoundaryRule`), and
//! the turn kernel must not name a products crate. Composition is the one
//! layer that sees both — the same placement as `LiveSkillLearnedNotifier`.
//! It holds no policy: it maps a committed lifecycle transition onto the
//! registry's neutral `record_*` calls.
//!
//! The `metered_*` helpers are the only decorator sites: production assembly
//! calls them with the process-wide registry, and the integration harness
//! calls the same helpers (through `test_support`) with its own registry, so
//! the harness cannot drift from production's decorator positions.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_event_log::{DurableEventLog, NonBlockingEventSink};
use ironclaw_event_store::{CoalescingEventSink, EventBatchConfig};
use ironclaw_host_api::Timestamp;
use ironclaw_operator::{OperatorMetrics, TurnRunOutcome};
use ironclaw_turns::{
    TurnBlockedGateKind, TurnError, TurnEventKind, TurnEventSink, TurnLifecycleEvent, TurnRunId,
};

/// The runtime's write-behind durable event sink, with operator metrics
/// folded from the rows its drain task durably commits (not from what
/// producers hand it — the bounded queue drops under overload).
pub(crate) fn metered_runtime_event_sink(
    log: Arc<dyn DurableEventLog>,
    config: EventBatchConfig,
    metrics: Arc<OperatorMetrics>,
) -> Arc<dyn NonBlockingEventSink> {
    Arc::new(CoalescingEventSink::new(
        Arc::new(ironclaw_operator::MetricsDurableEventLog::new(log, metrics)),
        config,
    ))
}

/// The gateway's provider chain with the metrics provider outermost.
pub(crate) fn metered_model_provider(
    provider: Arc<dyn ironclaw_llm::LlmProvider>,
    metrics: Arc<OperatorMetrics>,
) -> Arc<dyn ironclaw_llm::LlmProvider> {
    Arc::new(ironclaw_operator::MetricsLlmProvider::new(
        provider, metrics,
    ))
}

/// The approval resolver's store, counting committed resolutions. Applied at
/// the consumer so the production-wiring report still classifies the
/// concrete store type registered with `HostRuntimeServices`.
pub(crate) fn metered_approval_requests(
    store: Arc<dyn ironclaw_approvals::ApprovalRequestStorePort>,
    metrics: Arc<OperatorMetrics>,
) -> Arc<dyn ironclaw_approvals::ApprovalRequestStorePort> {
    Arc::new(ironclaw_operator::MetricsApprovalRequestStore::new(
        store, metrics,
    ))
}

/// Runs whose start is remembered for the duration histogram. Past it the
/// oldest remembered start is evicted (its run, if it ever ends here, still
/// counts — just without a duration sample), so starts whose terminal event
/// this process never sees cannot wedge the map shut.
const MAX_TRACKED_RUN_STARTS: usize = 10_000;

#[derive(Default)]
struct RunStarts {
    by_run: HashMap<TurnRunId, (Timestamp, u64)>,
    /// Insertion order: sequence number → run, oldest first.
    order: BTreeMap<u64, TurnRunId>,
    next_seq: u64,
    capacity: usize,
}

impl RunStarts {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            ..Self::default()
        }
    }

    fn remember(&mut self, run_id: TurnRunId, started_at: Timestamp) {
        if self.by_run.contains_key(&run_id) {
            return;
        }
        while self.by_run.len() >= self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.by_run.remove(&oldest);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.order.insert(seq, run_id);
        self.by_run.insert(run_id, (started_at, seq));
    }

    fn take(&mut self, run_id: &TurnRunId) -> Option<Timestamp> {
        let (started_at, seq) = self.by_run.remove(run_id)?;
        self.order.remove(&seq);
        Some(started_at)
    }
}

pub(crate) struct OperatorMetricsTurnEventSink {
    metrics: Arc<OperatorMetrics>,
    started_at: Mutex<RunStarts>,
}

impl OperatorMetricsTurnEventSink {
    pub(crate) fn new(metrics: Arc<OperatorMetrics>) -> Self {
        Self::with_run_start_capacity(metrics, MAX_TRACKED_RUN_STARTS)
    }

    fn with_run_start_capacity(metrics: Arc<OperatorMetrics>, capacity: usize) -> Self {
        Self {
            metrics,
            started_at: Mutex::new(RunStarts::with_capacity(capacity)),
        }
    }

    fn started(&self) -> std::sync::MutexGuard<'_, RunStarts> {
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
                let started = self.started().take(&event.run_id);
                let duration = match (started, event.occurred_at) {
                    (Some(started), Some(ended)) => (ended - started).to_std().ok(),
                    _ => None,
                };
                self.metrics.record_turn_run(outcome, duration);
            }
            None => {
                if let Some(occurred_at) = event.occurred_at {
                    self.started().remember(event.run_id, occurred_at);
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

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_event_log::{
        EventCursor, EventError, EventLogEntry, EventReplay, EventStreamKey,
        InMemoryDurableEventLog, ReadScope, RuntimeEvent,
    };
    use ironclaw_host_api::ids::{CapabilityId, ExtensionId, InvocationId, TenantId, UserId};
    use ironclaw_host_api::resource::ResourceScope;
    use ironclaw_host_api::runtime::RuntimeKind;
    use tokio::sync::{Notify, Semaphore};

    /// A durable log whose batch append parks until released, so the
    /// coalescing sink's bounded queue can be filled deterministically; rows
    /// of `rejected` come back `Err` as a backend reject would.
    struct GatedLog {
        inner: InMemoryDurableEventLog,
        entered: Notify,
        release: Semaphore,
        rejected: &'static str,
    }

    #[async_trait]
    impl DurableEventLog for GatedLog {
        async fn append(
            &self,
            event: RuntimeEvent,
        ) -> Result<EventLogEntry<RuntimeEvent>, EventError> {
            if event.capability_id.as_str() == self.rejected {
                return Err(EventError::Sink {
                    reason: "rejected".to_string(),
                });
            }
            self.inner.append(event).await
        }

        async fn append_batch(
            &self,
            events: Vec<RuntimeEvent>,
        ) -> Vec<Result<EventLogEntry<RuntimeEvent>, EventError>> {
            self.entered.notify_one();
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
            let mut out = Vec::with_capacity(events.len());
            for event in events {
                out.push(self.append(event).await);
            }
            out
        }

        async fn read_after_cursor(
            &self,
            stream: &EventStreamKey,
            filter: &ReadScope,
            after: Option<EventCursor>,
            limit: usize,
        ) -> Result<EventReplay<RuntimeEvent>, EventError> {
            self.inner
                .read_after_cursor(stream, filter, after, limit)
                .await
        }

        async fn head_cursor(
            &self,
            stream: &EventStreamKey,
            after: EventCursor,
        ) -> Result<EventCursor, EventError> {
            self.inner.head_cursor(stream, after).await
        }
    }

    fn capability_succeeded(id: &str) -> RuntimeEvent {
        RuntimeEvent::capability_activity_succeeded(
            ResourceScope {
                tenant_id: TenantId::new("metrics-tenant").expect("tenant"),
                user_id: UserId::new("metrics-user").expect("user"),
                agent_id: None,
                project_id: None,
                mission_id: None,
                thread_id: None,
                invocation_id: InvocationId::new(),
            },
            CapabilityId::new(id).expect("capability id"),
            ExtensionId::new("builtin").expect("extension"),
            RuntimeKind::FirstParty,
            1,
        )
    }

    fn invocations(metrics: &OperatorMetrics, capability: &str) -> u64 {
        let series = format!(
            "ironclaw_capability_invocations_total{{capability_id=\"{capability}\",outcome=\"succeeded\"}} "
        );
        metrics
            .render_prometheus()
            .lines()
            .find_map(|line| line.strip_prefix(&series)?.parse().ok())
            .unwrap_or(0)
    }

    /// Production's sink is lossy under overload: `try_emit` answers `Ok`
    /// for an event its full queue drops. The metrics count what the drain
    /// durably committed, so dropped and backend-rejected events are absent.
    #[tokio::test]
    async fn runtime_event_metrics_count_durable_commits_not_queue_acceptance() {
        let metrics = Arc::new(OperatorMetrics::new());
        let log = Arc::new(GatedLog {
            inner: InMemoryDurableEventLog::new(),
            entered: Notify::new(),
            release: Semaphore::new(0),
            rejected: "builtin.rejected",
        });
        let sink = metered_runtime_event_sink(
            Arc::clone(&log) as Arc<dyn DurableEventLog>,
            EventBatchConfig {
                max_batch: 1,
                flush_interval: std::time::Duration::from_millis(1),
                channel_capacity: 1,
            },
            Arc::clone(&metrics),
        );

        // The drain takes the first event and parks inside `append_batch`.
        sink.try_emit(capability_succeeded("builtin.http"))
            .expect("accepted");
        log.entered.notified().await;
        // One more fits the queue; the rest are dropped, yet answer `Ok`.
        sink.try_emit(capability_succeeded("builtin.rejected"))
            .expect("queued");
        for _ in 0..3 {
            sink.try_emit(capability_succeeded("builtin.http"))
                .expect("a dropped event still answers Ok");
        }
        assert_eq!(
            invocations(&metrics, "builtin.http"),
            0,
            "nothing committed yet"
        );

        log.release.add_permits(16);
        sink.flush()
            .await
            .expect_err("the rejected row surfaces on flush");

        assert_eq!(
            invocations(&metrics, "builtin.http"),
            1,
            "only the committed event counts — not the three the full queue dropped"
        );
        assert_eq!(
            invocations(&metrics, "builtin.rejected"),
            0,
            "a backend-rejected row is not counted"
        );
    }

    fn run_id() -> TurnRunId {
        TurnRunId::new()
    }

    #[test]
    fn a_full_run_start_map_evicts_the_oldest_instead_of_refusing_new_runs() {
        let mut starts = RunStarts::with_capacity(2);
        let now = chrono::Utc::now();
        let (leaked, kept, newest) = (run_id(), run_id(), run_id());
        starts.remember(leaked, now);
        starts.remember(kept, now);
        // Re-remembering a tracked run keeps its first start and order.
        starts.remember(kept, now + chrono::Duration::seconds(5));
        starts.remember(newest, now);

        assert_eq!(starts.take(&leaked), None, "the oldest start was evicted");
        assert_eq!(starts.take(&kept), Some(now));
        assert_eq!(
            starts.take(&newest),
            Some(now),
            "a new run is still tracked"
        );
        assert!(starts.by_run.is_empty() && starts.order.is_empty());
    }
}
