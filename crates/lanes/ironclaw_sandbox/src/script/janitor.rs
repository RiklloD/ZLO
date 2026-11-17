//! The single owner of `docker` helper processes (`rm`, `ps`, …) that did
//! not finish inside their bound.
//!
//! A helper that times out or whose poll fails is killed and given a short
//! wait; if it still has not been waited, it is handed here instead of being
//! dropped (dropping a `Child` never waits it, which leaves a zombie). One
//! worker thread kills and polls every held child until it is waited, with
//! backoff growing to a capped interval. A failed worker spawn leaves the
//! children held, and the next hand-off retries the spawn, so a wedged Docker
//! daemon cannot make unwaited helper processes accumulate.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use super::docker::ScriptChild;
use super::orphans::WorkerSpawner;

/// Waits between sweeps of the held children; the last value repeats.
const SWEEP_BACKOFF: [Duration; 4] = [
    Duration::from_millis(20),
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

pub(crate) struct ChildJanitor {
    pub(super) state: Arc<JanitorState>,
    spawner: WorkerSpawner,
}

pub(super) struct JanitorState {
    held: Mutex<Vec<Box<dyn ScriptChild>>>,
    worker_running: AtomicBool,
}

impl JanitorState {
    pub(super) fn held(&self) -> MutexGuard<'_, Vec<Box<dyn ScriptChild>>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ChildJanitor {
    pub(crate) fn new(spawner: WorkerSpawner) -> Self {
        Self {
            state: Arc::new(JanitorState {
                held: Mutex::new(Vec::new()),
                worker_running: AtomicBool::new(false),
            }),
            spawner,
        }
    }

    /// Take ownership of a child that has not been waited yet.
    pub(crate) fn adopt(&self, child: Box<dyn ScriptChild>) {
        self.state.held().push(child);
        self.ensure_worker();
    }

    fn ensure_worker(&self) {
        if self.state.worker_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(&self.state);
        let spawned = (self.spawner)(
            "ironclaw-script-child-janitor",
            Box::new(move || {
                // A worker that dies anyway must not leave the flag set, or
                // no later hand-off would start a new one.
                if catch_unwind(AssertUnwindSafe(|| run_worker(&state))).is_err() {
                    state.worker_running.store(false, Ordering::SeqCst);
                }
            }),
        );
        if let Err(error) = spawned {
            // The children stay held; the next hand-off retries the spawn.
            self.state.worker_running.store(false, Ordering::SeqCst);
            tracing::debug!(%error, "spawning the docker child janitor failed; children stay held");
        }
    }
}

/// Kill and wait a helper that missed its bound: a short bounded wait, then
/// the janitor if it is still not waited.
pub(crate) fn settle_or_hand_off(
    mut child: Box<dyn ScriptChild>,
    janitor: &ChildJanitor,
    bound: Duration,
) {
    if let Err(error) = child.kill() {
        tracing::debug!(%error, "killing a docker helper process failed");
    }
    let deadline = std::time::Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "waiting a docker helper process failed");
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    janitor.adopt(child);
}

fn run_worker(state: &JanitorState) {
    let mut sweep = 0_usize;
    loop {
        let delay = SWEEP_BACKOFF
            .get(sweep)
            .or(SWEEP_BACKOFF.last())
            .copied()
            .unwrap_or(Duration::from_secs(1));
        thread::sleep(delay);
        let mut held = state.held();
        if catch_unwind(AssertUnwindSafe(|| sweep_children(&mut held))).is_err() {
            tracing::debug!("a docker child janitor sweep panicked; retrying");
        }
        if held.is_empty() {
            // Cleared under the lock, so a concurrent `adopt` either sees the
            // worker still running or starts a new one.
            state.worker_running.store(false, Ordering::SeqCst);
            return;
        }
        sweep = sweep.saturating_add(1);
    }
}

/// Keep only the children that are still not waited (killing them again).
fn sweep_children(held: &mut Vec<Box<dyn ScriptChild>>) {
    held.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) => {
            if let Err(error) = child.kill() {
                tracing::debug!(%error, "killing a held docker helper process failed");
            }
            true
        }
        Err(error) => {
            tracing::debug!(%error, "waiting a held docker helper process failed");
            true
        }
    });
}
