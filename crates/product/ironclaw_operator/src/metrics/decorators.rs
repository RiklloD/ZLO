//! Observing decorators over the durable sinks the metrics are folded from.
//!
//! Each records only **after** the inner owner accepted the fact, and never
//! changes the inner result: a metrics fold is observability, so it may not
//! turn a success into a failure or swallow an error.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_approvals::{ApprovalRecord, ApprovalRequestStorePort, ApprovalStoreError};
use ironclaw_event_log::{EventError, EventSink, NonBlockingEventSink, RuntimeEvent};
use ironclaw_host_api::approval::ApprovalRequest;
use ironclaw_host_api::ids::ApprovalRequestId;
use ironclaw_host_api::resource::ResourceScope;

use super::{ApprovalResolution, OperatorMetrics};

/// Folds every runtime event the inner (durable) sink accepts into
/// [`OperatorMetrics`]. Sits directly outside the runtime's coalescing durable
/// sink, so the metrics see exactly the events the event log is handed.
pub struct MetricsEventSink {
    inner: Arc<dyn NonBlockingEventSink>,
    metrics: Arc<OperatorMetrics>,
}

impl MetricsEventSink {
    pub fn new(inner: Arc<dyn NonBlockingEventSink>, metrics: Arc<OperatorMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl std::fmt::Debug for MetricsEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsEventSink").finish_non_exhaustive()
    }
}

#[async_trait]
impl EventSink for MetricsEventSink {
    async fn emit(&self, event: RuntimeEvent) -> Result<(), EventError> {
        let observed = event.clone();
        self.inner.emit(event).await?;
        self.metrics.record_runtime_event(&observed);
        Ok(())
    }

    async fn flush(&self) -> Result<(), EventError> {
        self.inner.flush().await
    }
}

impl NonBlockingEventSink for MetricsEventSink {
    fn try_emit(&self, event: RuntimeEvent) -> Result<(), EventError> {
        let observed = event.clone();
        self.inner.try_emit(event)?;
        self.metrics.record_runtime_event(&observed);
        Ok(())
    }
}

/// Counts approval-gate resolutions as their durable records commit:
/// `approve`, `deny`, `discard_pending`. A failed store call records nothing.
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
        // Delegate to the inner override (some stores delete rather than
        // tombstone); the default would route through `self.deny` and count
        // a rollback as a user denial.
        let record = self.inner.discard_pending(scope, request_id).await?;
        self.metrics
            .record_approval_resolved(ApprovalResolution::Discarded);
        Ok(record)
    }

    async fn records_for_scope(
        &self,
        scope: &ResourceScope,
    ) -> Result<Vec<ApprovalRecord>, ApprovalStoreError> {
        self.inner.records_for_scope(scope).await
    }
}
