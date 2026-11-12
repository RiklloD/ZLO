//! Observing decorators over the durable owners the metrics are folded from.
//!
//! Each records only **after** the inner owner committed the fact, and never
//! changes the inner result: a metrics fold is observability, so it may not
//! turn a success into a failure or swallow an error.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_approvals::{ApprovalRecord, ApprovalRequestStorePort, ApprovalStoreError};
use ironclaw_event_log::{
    DurableEventLog, EventCursor, EventError, EventLogEntry, EventReplay, EventStreamKey,
    ReadScope, RuntimeEvent,
};
use ironclaw_host_api::approval::ApprovalRequest;
use ironclaw_host_api::ids::ApprovalRequestId;
use ironclaw_host_api::resource::ResourceScope;

use super::{ApprovalResolution, OperatorMetrics};

/// Folds every runtime event the inner durable event log **commits** into
/// [`OperatorMetrics`].
///
/// Production hands this to the write-behind `CoalescingEventSink` as the log
/// its drain task appends to, so producers stay non-blocking while the
/// metrics see exactly the rows that became durable: an event the sink's
/// bounded queue drops under overload never reaches `append_batch`, and a row
/// the backend rejects comes back `Err` and is not counted. The fold reads the
/// committed [`EventLogEntry::record`], so nothing is cloned on the hot path.
pub struct MetricsDurableEventLog {
    inner: Arc<dyn DurableEventLog>,
    metrics: Arc<OperatorMetrics>,
}

impl MetricsDurableEventLog {
    pub fn new(inner: Arc<dyn DurableEventLog>, metrics: Arc<OperatorMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl std::fmt::Debug for MetricsDurableEventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsDurableEventLog")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DurableEventLog for MetricsDurableEventLog {
    async fn append(&self, event: RuntimeEvent) -> Result<EventLogEntry<RuntimeEvent>, EventError> {
        let entry = self.inner.append(event).await?;
        self.metrics.record_runtime_event(&entry.record);
        Ok(entry)
    }

    async fn append_batch(
        &self,
        events: Vec<RuntimeEvent>,
    ) -> Vec<Result<EventLogEntry<RuntimeEvent>, EventError>> {
        let results = self.inner.append_batch(events).await;
        for entry in results.iter().flatten() {
            self.metrics.record_runtime_event(&entry.record);
        }
        results
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

/// Counts approval-gate resolutions as their durable records commit:
/// `approve`, `deny`. A failed store call records nothing. `discard_pending`
/// is delegated uncounted — a capability-side rollback of a gate that never
/// became user-actionable, so never counted opened either (module docs).
/// Gates *opened* are counted from the committed turn `Blocked` transition
/// instead ([`OperatorMetrics::record_approval_gate_opened`]), so wrapping a
/// store that also takes `save_pending` cannot double count.
pub struct MetricsApprovalRequestStore {
    inner: Arc<dyn ApprovalRequestStorePort>,
    metrics: Arc<OperatorMetrics>,
}

impl MetricsApprovalRequestStore {
    pub fn new(inner: Arc<dyn ApprovalRequestStorePort>, metrics: Arc<OperatorMetrics>) -> Self {
        Self { inner, metrics }
    }
}

#[async_trait]
impl ApprovalRequestStorePort for MetricsApprovalRequestStore {
    async fn save_pending(
        &self,
        scope: ResourceScope,
        request: ApprovalRequest,
    ) -> Result<ApprovalRecord, ApprovalStoreError> {
        self.inner.save_pending(scope, request).await
    }

    async fn get(
        &self,
        scope: &ResourceScope,
        request_id: ApprovalRequestId,
    ) -> Result<Option<ApprovalRecord>, ApprovalStoreError> {
        self.inner.get(scope, request_id).await
    }

    async fn approve(
        &self,
        scope: &ResourceScope,
        request_id: ApprovalRequestId,
    ) -> Result<ApprovalRecord, ApprovalStoreError> {
        let record = self.inner.approve(scope, request_id).await?;
        self.metrics
            .record_approval_resolved(ApprovalResolution::Approved);
        Ok(record)
    }

    async fn deny(
        &self,
        scope: &ResourceScope,
        request_id: ApprovalRequestId,
    ) -> Result<ApprovalRecord, ApprovalStoreError> {
        let record = self.inner.deny(scope, request_id).await?;
        self.metrics
            .record_approval_resolved(ApprovalResolution::Denied);
        Ok(record)
    }

    async fn discard_pending(
        &self,
        scope: &ResourceScope,
        request_id: ApprovalRequestId,
    ) -> Result<ApprovalRecord, ApprovalStoreError> {
        // Delegate to the inner override (some stores tombstone rather than
        // deny); the trait default would route through `self.deny` and count
        // a rollback as a user denial.
        self.inner.discard_pending(scope, request_id).await
    }

    async fn records_for_scope(
        &self,
        scope: &ResourceScope,
    ) -> Result<Vec<ApprovalRecord>, ApprovalStoreError> {
        self.inner.records_for_scope(scope).await
    }
}
