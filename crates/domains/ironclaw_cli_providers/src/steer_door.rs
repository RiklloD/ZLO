//! Where a steer meets a turn of an RPC-driven agent (Codex, Muse Code):
//! the turn's steer state from admission to its end.
//!
//! - **Opening** (host spawning, session opening, `turn/start` not answered
//!   yet): a steer is accepted and held. Once the turn id is known the
//!   driver delivers the held steers in order, and only then opens the door,
//!   so a steer that arrives meanwhile joins the held ones instead of
//!   overtaking them (T3 Code's "steer when ready": never a separate later
//!   turn).
//! - **Open**: the steer goes straight to the live turn.
//! - **Closed**: the turn ended; a steer is refused and the engine queues it.
//!
//! Held steers the turn never delivered (it failed to start, was stopped,
//! or refused them) come back from [`SteerDoor::close`] and are reported
//! through [`report_unread`], the same path Claude's unread steers take, so
//! the engine requeues them ahead of later messages.

use std::sync::{Mutex, MutexGuard, PoisonError, Weak};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::runtime::{TurnEventSink, TurnOutcome, UnreadSteer};

enum State<L> {
    Opening(Vec<UnreadSteer>),
    Open(L),
    Closed,
}

/// What a steer found at the door.
pub(crate) enum Knock<L> {
    /// Accepted and held for the turn's start.
    Held,
    /// The live turn: deliver it now.
    Live(L),
    /// No turn takes it any more.
    Closed,
}

pub(crate) struct SteerDoor<L> {
    state: Mutex<State<L>>,
}

impl<L> Default for SteerDoor<L> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::Opening(Vec::new())),
        }
    }
}

impl<L: Clone> SteerDoor<L> {
    /// A door that never opens: for work that takes no steers (an idle
    /// compaction), so a steer is refused and queued, never held.
    pub(crate) fn closed() -> Self {
        Self {
            state: Mutex::new(State::Closed),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<L>> {
        // A poisoned lock only follows a panic elsewhere; the state is intact.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn knock(&self, text: &str) -> Knock<L> {
        match &mut *self.lock() {
            State::Opening(held) => {
                held.push(UnreadSteer {
                    id: uuid::Uuid::new_v4().to_string(),
                    text: text.to_string(),
                });
                Knock::Held
            }
            State::Open(live) => Knock::Live(live.clone()),
            State::Closed => Knock::Closed,
        }
    }

    /// The turn can take steers: the held ones, oldest first, for the
    /// driver to deliver; it calls again until this returns nothing, and
    /// that call opens the door (atomically with finding nothing held).
    pub(crate) fn drain_or_open(&self, live: &L) -> Vec<UnreadSteer> {
        let mut state = self.lock();
        match &mut *state {
            State::Opening(held) if !held.is_empty() => std::mem::take(held),
            State::Opening(_) => {
                *state = State::Open(live.clone());
                Vec::new()
            }
            State::Open(_) | State::Closed => Vec::new(),
        }
    }

    /// The live turn, once the door is open.
    pub(crate) fn live(&self) -> Option<L> {
        match &*self.lock() {
            State::Open(live) => Some(live.clone()),
            State::Opening(_) | State::Closed => None,
        }
    }

    /// The turn ended (or never started): nothing reaches it any more.
    pub(crate) fn is_closed(&self) -> bool {
        matches!(&*self.lock(), State::Closed)
    }

    /// Change the live turn in place (no-op unless open).
    pub(crate) fn update(&self, change: impl FnOnce(&mut L)) {
        if let State::Open(live) = &mut *self.lock() {
            change(live);
        }
    }

    /// The turn ended: later steers are refused. Returns the steers still
    /// held, oldest first; they were never delivered.
    pub(crate) fn close(&self) -> Vec<UnreadSteer> {
        match std::mem::replace(&mut *self.lock(), State::Closed) {
            State::Opening(held) => held,
            State::Open(_) | State::Closed => Vec::new(),
        }
    }
}

/// Hand steers the turn never delivered to the engine, before the turn is
/// reported finished (so no queue drain runs between the two). One the
/// engine could not keep fails the turn, naming it.
pub(crate) async fn report_unread(
    sink: &Weak<dyn TurnEventSink>,
    thread: &ThreadId,
    run: &str,
    unread: Vec<UnreadSteer>,
    outcome: TurnOutcome,
) -> TurnOutcome {
    if unread.is_empty() {
        return outcome;
    }
    let lost = match sink.upgrade() {
        Some(sink) => sink.steers_unread(thread, run, unread).await.undelivered,
        None => unread,
    };
    outcome.with_undelivered_steers(&lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(steers: &[UnreadSteer]) -> Vec<&str> {
        steers.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn held_steers_are_delivered_in_order_before_the_door_opens() {
        let door = SteerDoor::<u8>::default();
        assert!(matches!(door.knock("one"), Knock::Held));
        assert!(matches!(door.knock("two"), Knock::Held));
        assert!(door.live().is_none());
        assert_eq!(texts(&door.drain_or_open(&7)), ["one", "two"]);
        // Arrives while the first batch is being delivered: still held.
        assert!(matches!(door.knock("three"), Knock::Held));
        assert_eq!(texts(&door.drain_or_open(&7)), ["three"]);
        assert!(door.drain_or_open(&7).is_empty());
        assert!(matches!(door.knock("four"), Knock::Live(7)));
        door.update(|l| *l = 8);
        assert_eq!(door.live(), Some(8));
        assert!(!door.is_closed());
        assert!(door.close().is_empty());
        assert!(door.is_closed());
        assert!(matches!(door.knock("late"), Knock::Closed));
    }

    #[test]
    fn a_turn_that_never_opens_returns_what_it_held() {
        let door = SteerDoor::<u8>::default();
        assert!(matches!(door.knock("early"), Knock::Held));
        assert_eq!(texts(&door.close()), ["early"]);
        assert!(matches!(door.knock("late"), Knock::Closed));
    }
}
