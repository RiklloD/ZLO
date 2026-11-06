//! Idle compaction for Codex threads, on the same rules as the Claude CLI's
//! (`claude/compact.rs`): a thread that sits idle for [`IDLE_DELAY`] after a
//! completed turn with at least [`MIN_CONTEXT_TOKENS`] of context runs
//! `thread/compact/start` while the provider's prompt cache is still warm,
//! when `Settings::idle_compact_claude` is on (read through the sink at fire
//! time). Every turn start, steer restart, Stop and thread action preempts
//! the timer and kills a compaction already running, and waits until its
//! app-server has exited. A compaction never re-arms the timer and is not
//! reported to the engine as a turn; the journal gets one "Context
//! compacted." row.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnEventSink};
use tokio::sync::watch;

use super::actions::{self, Compacted};
use super::admission::Admission;
use super::{CodexConfig, CodexRuntime};

/// Idle time before compacting: half a minute inside a 5-minute cache TTL.
const IDLE_DELAY: Duration = Duration::from_secs(270);
/// Below this many context tokens a compaction does not pay for itself.
const MIN_CONTEXT_TOKENS: u64 = 20_000;

/// Whether a finished turn arms the idle timer.
fn should_arm(status: TurnEndStatus, context_used: Option<u64>) -> bool {
    status == TurnEndStatus::Completed && context_used.is_some_and(|t| t >= MIN_CONTEXT_TOKENS)
}

/// One pending timer (or running compaction) per thread.
#[derive(Default)]
pub(super) struct IdleCompactor {
    slots: Mutex<HashMap<ThreadId, Slot>>,
    next_id: AtomicU64,
}

struct Slot {
    id: u64,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

impl IdleCompactor {
    /// Cancel the thread's timer and kill its running compaction, returning
    /// once the compaction's app-server has exited.
    /// The slot stays registered until its work reports done, so a second
    /// caller (or `has_slot`) still sees the compaction while it winds down
    /// and waits on the same signal.
    pub(super) async fn preempt(&self, thread: &ThreadId) {
        // silent-ok: a poisoned map only follows a panic elsewhere; nothing to cancel then
        let slot = self.slots.lock().ok().and_then(|slots| {
            let slot = slots.get(thread)?;
            let _ = slot.cancel.send(true); // silent-ok: the task already ended
            Some((slot.id, slot.done.clone()))
        });
        let Some((id, mut done)) = slot else {
            return;
        };
        let _ = done.wait_for(|d| *d).await; // silent-ok: sender dropped means the task ended
        self.finish(thread, id);
        tracing::debug!(thread = %thread, "codex idle compaction: preempted");
    }

    #[cfg(test)]
    fn has_slot(&self, thread: &ThreadId) -> bool {
        self.slots.lock().is_ok_and(|s| s.contains_key(thread))
    }

    /// Register a slot for `thread` (replacing a timer): its id, the cancel
    /// signal the work watches, and the sender it flips when its process
    /// is gone.
    pub(super) fn begin(
        &self,
        thread: &ThreadId,
    ) -> (u64, watch::Receiver<bool>, watch::Sender<bool>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (cancel, cancelled) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);
        let old = self
            .slots
            .lock()
            .ok()
            .and_then(|mut slots| slots.insert(thread.clone(), Slot { id, cancel, done }));
        if let Some(old) = old {
            let _ = old.cancel.send(true); // silent-ok: the task already ended
        }
        (id, cancelled, done_tx)
    }

    pub(super) fn finish(&self, thread: &ThreadId, id: u64) {
        if let Ok(mut slots) = self.slots.lock()
            && slots.get(thread).is_some_and(|s| s.id == id)
        {
            slots.remove(thread);
        }
    }
}

/// What a finished turn needs to compact its thread later.
pub(super) struct IdleArm {
    config: Arc<CodexConfig>,
    sink: Weak<dyn TurnEventSink>,
    bridged: bool,
    admission: Arc<Admission>,
    compactor: Arc<IdleCompactor>,
    thread_id: ThreadId,
    session: Option<String>,
    model: Option<String>,
}

impl IdleArm {
    /// Captured in `spawn_turn`, before the turn's task starts.
    pub(super) fn new(rt: &CodexRuntime, thread: &ThreadRecord) -> Self {
        Self {
            config: Arc::clone(&rt.config),
            sink: rt.sink.clone(),
            bridged: rt.bridge.is_some(),
            admission: Arc::clone(&rt.admission),
            compactor: Arc::clone(&rt.compactor),
            thread_id: thread.thread_id.clone(),
            session: thread.native_session_id.clone(),
            model: thread.model.clone(),
        }
    }

    /// Called once the turn is reported finished and no longer live.
    pub(super) fn arm(self, status: TurnEndStatus, context_used: Option<u64>) {
        if !should_arm(status, context_used) || self.session.is_none() {
            return;
        }
        let compactor = Arc::clone(&self.compactor);
        let thread = self.thread_id.clone();
        let (id, mut cancelled, done) = compactor.begin(&thread);
        tokio::spawn(async move {
            let idle = tokio::select! {
                _ = tokio::time::sleep(IDLE_DELAY) => true,
                _ = cancelled.wait_for(|c| *c) => false,
            };
            if idle {
                self.fire(cancelled).await;
            }
            compactor.finish(&thread, id);
            let _ = done.send(true); // silent-ok: nobody waits on this slot
        });
    }

    async fn fire(self, cancelled: watch::Receiver<bool>) {
        let Some(session) = self.session.as_deref() else {
            return;
        };
        // The session is ours until the compaction's process is gone; a
        // turn or action holding it means the thread is not idle.
        let Some(_claim) = self.admission.try_claim(&self.thread_id, "idle compaction") else {
            return;
        };
        let Some(sink) = self.sink.upgrade() else {
            return;
        };
        if !sink.idle_compact_enabled().await {
            return;
        }
        let result = actions::run_compaction(
            &self.config,
            session,
            self.model.as_deref(),
            self.bridged,
            cancelled,
        )
        .await;
        tracing::debug!(thread = %self.thread_id, ?result, "codex idle compaction");
        if result == Compacted::Done {
            actions::note_compacted(&self.config, session).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preempt_keeps_the_slot_until_the_compaction_has_shut_down() {
        let compactor = Arc::new(IdleCompactor::default());
        let thread = ThreadId::new("t").unwrap();
        let (_id, mut cancelled, done) = compactor.begin(&thread);
        let preempting = {
            let (compactor, thread) = (Arc::clone(&compactor), thread.clone());
            tokio::spawn(async move { compactor.preempt(&thread).await })
        };
        cancelled.wait_for(|c| *c).await.unwrap();
        // Cancelled but still shutting down: the slot is still there, and a
        // second preempt waits too.
        assert!(compactor.has_slot(&thread));
        let second = {
            let (compactor, thread) = (Arc::clone(&compactor), thread.clone());
            tokio::spawn(async move { compactor.preempt(&thread).await })
        };
        tokio::task::yield_now().await;
        assert!(!preempting.is_finished() && !second.is_finished());
        done.send(true).unwrap();
        preempting.await.unwrap();
        second.await.unwrap();
        assert!(!compactor.has_slot(&thread));
    }

    #[test]
    fn only_completed_turns_with_a_large_context_arm_the_timer() {
        assert!(should_arm(
            TurnEndStatus::Completed,
            Some(MIN_CONTEXT_TOKENS)
        ));
        assert!(!should_arm(
            TurnEndStatus::Completed,
            Some(MIN_CONTEXT_TOKENS - 1)
        ));
        assert!(!should_arm(TurnEndStatus::Completed, None));
        assert!(!should_arm(TurnEndStatus::Failed, Some(150_000)));
        assert!(!should_arm(TurnEndStatus::Cancelled, Some(150_000)));
    }
}
