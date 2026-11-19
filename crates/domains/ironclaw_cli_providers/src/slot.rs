//! One thread's turn slot for the drivers that run an agent host per turn
//! (Antigravity, Muse Code): admission is an atomic take before any await
//! or spawn, the running turn is published into the slot once its host
//! runs, and the slot is released only by the [`Claim`] that took it, on
//! every exit (normal end, error, panic or a dropped future).
//!
//! [`GroupGuard`] is the matching process-side guard: it ends a host's whole
//! process group when its owner is dropped before the normal teardown ran.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;

struct Slot<T> {
    claim: u64,
    turn: Option<T>,
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

impl<T> Slots<T> {
    /// Takes the thread's slot, or `None` when something already holds it.
    pub(crate) fn claim(&self, thread: &ThreadId) -> Option<Claim<T>> {
        let mut map = self.0.lock().ok()?; // silent-ok: a poisoned map reads as busy, which fails closed
        if map.contains_key(thread) {
            return None;
        }
        let id = NEXT_CLAIM.fetch_add(1, Ordering::Relaxed);
        map.insert(
            thread.clone(),
            Slot {
                claim: id,
                turn: None,
            },
        );
        Some(Claim {
            slots: self.clone(),
            thread: thread.clone(),
            id,
        })
    }

    /// Read the thread's running turn, once one was published.
    pub(crate) fn with_turn<R>(&self, thread: &ThreadId, read: impl FnOnce(&T) -> R) -> Option<R> {
        let map = self.0.lock().ok()?; // silent-ok: a poisoned map reads as no turn
        map.get(thread)?.turn.as_ref().map(read)
    }
}

/// The right to run one turn (or compaction) on a thread. Dropping it
/// frees the slot, and only the slot this claim took.
pub(crate) struct Claim<T> {
    slots: Slots<T>,
    thread: ThreadId,
    id: u64,
}

impl<T> Claim<T> {
    /// Publishes the running turn so Stop, steer and the live view find it.
    pub(crate) fn fill(&self, turn: T) {
        if let Ok(mut map) = self.slots.0.lock()
            && let Some(slot) = map.get_mut(&self.thread)
            && slot.claim == self.id
        {
            slot.turn = Some(turn);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_is_exclusive_and_frees_only_its_own_slot() {
        let slots: Slots<u8> = Slots::default();
        let thread = ThreadId::new("t1").unwrap();
        let first = slots.claim(&thread).unwrap();
        assert!(slots.claim(&thread).is_none(), "second admission");
        first.fill(7);
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
        };
        stale.fill(9);
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
            claim.fill(1);
            panic!("turn task panicked");
        });
        assert!(task.await.is_err());
        assert!(slots.claim(&thread).is_some(), "slot stranded");
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
