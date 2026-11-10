//! Background work and the way back to the UI thread.
//!
//! Rune runs every handler on the UI thread with `&mut State`. Anything slow
//! (gateway requests, file reads, timers) runs elsewhere and lands through
//! the app's [`AppHandle`], whose `update` queues a closure that runs on the
//! UI thread with `&mut State` before the next frame.
//!
//! This replaces the GPUI app's `jobs::run(backend, cx, f, done)`: there is
//! no per-view entity any more, so `done` receives the whole `State` and must
//! find its own sub-state again (and check it is still the one that asked,
//! e.g. the same thread id; see [`Generation`]).

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use rune::{AppHandle, Tx};
use zlo_client::{Backend, Client, ClientError};

use crate::state::State;

/// The runtime services every handler can reach through `State::rt`.
#[derive(Clone)]
pub struct Rt {
    /// Queue work onto the UI thread from any thread.
    pub handle: AppHandle<State>,
    /// The gateway, once connected.
    pub backend: Option<Backend>,
}

impl Rt {
    pub fn new(handle: AppHandle<State>) -> Self {
        Self {
            handle,
            backend: None,
        }
    }
}

pub fn flatten<T>(
    joined: Result<Result<T, ClientError>, tokio::task::JoinError>,
) -> Result<T, ClientError> {
    joined.map_err(|e| ClientError::Config(format!("background task failed: {e}")))?
}

/// `f` runs on the network runtime; `done` runs on the UI thread with the
/// whole state. `label` names the caller in the log when the request fails.
/// A reply asked on an earlier connection (another computer) is dropped.
/// Without a backend (not connected yet) `done` gets a `Config` error at once,
/// on the next frame.
pub fn run<F, Fut, T>(
    s: &State,
    label: &'static str,
    f: F,
    done: impl FnOnce(&mut State, Result<T, ClientError>, &mut Tx) + Send + 'static,
) where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    // A fixture scene's state is not the gateway's: never send its requests
    // (their replies would be dropped anyway).
    if s.fixture {
        return;
    }
    let handle = s.rt.handle.clone();
    // Asked on this connection: a reply that lands after a switch to
    // another computer (or a reconnect) is dropped (`remote::lands`).
    let tag = s.remote.tag.clone();
    let Some(backend) = s.rt.backend.as_ref() else {
        handle.update(move |s, tx| {
            if s.fixture || !crate::remote::lands(s, &tag) {
                return;
            }
            done(
                s,
                Err(ClientError::Config("not connected to the gateway".into())),
                tx,
            )
        });
        return;
    };
    let job = backend.spawn(f);
    backend.spawn(move |_| async move {
        let result = flatten(job.await);
        if let Err(e) = &result {
            tracing::warn!(view = label, error = %e, "gateway request failed");
        }
        handle.update(move |s, tx| {
            crate::energy::hit(crate::energy::C::UpdateJob);
            if s.fixture {
                return;
            }
            if !crate::remote::lands(s, &tag) {
                tracing::debug!(view = label, "reply from an earlier connection dropped");
                return;
            }
            done(s, result, tx)
        });
    });
}

/// Run blocking work (disk, decoding) on a plain thread, then land it.
pub fn blocking<T: Send + 'static>(
    s: &State,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&mut State, T, &mut Tx) + Send + 'static,
) {
    let handle = s.rt.handle.clone();
    std::thread::spawn(move || {
        let out = work();
        handle.update(move |s, tx| {
            crate::energy::hit(crate::energy::C::UpdateBlocking);
            if !s.fixture {
                done(s, out, tx)
            }
        });
    });
}

/// Run `f` on the UI thread after `delay`. For polls and retries; check in
/// `f` that the state still wants it (a [`Generation`] does that cheaply).
/// For per-frame or debounce timing prefer `tx.wake_after` plus `poll`.
///
/// The wait is an entry in the app's one timer queue ([`timers`]), not a
/// thread of its own. The queue belongs to the process, not to a gateway
/// connection: a timer set before the first connection, or across a switch
/// to another computer, still fires.
pub fn after(s: &State, delay: Duration, f: impl FnOnce(&mut State, &mut Tx) + Send + 'static) {
    let handle = s.rt.handle.clone();
    timers::schedule(delay, move || {
        handle.update(move |s, tx| {
            crate::energy::hit(crate::energy::C::UpdateTimer);
            if !s.fixture {
                f(s, tx)
            }
        });
    });
}

/// The app's timer queue: every pending [`after`] is one entry, and one
/// thread sleeps until the earliest is due. It starts with the first timer
/// and lives as long as the process.
mod timers {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;
    use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    type Job = Box<dyn FnOnce() + Send>;

    struct Entry {
        at: Instant,
        /// Order of asking: timers due at the same instant run in it.
        seq: u64,
        job: Job,
    }

    impl PartialEq for Entry {
        fn eq(&self, other: &Self) -> bool {
            self.at == other.at && self.seq == other.seq
        }
    }

    impl Eq for Entry {}

    impl PartialOrd for Entry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    // Reversed: `BinaryHeap` is a max-heap and the earliest entry goes first.
    impl Ord for Entry {
        fn cmp(&self, other: &Self) -> Ordering {
            (other.at, other.seq).cmp(&(self.at, self.seq))
        }
    }

    #[derive(Default)]
    struct Queue {
        entries: BinaryHeap<Entry>,
        next_seq: u64,
    }

    struct Timers {
        queue: Mutex<Queue>,
        changed: Condvar,
    }

    fn timers() -> &'static Timers {
        static TIMERS: OnceLock<Timers> = OnceLock::new();
        static STARTED: OnceLock<()> = OnceLock::new();
        let timers = TIMERS.get_or_init(|| Timers {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
        });
        STARTED.get_or_init(|| {
            let spawned = std::thread::Builder::new()
                .name("zlo-timers".into())
                .spawn(move || run(timers));
            if let Err(e) = spawned {
                tracing::error!(error = %e, "timer thread not started: delayed work will not run");
            }
        });
        timers
    }

    /// Run `job` on the timer thread once `delay` has passed. Keep `job`
    /// short (hand the work to the UI thread): every timer shares the thread.
    pub(super) fn schedule(delay: Duration, job: impl FnOnce() + Send + 'static) {
        let timers = timers();
        let mut queue = timers.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = queue.next_seq;
        queue.next_seq += 1;
        queue.entries.push(Entry {
            at: Instant::now() + delay,
            seq,
            job: Box::new(job),
        });
        drop(queue);
        // The new entry may be the earliest: the sleeper re-reads its wait.
        timers.changed.notify_one();
    }

    fn run(timers: &'static Timers) {
        let mut queue = timers.queue.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let now = Instant::now();
            match queue.entries.peek().map(|e| e.at) {
                Some(at) if at <= now => {
                    let Some(entry) = queue.entries.pop() else {
                        continue;
                    };
                    // Jobs run unlocked, so one may set the next timer.
                    drop(queue);
                    (entry.job)();
                    queue = timers.queue.lock().unwrap_or_else(PoisonError::into_inner);
                }
                Some(at) => {
                    queue = timers
                        .changed
                        .wait_timeout(queue, at - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                None => {
                    queue = timers
                        .changed
                        .wait(queue)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc;
        use std::thread::ThreadId;

        use super::*;

        const SOON: Duration = Duration::from_secs(5);

        #[test]
        fn timers_fire_by_deadline_whatever_order_they_were_set_in() {
            let (tx, rx) = mpsc::channel();
            for ms in [120u64, 20, 70] {
                let tx = tx.clone();
                schedule(Duration::from_millis(ms), move || {
                    let _ = tx.send(ms);
                });
            }
            let fired: Vec<u64> = (0..3).filter_map(|_| rx.recv_timeout(SOON).ok()).collect();
            assert_eq!(fired, [20, 70, 120]);
        }

        #[test]
        fn a_timer_never_fires_before_its_delay() {
            let (tx, rx) = mpsc::channel();
            let delay = Duration::from_millis(60);
            let set = Instant::now();
            schedule(delay, move || {
                let _ = tx.send(Instant::now());
            });
            let fired = rx.recv_timeout(SOON).expect("the timer fires");
            assert!(fired - set >= delay);
        }

        /// A short timer set while the queue sleeps toward a long one (a
        /// debounce beside a minute-long poll) is not held up by it.
        #[test]
        fn a_short_timer_set_behind_a_long_one_fires_on_time() {
            let (tx, rx) = mpsc::channel();
            let long = tx.clone();
            schedule(Duration::from_secs(600), move || {
                let _ = long.send("long");
            });
            schedule(Duration::from_millis(30), move || {
                let _ = tx.send("short");
            });
            assert_eq!(rx.recv_timeout(SOON), Ok("short"));
        }

        /// The point of the queue: timers are entries, not threads. A job
        /// may set the next timer (every poll loop does).
        #[test]
        fn every_timer_runs_on_the_one_timer_thread() {
            let (tx, rx) = mpsc::channel::<ThreadId>();
            for ms in 1..=20u64 {
                let tx = tx.clone();
                schedule(Duration::from_millis(ms), move || {
                    let again = tx.clone();
                    let _ = tx.send(std::thread::current().id());
                    schedule(Duration::from_millis(1), move || {
                        let _ = again.send(std::thread::current().id());
                    });
                });
            }
            let ids: Vec<ThreadId> = (0..40).filter_map(|_| rx.recv_timeout(SOON).ok()).collect();
            assert_eq!(ids.len(), 40, "every timer fired");
            assert!(ids.iter().all(|id| *id == ids[0]));
            assert_ne!(ids[0], std::thread::current().id());
        }
    }
}

/// A token that invalidates earlier async replies: bump it when the thing
/// that asked goes away (thread switched, page closed, query changed), and
/// compare the captured value in `done`.
///
/// Every bump (and [`Generation::unique`]) takes the next value of one
/// app-wide counter, so values are never reused, not even by a slot that was
/// dropped and created again: a chat closed and reopened (A → B → A) starts
/// from a fresh value, and a reply or timer of the old one can never match.
/// `Default` (0) is never handed out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Generation(pub u64);

impl Generation {
    /// A value no other `Generation` in this process has held.
    pub fn unique() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn bump(&mut self) -> Self {
        *self = Self::unique();
        *self
    }
}

/// Names one instance of a slot (a chat's extras, say) for its timers and
/// replies: a fresh [`Generation::unique`] each time the slot is created,
/// never bumped. A timer that captured it stops once the slot it belongs to
/// was replaced, even by one for the same thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance(Generation);

impl Default for Instance {
    fn default() -> Self {
        Self(Generation::unique())
    }
}

/// "just now", "5 min ago", "3 h ago", then a local date.
pub fn ago(at: DateTime<Utc>) -> String {
    let secs = (Utc::now() - at).num_seconds();
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        _ => when(at),
    }
}

/// Local wall-clock time, e.g. "23.10. 14:05".
pub fn when(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%d.%m. %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::{Generation, Instance};

    #[test]
    fn every_instance_is_distinct() {
        assert_ne!(Instance::default(), Instance::default());
    }

    #[test]
    fn a_recreated_slot_never_repeats_an_earlier_generation() {
        let mut first = Generation::default();
        let old = first.bump();
        // The slot is dropped and made again from its default.
        let mut second = Generation::default();
        assert_ne!(second.bump(), old);
        assert_ne!(Generation::unique(), Generation::default());
    }
}
