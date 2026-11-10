//! In-memory fakes for the extension-tier reply port, used by contract tests
//! and downstream adapter tests.
//!
//! The product-tier half of the original module (projection stream, inbound
//! ack/rejection helpers) moved with its DTOs to
//! `ironclaw_product_contracts::test_support::fakes`; a fake belongs beside the
//! port it implements.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use ironclaw_host_api::turn::ReplyTargetBindingRef;

pub fn fake_reply_target(suffix: &str) -> ReplyTargetBindingRef {
    ReplyTargetBindingRef::new(format!("reply:fake-{suffix}")).expect("valid reply target") // safety: test-support helper prefixes caller suffix into bounded ref
}

/// A recording [`crate::reply::ReplySink`]: captures every reconcile request
/// (so a test can prove which revisions reached the edge, at which cadence
/// point, and with which checkpoint) and answers from a scripted outcome
/// queue, defaulting to `Applied` with a checkpoint that echoes the revision
/// it applied.
///
/// The publication worker, the binding rule, and the coordinator's
/// reply-publication bookkeeping are all tested against it.
pub struct RecordingReplySink {
    requests: Mutex<Vec<crate::reply::ReplyReconcileRequest>>,
    scripted: Mutex<VecDeque<crate::reply::ReplySinkOutcome>>,
    provider_ref_prefix: String,
}

impl Default for RecordingReplySink {
    fn default() -> Self {
        Self::new("provider-ref")
    }
}

impl RecordingReplySink {
    pub fn new(provider_ref_prefix: impl Into<String>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            scripted: Mutex::new(VecDeque::new()),
            provider_ref_prefix: provider_ref_prefix.into(),
        }
    }

    /// Queue outcomes for the next reconcile calls, in order. Once the queue
    /// drains the sink answers `Applied` again.
    pub fn script(&self, outcomes: impl IntoIterator<Item = crate::reply::ReplySinkOutcome>) {
        let mut scripted = self.scripted.lock().expect("fake sink lock poisoned"); // safety: test-support fake; poisoned mutex means another test already panicked;
        scripted.extend(outcomes);
    }

    pub fn requests(&self) -> Vec<crate::reply::ReplyReconcileRequest> {
        self.requests
            .lock()
            .expect("fake sink lock poisoned") // safety: test-support fake; poisoned mutex means another test already panicked;
            .clone()
    }

    /// The revisions that reached this sink, in call order.
    pub fn revisions(&self) -> Vec<u64> {
        self.requests()
            .iter()
            .map(|request| request.revision.revision)
            .collect()
    }

    /// The cadence points that reached this sink, in call order.
    pub fn points(&self) -> Vec<crate::reply::ReplyReconcilePoint> {
        self.requests()
            .iter()
            .map(|request| request.point)
            .collect()
    }
}

#[async_trait]
impl crate::reply::ReplySink for RecordingReplySink {
    async fn reconcile(
        &self,
        request: crate::reply::ReplyReconcileRequest,
        _egress: &dyn crate::tool_adapter::RestrictedEgress,
    ) -> Result<crate::reply::ReplySinkReport, crate::channel_adapter::ChannelError> {
        let revision = request.revision.revision;
        let terminal = request.revision.document.is_terminal();
        self.requests
            .lock()
            .expect("fake sink lock poisoned") // safety: test-support fake; poisoned mutex means another test already panicked;
            .push(request);
        let outcome = self
            .scripted
            .lock()
            .expect("fake sink lock poisoned") // safety: test-support fake; poisoned mutex means another test already panicked;
            .pop_front()
            .unwrap_or(crate::reply::ReplySinkOutcome::Applied);
        // Like a real sink that opened a provider presentation before the
        // provider answered, a non-applied outcome still hands back the
        // checkpoint describing what was started.
        let checkpoint_payload = match &outcome {
            crate::reply::ReplySinkOutcome::Applied => format!("applied:{revision}"),
            crate::reply::ReplySinkOutcome::Retryable { .. } => format!("retryable:{revision}"),
            crate::reply::ReplySinkOutcome::Ambiguous { .. } => format!("ambiguous:{revision}"),
            crate::reply::ReplySinkOutcome::Permanent { .. } => format!("permanent:{revision}"),
            crate::reply::ReplySinkOutcome::Unauthorized { .. } => {
                format!("unauthorized:{revision}")
            }
            crate::reply::ReplySinkOutcome::StoppedByUser => String::new(),
        };
        let checkpoint = crate::reply::ReplySinkCheckpoint::new(1, checkpoint_payload.clone())
            .expect("checkpoint within bound"); // safety: test-support fake with a fixed small payload.
        let mut evidence = crate::reply::ReplySinkEvidence::default();
        if outcome.is_applied() {
            let reference = crate::reply::ReplyProviderRef::new(format!(
                "{}:{revision}",
                self.provider_ref_prefix
            ))
            .expect("provider ref within bound"); // safety: test-support fake with a fixed small ref.
            evidence
                .provider_refs
                .push(reference)
                .expect("one ref per report is within the bound"); // safety: test-support fake pushes a single ref.
            evidence.read_back_verified = terminal;
        }
        Ok(crate::reply::ReplySinkReport {
            checkpoint: (!checkpoint_payload.is_empty()).then_some(checkpoint),
            outcome,
            evidence,
        })
    }
}
