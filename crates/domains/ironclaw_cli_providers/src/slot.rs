//! One thread's turn slot for the drivers that run an agent host per turn
//! (Antigravity, Muse Code): admission is an atomic take before any await
//! or spawn, the running turn is published into the slot once its host
//! runs, and the slot is released only by the [`Claim`] that took it, on
//! every exit (normal end, error, panic or a dropped future).
//!
//! The slot's cancellation flag exists from admission on: a Stop that
//! lands while the turn is still preparing (no host yet) sets it, the
//! driver checks it before spawning, and [`Claim::fill`] refuses a
//! cancelled slot, so the host is never launched (or is ended at once).
//!
//! [`GroupGuard`] is the matching process-side guard: it ends a host's whole
//! process group when its owner is dropped before the normal teardown ran.
//! [`ReportGuard`] reports a turn whose task panicked or was aborted as
//! failed, so the engine and the live view are never left waiting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnEventSink, TurnOutcome};
use tokio::sync::watch;

use crate::bridge::CliBridge;

struct Slot<T> {
    claim: u64,
    turn: Option<T>,
    cancelled: Arc<AtomicBool>,
    /// Closes when the claim is dropped.
    released: watch::Receiver<()>,
}

/// Every thread's slot for one runtime; `T` is what Stop, steer and the
/// live view need from the running turn.
pub(crate) struct Slots<T>(Arc<Mutex<HashMap<ThreadId, Slot<T>>>>);

impl<T> Clone for Slots<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for Slots<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

static NEXT_CLAIM: AtomicU64 = AtomicU64::new(1);

/// How long a Stop waits for a turn still preparing to report itself.
const UNFILLED_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// End a turn a Stop cancelled before its host ran: it is reported started
/// and then `Cancelled`, from its own task as a launched turn would be, and
/// its slot is freed before `turn_finished` (the engine may admit the next
/// turn from it).
pub(crate) fn report_cancelled<T: Send + 'static>(
    claim: Claim<T>,
    sink: Weak<dyn TurnEventSink>,
    thread: ThreadId,
    run: String,
) {
    tokio::spawn(async move {
        let sink = sink.upgrade();
        if let Some(sink) = &sink {
            sink.turn_started(&thread, Some(run.clone()), now_ms())
                .await;
        }
        let released = claim.release();
        if let Some(sink) = &sink {
            let outcome = TurnOutcome::ended(TurnEndStatus::Cancelled, None);
            sink.turn_finished(&thread, Some(run), outcome, now_ms())
                .await;
        }
        drop(released);
    });
}

/// Test seam: holds a turn between admission and its host's launch.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PrepGate {
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(test)]
impl PrepGate {
    pub(crate) async fn pass(gate: Option<&Arc<PrepGate>>) {
        if let Some(gate) = gate {
            gate.reached.notify_one();
            gate.release.notified().await;
        }
    }
}

/// What a Stop found in the thread's slot.
pub(crate) enum Cancel {
    /// Nothing holds the thread.
    Idle,
    /// Admitted but not launched yet: the flag is set, so the host never
    /// runs. The receiver closes once the driver has reported the turn and
    /// released the slot.
    Unfilled(watch::Receiver<()>),
    /// The turn runs; its flag is set and the driver's Stop goes on.
    Running,
}

impl<T> Slots<T> {
    /// Takes the thread's slot, or `None` when something already holds it.
    pub(crate) fn claim(&self, thread: &ThreadId) -> Option<Claim<T>> {
        let mut map = self.0.lock().ok()?; // silent-ok: a poisoned map reads as busy, which fails closed
        if map.contains_key(thread) {
            return None;
        }
        let id = NEXT_CLAIM.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (release, released) = watch::channel(());
        map.insert(
            thread.clone(),
            Slot {
                claim: id,
                turn: None,
                cancelled: Arc::clone(&cancelled),
                released,
            },
        );
        Some(Claim {
            slots: self.clone(),
            thread: thread.clone(),
            id,
            cancelled,
            _release: Some(release),
        })
    }

    /// Read the thread's running turn, once one was published.
    pub(crate) fn with_turn<R>(&self, thread: &ThreadId, read: impl FnOnce(&T) -> R) -> Option<R> {
        let map = self.0.lock().ok()?; // silent-ok: a poisoned map reads as no turn
        map.get(thread)?.turn.as_ref().map(read)
    }

    /// Stop's first step: cancel whatever holds the thread. `true` when a
    /// running turn needs the driver's own Stop; a turn admitted but not yet
    /// launched never launches, and is waited for (bounded) until reported.
    pub(crate) async fn stop_admitted(&self, thread: &ThreadId) -> bool {
        match self.cancel(thread) {
            Cancel::Idle => false,
            Cancel::Running => true,
            Cancel::Unfilled(mut released) => {
                if tokio::time::timeout(UNFILLED_STOP_WAIT, released.changed())
                    .await
                    .is_err()
                {
                    tracing::debug!("stop: an admitted turn is still preparing");
                }
                false
            }
        }
    }

    /// Mark whatever holds the thread cancelled. Checked and set under the
    /// same lock as [`Claim::fill`], so a Stop either sees the running turn
    /// or keeps it from ever being published.
    pub(crate) fn cancel(&self, thread: &ThreadId) -> Cancel {
        let Ok(map) = self.0.lock() else {
            return Cancel::Idle; // silent-ok: a poisoned map admits nothing, so nothing runs to stop
        };
        let Some(slot) = map.get(thread) else {
            return Cancel::Idle;
        };
        slot.cancelled.store(true, Ordering::SeqCst);
        if slot.turn.is_some() {
            Cancel::Running
        } else {
            Cancel::Unfilled(slot.released.clone())
        }
    }
}

/// The right to run one turn (or compaction) on a thread. Dropping it
/// frees the slot, and only the slot this claim took.
pub(crate) struct Claim<T> {
    slots: Slots<T>,
    thread: ThreadId,
    id: u64,
    cancelled: Arc<AtomicBool>,
    /// Kept until the turn is reported (see [`Claim::release`]); a Stop
    /// that found the slot unfilled waits for it to close.
    _release: Option<watch::Sender<()>>,
}

/// What a released claim keeps: the Stop waiting on an unfilled slot
/// returns once this is dropped, after the turn was reported.
pub(crate) struct Released {
    /// Held only to be dropped.
    _held: Option<watch::Sender<()>>,
}

impl<T> Claim<T> {
    /// Free the slot now (so the engine may admit the next turn from its
    /// `turn_finished`) while a Stop waiting on it keeps waiting until the
    /// returned value is dropped.
    pub(crate) fn release(mut self) -> Released {
        Released {
            _held: self._release.take(),
        }
    }

    /// Publishes the running turn so Stop, steer and the live view find it.
    /// `false` (nothing published) when a Stop already cancelled the slot:
    /// the caller ends what it spawned and reports the turn cancelled.
    #[must_use]
    pub(crate) fn fill(&self, turn: T) -> bool {
        if let Ok(mut map) = self.slots.0.lock()
            && let Some(slot) = map.get_mut(&self.thread)
            && slot.claim == self.id
            && !slot.cancelled.load(Ordering::SeqCst)
        {
            slot.turn = Some(turn);
            return true;
        }
        false
    }

    /// The slot's cancellation flag, set by Stop from admission on.
    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl<T> Drop for Claim<T> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.slots.0.lock()
            && map.get(&self.thread).is_some_and(|s| s.claim == self.id)
        {
            map.remove(&self.thread);
        }
    }
}

/// Reports a turn as failed when its task ends without the normal report
/// (a panic, or the task aborted): the bridge's live turn is ended and the
/// engine gets `turn_finished`, best effort from a spawned task.
pub(crate) struct ReportGuard {
    sink: Weak<dyn TurnEventSink>,
    // arch-exempt: optional_arc, a host without the gateway has no live view to end, plan #25
    bridge: Option<Arc<CliBridge>>,
    thread: ThreadId,
    run: String,
    /// A loopback MCP token bound to the turn, revoked with it.
    token: Option<String>,
    context: &'static str,
    armed: bool,
}

impl ReportGuard {
    pub(crate) fn new(
        sink: Weak<dyn TurnEventSink>,
        bridge: Option<Arc<CliBridge>>,
        thread: ThreadId,
        run: String,
        context: &'static str,
    ) -> Self {
        Self {
            sink,
            bridge,
            thread,
            run,
            token: None,
            context,
            armed: true,
        }
    }

    pub(crate) fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// The task is about to make the normal report.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReportGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::debug!(context = self.context, "turn task ended without its report");
        if let Some(bridge) = &self.bridge {
            if let Some(token) = &self.token {
                bridge.revoke_token(token);
            }
            bridge.turn_ended(&self.thread, &self.run, TurnEndStatus::Failed.as_str());
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(context = self.context, "no runtime to report the turn on");
            return;
        };
        let sink = self.sink.clone();
        let thread = self.thread.clone();
        let run = std::mem::take(&mut self.run);
        let context = self.context;
        drop(handle.spawn(async move {
            if let Some(sink) = sink.upgrade() {
                let outcome = TurnOutcome::ended(
                    TurnEndStatus::Failed,
                    Some(format!("{context}: the turn ended unexpectedly")),
                );
                sink.turn_finished(&thread, Some(run), outcome, now_ms())
                    .await;
            }
        }));
    }
}

/// Ends a host's whole process group if its owner is dropped before the
/// normal teardown ran (`kill_on_drop` reaches only the direct child).
pub(crate) struct GroupGuard {
    pgid: i32,
    context: &'static str,
    armed: bool,
}

impl GroupGuard {
    pub(crate) fn new(pgid: i32, context: &'static str) -> Self {
        Self {
            pgid,
            context,
            armed: true,
        }
    }

    /// The normal teardown ended the group.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            crate::rpc::kill_group(self.pgid, self.context);
        }
    }
}

/// Shared by the drivers' Stop-before-launch tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use ironclaw_orchestrator::model::{Millis, RuntimeId, ThreadRecord, ThreadScope};
    use ironclaw_orchestrator::runtime::{
        CreateThread, TurnAdmission, TurnInput, TurnOrigin, TurnRuntime,
    };

    #[derive(Default)]
    pub(crate) struct Recorder(pub(crate) Mutex<Vec<TurnOutcome>>);

    impl Recorder {
        pub(crate) fn sink(self: &Arc<Self>) -> Weak<dyn TurnEventSink> {
            Arc::downgrade(self) as Weak<dyn TurnEventSink>
        }
    }

    #[async_trait::async_trait]
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

    /// A host program that only records that it was launched.
    pub(crate) fn marker_host(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let marker = dir.join("launched");
        let host = dir.join("host");
        std::fs::write(&host, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        (host, marker)
    }

    /// Start a turn held at the prep gate, Stop it there, then let it go:
    /// the host must never launch, the turn is reported `Cancelled`, and
    /// Stop returns only after that report.
    pub(crate) async fn stop_before_launch<R: TurnRuntime + 'static>(
        rt: Arc<R>,
        gate: Arc<PrepGate>,
        recorder: Arc<Recorder>,
        marker: &std::path::Path,
    ) {
        let scope = ThreadScope {
            tenant_id: ironclaw_host_api::ids::TenantId::new("t").unwrap(),
            user_id: ironclaw_host_api::ids::UserId::new("u").unwrap(),
            agent_id: None,
            project_id: None,
        };
        let created = rt
            .create_thread(CreateThread {
                scope: scope.clone(),
                title: None,
                provider: None,
                model: Some("default".into()),
                idempotency_key: "k".into(),
            })
            .await
            .unwrap();
        let id: RuntimeId = rt.id();
        let mut thread = ThreadRecord::new(created.thread_id, scope, id);
        thread.native_session_id = created.native_session_id;
        let input = TurnInput {
            text: "hi".into(),
            origin: TurnOrigin::DelegatedBrief,
            model: None,
            goal: None,
            effort: None,
            profile: None,
            files: Vec::new(),
            work_dir: None,
        };
        let start = tokio::spawn({
            let (rt, thread) = (Arc::clone(&rt), thread.clone());
            async move { rt.start_turn(&thread, input).await }
        });
        gate.reached.notified().await;
        // The Stop's first poll cancels the slot; only then is the turn let
        // go on to its launch.
        let (stopped, ()) = tokio::join!(rt.cancel(&thread, "stop"), async {
            gate.release.notify_one();
        });
        stopped.unwrap();
        let finished = recorder.0.lock().unwrap().clone();
        assert_eq!(
            finished.len(),
            1,
            "Stop returned before the report: {finished:?}"
        );
        assert_eq!(finished[0].status, TurnEndStatus::Cancelled);
        assert!(matches!(
            start.await.unwrap().unwrap(),
            TurnAdmission::Started { .. }
        ));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!marker.exists(), "the host launched after Stop");
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_is_exclusive_and_frees_only_its_own_slot() {
        let slots: Slots<u8> = Slots::default();
        let thread = ThreadId::new("t1").unwrap();
        let first = slots.claim(&thread).unwrap();
        assert!(slots.claim(&thread).is_none(), "second admission");
        assert!(first.fill(7));
        assert_eq!(slots.with_turn(&thread, |t| *t), Some(7));
        drop(first);
        assert_eq!(slots.with_turn(&thread, |t| *t), None);
        let second = slots.claim(&thread).unwrap();
        // A stale claim (already released) never frees a newer holder,
        // nor fills its slot.
        let stale = Claim {
            slots: slots.clone(),
            thread: thread.clone(),
            id: 0,
            cancelled: Arc::new(AtomicBool::new(false)),
            _release: None,
        };
        assert!(!stale.fill(9));
        assert_eq!(slots.with_turn(&thread, |t| *t), None);
        drop(stale);
        assert!(slots.claim(&thread).is_none(), "newer slot freed");
        drop(second);
        assert!(slots.claim(&thread).is_some());
    }

    #[tokio::test]
    async fn a_claim_moved_into_a_panicking_task_frees_the_slot() {
        let slots: Slots<u8> = Slots::default();
        let thread = ThreadId::new("t1").unwrap();
        let claim = slots.claim(&thread).unwrap();
        let task = tokio::spawn(async move {
            assert!(claim.fill(1));
            panic!("turn task panicked");
        });
        assert!(task.await.is_err());
        assert!(slots.claim(&thread).is_some(), "slot stranded");
    }

    // Review (2nd round, B): a Stop that lands between admission and
    // launch cancels the slot, the turn is never published, and the Stop
    // can wait for the slot's release; a Stop on a running turn sets the
    // flag the turn shares.
    #[tokio::test]
    async fn stop_before_fill_cancels_the_unfilled_slot() {
        let slots: Slots<u8> = Slots::default();
        let thread = ThreadId::new("t1").unwrap();
        assert!(matches!(slots.cancel(&thread), Cancel::Idle));

        let claim = slots.claim(&thread).unwrap();
        let Cancel::Unfilled(mut released) = slots.cancel(&thread) else {
            panic!("an admitted, unfilled slot");
        };
        assert!(claim.is_cancelled());
        assert!(!claim.fill(1), "a cancelled slot is never published");
        assert_eq!(slots.with_turn(&thread, |t| *t), None);
        drop(claim);
        tokio::time::timeout(std::time::Duration::from_secs(1), released.changed())
            .await
            .expect("release seen")
            .expect_err("the claim's drop closes the watch");

        let claim = slots.claim(&thread).unwrap();
        assert!(!claim.is_cancelled(), "a new claim starts clean");
        let flag = claim.cancel_flag();
        assert!(claim.fill(2));
        assert!(matches!(slots.cancel(&thread), Cancel::Running));
        assert!(flag.load(Ordering::SeqCst));
    }

    // Review (2nd round, B): a turn task that panics still reports the
    // turn failed to the engine and ends the bridge's live turn; a disarmed
    // guard reports nothing.
    #[tokio::test]
    async fn a_panicking_turn_task_is_reported_failed() {
        let recorder = Arc::new(test_support::Recorder::default());
        let sink = recorder.sink();
        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let thread = ThreadId::new("t1").unwrap();
        bridge.turn_started(&thread, "run-1");
        let guard = ReportGuard::new(
            sink.clone(),
            Some(Arc::clone(&bridge)),
            thread.clone(),
            "run-1".into(),
            "test",
        );
        let task = tokio::spawn(async move {
            let _guard = guard;
            panic!("turn task panicked");
        });
        assert!(task.await.is_err());
        for _ in 0..200 {
            if !recorder.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let finished = recorder.0.lock().unwrap().clone();
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert_eq!(finished[0].status, TurnEndStatus::Failed);
        assert!(bridge.snapshot(&thread).is_none(), "live turn left open");

        let mut quiet = ReportGuard::new(sink, None, thread, "run-2".into(), "test");
        quiet.disarm();
        drop(quiet);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_dropped_group_guard_kills_the_group() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = crate::rpc::group_of(&child).unwrap();
        drop(GroupGuard::new(pgid, "test"));
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("the group was not killed")
            .unwrap();
        assert!(!status.success());

        // A disarmed guard leaves the group alone.
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut guard = GroupGuard::new(crate::rpc::group_of(&child).unwrap(), "test");
        guard.disarm();
        drop(guard);
        assert!(child.try_wait().unwrap().is_none());
    }
}
