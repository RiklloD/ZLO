//! A Codex turn's teardown, owned by a drop guard so it happens however the
//! turn's task ends: normally, by a panic, or because the runtime dropped
//! the task. Dropped without [`TurnEnd::finish`], it clears the turn's live
//! state, frees the session, closes the bridge's live turn, reports the
//! turn failed to the engine (unless it was already being told) and wakes
//! anyone waiting in `stop`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnEventSink, TurnOutcome};
use tokio::sync::watch;

use super::ActiveTurn;
use super::admission::Claim;
use crate::bridge::CliBridge;

pub(super) struct TurnEnd {
    thread_id: ThreadId,
    run: String,
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
    // arch-exempt: optional_arc, mirrors CodexRuntime::bridge (tools-off hosts), plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    sink: Weak<dyn TurnEventSink>,
    claim: Option<Claim>,
    /// `None` once finished normally.
    done: Option<watch::Sender<bool>>,
    settled: bool,
    /// The engine is being told how the turn ended: never tell it twice.
    reported: bool,
}

/// What a turn's task needs to tear it down, gathered in `spawn_turn`.
pub(super) struct TurnEndParts {
    pub(super) thread_id: ThreadId,
    pub(super) run: String,
    pub(super) active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
    // arch-exempt: optional_arc, mirrors CodexRuntime::bridge (tools-off hosts), plan #v3-cli-bridge
    pub(super) bridge: Option<Arc<CliBridge>>,
    pub(super) sink: Weak<dyn TurnEventSink>,
    pub(super) claim: Claim,
    pub(super) done: watch::Sender<bool>,
}

impl TurnEnd {
    pub(super) fn new(parts: TurnEndParts) -> Self {
        Self {
            thread_id: parts.thread_id,
            run: parts.run,
            active: parts.active,
            bridge: parts.bridge,
            sink: parts.sink,
            claim: Some(parts.claim),
            done: Some(parts.done),
            settled: false,
            reported: false,
        }
    }

    /// The turn's process is gone: close its live turn, forget it, and free
    /// the session (before the engine hears, as it may start the next turn
    /// at once). Idempotent.
    pub(super) fn settle(&mut self, status: &'static str) {
        if self.settled {
            return;
        }
        self.settled = true;
        if let Some(bridge) = &self.bridge {
            bridge.turn_ended(&self.thread_id, &self.run, status);
        }
        if let Ok(mut map) = self.active.lock() {
            map.remove(&self.thread_id);
        }
        self.claim = None;
    }

    /// The engine is about to be told how the turn ended.
    pub(super) fn reporting(&mut self) {
        self.reported = true;
    }

    /// Normal end: wake `stop`'s waiters.
    pub(super) fn finish(mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(true); // silent-ok: nobody waiting on this turn
        }
    }
}

impl Drop for TurnEnd {
    fn drop(&mut self) {
        let Some(done) = self.done.take() else {
            return; // finished normally
        };
        self.settle(TurnEndStatus::Failed.as_str());
        if !self.reported
            && let (Ok(handle), Some(sink)) =
                (tokio::runtime::Handle::try_current(), self.sink.upgrade())
        {
            let (thread, run) = (self.thread_id.clone(), self.run.clone());
            tracing::debug!(thread = %thread, "codex: turn task ended without finishing");
            handle.spawn(async move {
                let outcome = TurnOutcome::ended(
                    TurnEndStatus::Failed,
                    Some("codex turn ended unexpectedly".into()),
                );
                sink.turn_finished(&thread, Some(run), outcome, now_ms())
                    .await;
            });
        }
        let _ = done.send(true); // silent-ok: nobody waiting on this turn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::DEFAULT_APPROVAL_TIMEOUT;
    use async_trait::async_trait;
    use ironclaw_orchestrator::model::Millis;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<TurnOutcome>>);

    #[async_trait]
    impl TurnEventSink for Recorder {
        async fn turn_started(&self, _: &ThreadId, _: Option<String>, _: Millis) {}
        async fn turn_finished(
            &self,
            _: &ThreadId,
            _: Option<String>,
            outcome: TurnOutcome,
            _: Millis,
        ) {
            self.0.lock().unwrap().push(outcome);
        }
    }

    /// A turn task that panics still clears the active map, frees the
    /// session, closes the bridge's live turn, wakes `stop` and reports the
    /// turn failed to the engine.
    #[tokio::test]
    async fn a_panicking_turn_is_cleaned_up_and_reported() {
        let thread = ThreadId::new("t").unwrap();
        let admission = Arc::new(super::super::admission::Admission::default());
        let claim = admission.try_claim(&thread, "turn").unwrap();
        let (done_tx, done_rx) = watch::channel(false);
        let active = Arc::new(Mutex::new(HashMap::from([(
            thread.clone(),
            ActiveTurn {
                pgid: 0,
                cancelled: Arc::new(AtomicBool::new(false)),
                live: Arc::new(Mutex::new(None)),
                done: done_rx.clone(),
            },
        )])));
        let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
        bridge.turn_started(&thread, "r");
        let recorder = Arc::new(Recorder::default());
        let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
        let end = TurnEnd::new(TurnEndParts {
            thread_id: thread.clone(),
            run: "r".into(),
            active: Arc::clone(&active),
            bridge: Some(Arc::clone(&bridge)),
            sink,
            claim,
            done: done_tx,
        });
        let task = tokio::spawn(async move {
            let _end = end;
            panic!("fold blew up");
        });
        assert!(task.await.unwrap_err().is_panic());

        assert!(active.lock().unwrap().is_empty());
        assert!(admission.holder(&thread).is_none());
        assert!(bridge.snapshot(&thread).is_none());
        assert!(*done_rx.borrow());
        for _ in 0..200 {
            if !recorder.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let outcomes = recorder.0.lock().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, TurnEndStatus::Failed);
    }

    /// A normal finish reports nothing extra.
    #[tokio::test]
    async fn a_finished_turn_is_not_reported_again() {
        let thread = ThreadId::new("t").unwrap();
        let admission = Arc::new(super::super::admission::Admission::default());
        let (done_tx, done_rx) = watch::channel(false);
        let recorder = Arc::new(Recorder::default());
        let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
        let mut end = TurnEnd::new(TurnEndParts {
            thread_id: thread.clone(),
            run: "r".into(),
            active: Arc::default(),
            bridge: None,
            sink,
            claim: admission.try_claim(&thread, "turn").unwrap(),
            done: done_tx,
        });
        end.settle(TurnEndStatus::Completed.as_str());
        assert!(admission.holder(&thread).is_none());
        end.reporting();
        end.finish();
        tokio::task::yield_now().await;
        assert!(*done_rx.borrow());
        assert!(recorder.0.lock().unwrap().is_empty());
    }
}
