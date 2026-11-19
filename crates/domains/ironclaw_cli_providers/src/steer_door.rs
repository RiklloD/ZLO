//! Where a steer meets a turn of an RPC-driven agent (Codex, Muse Code),
//! from admission to the turn's end, in one order.
//!
//! The runtime knocks at the turn's [`SteerDoor`]; every accepted steer goes
//! onto one channel the turn's own loop reads, in the order they came:
//! - **Opening** (host spawning, session opening, `turn/start` unanswered):
//!   the steer is accepted at once and held in the channel (T3 Code's "steer
//!   when ready": never a separate later turn);
//! - **Open**: the requester waits for the loop's verdict;
//! - **Closed**: refused, and the engine queues it.
//!
//! The loop ([`SteerPipe`]) takes them only once the turn has an id, while it
//! keeps reading the agent's events (completion, approvals), and never once
//! Stop has won. Each steer cuts the reply so far at the moment it is sent;
//! entries reach the transcript in that order, after the steer settles:
//! - **delivered** (answered): the reply before it, then its row;
//! - **refused** (an error answer, or the frame never reached the agent):
//!   the reply before it only; a held steer comes back through
//!   [`report_unread`] (the engine requeues it), a waiting one is refused;
//! - **unconfirmed** (no answer, and the turn ended meanwhile): its row and
//!   a "Steer delivery not confirmed" notice, never requeued (it may have
//!   been read) and never claimed read.
//!
//! The open door also carries the live turn, which Stop needs to interrupt
//! it, from the moment the turn has an id.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError, Weak};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::runtime::{
    HistoryMessage, HistoryRole, SteerOutcome, TurnEventSink, TurnOutcome, UnreadSteer,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::journal;
use crate::rpc::{PendingReply, RpcError, Written};

/// One accepted steer on its way to the turn's loop.
pub(crate) struct SteerRequest {
    pub(crate) steer: UnreadSteer,
    /// Who waits for the verdict; `None` for one accepted while the turn
    /// was starting (already answered `Steered`).
    waiter: Option<oneshot::Sender<SteerOutcome>>,
}

impl SteerRequest {
    fn answer(self, outcome: SteerOutcome) -> Option<UnreadSteer> {
        match self.waiter {
            Some(waiter) => {
                if waiter.send(outcome).is_err() {
                    tracing::debug!("steer: the requester stopped waiting");
                }
                None
            }
            // Held: it was answered `Steered` already; one refused goes
            // back to the engine.
            None => (outcome == SteerOutcome::Rejected).then_some(self.steer),
        }
    }
}

enum State<L> {
    Opening,
    Open(L),
    Closed,
}

/// What a steer found at the door.
pub(crate) enum Knock {
    /// Accepted while the turn starts; the turn delivers it or reports it.
    Held,
    /// Handed to the live turn: its verdict arrives here.
    Sent(oneshot::Receiver<SteerOutcome>),
    /// No turn takes it any more.
    Closed,
}

pub(crate) struct SteerDoor<L> {
    inner: Mutex<(State<L>, Option<mpsc::UnboundedSender<SteerRequest>>)>,
}

impl<L: Clone> SteerDoor<L> {
    /// A turn's door and the receiver its loop reads steers from.
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<SteerRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let door = Self {
            inner: Mutex::new((State::Opening, Some(tx))),
        };
        (door, rx)
    }

    /// A door that never opens: for work that takes no steers (an idle
    /// compaction), so a steer is refused and queued, never held.
    pub(crate) fn closed() -> Self {
        Self {
            inner: Mutex::new((State::Closed, None)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, (State<L>, Option<mpsc::UnboundedSender<SteerRequest>>)> {
        // A poisoned lock only follows a panic elsewhere; the state is intact.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Accept `text` for the turn, decided under the door's lock: a close
    /// either sees it in the channel (and reports it) or came first.
    pub(crate) fn knock(&self, text: &str) -> Knock {
        let mut inner = self.lock();
        let steer = UnreadSteer {
            id: uuid::Uuid::new_v4().to_string(),
            text: text.to_string(),
        };
        let (waiter, knock) = match &inner.0 {
            State::Closed => return Knock::Closed,
            State::Opening => (None, Knock::Held),
            State::Open(_) => {
                let (tx, rx) = oneshot::channel();
                (Some(tx), Knock::Sent(rx))
            }
        };
        let Some(tx) = inner.1.as_ref() else {
            return Knock::Closed;
        };
        if tx.send(SteerRequest { steer, waiter }).is_err() {
            inner.0 = State::Closed;
            return Knock::Closed;
        }
        knock
    }

    /// The turn has an id: Stop can interrupt it, steers wait for verdicts.
    pub(crate) fn open(&self, live: L) {
        let mut inner = self.lock();
        if matches!(inner.0, State::Opening) {
            inner.0 = State::Open(live);
        }
    }

    /// The live turn, once the door is open.
    pub(crate) fn live(&self) -> Option<L> {
        match &self.lock().0 {
            State::Open(live) => Some(live.clone()),
            State::Opening | State::Closed => None,
        }
    }

    /// Change the live turn in place (no-op unless open).
    pub(crate) fn update(&self, change: impl FnOnce(&mut L)) {
        if let State::Open(live) = &mut self.lock().0 {
            change(live);
        }
    }

    /// The turn ended: nothing more joins the channel.
    pub(crate) fn close(&self) {
        let mut inner = self.lock();
        inner.0 = State::Closed;
        inner.1 = None;
    }
}

/// How a sent steer ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Delivered,
    Refused,
    Unconfirmed,
}

type Reply = Pin<Box<dyn Future<Output = Result<Value, RpcError>> + Send>>;

/// Once the turn has ended, how long a steer still in flight may take to be
/// answered: an agent that has not answered by its turn's end rarely will,
/// and the turn's end waits on it.
const FINISH_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

struct InFlight {
    request: SteerRequest,
    /// The reply the steer cut short, journaled ahead of its row.
    segment: Option<HistoryMessage>,
    reply: Reply,
    written: Written,
    /// When its answer is due; the grace after the turn ends caps it.
    due: tokio::time::Instant,
}

/// What settling steers produced, for the loop to act on in order.
#[derive(Default)]
pub(crate) struct Settled {
    /// Transcript entries, in order, under the run.
    pub(crate) entries: Vec<HistoryMessage>,
    /// Held steers that never reached the agent.
    pub(crate) unread: Vec<UnreadSteer>,
}

impl Settled {
    fn extend(&mut self, other: Settled) {
        self.entries.extend(other.entries);
        self.unread.extend(other.unread);
    }
}

/// The turn loop's side: steers taken from the door's channel (the loop
/// reads that receiver itself), sent in order, settled in order.
pub(crate) struct SteerPipe {
    inflight: VecDeque<InFlight>,
    run_id: String,
}

impl SteerPipe {
    pub(crate) fn new(run_id: &str) -> Self {
        Self {
            inflight: VecDeque::new(),
            run_id: run_id.to_string(),
        }
    }

    /// Steers sent and not settled yet.
    pub(crate) fn busy(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// The oldest sent steer's answer (cancel-safe: the future stays with
    /// its steer). Only call while [`Self::busy`].
    pub(crate) async fn answer(&mut self) -> Result<Value, RpcError> {
        match self.inflight.front_mut() {
            Some(head) => head.reply.as_mut().await,
            None => std::future::pending().await,
        }
    }

    /// `request` was sent (or failed to be, `started` is `Err`), cutting
    /// the reply before it into `segment`. A send that failed at once
    /// settles now, behind any still in flight.
    pub(crate) fn sent(
        &mut self,
        request: SteerRequest,
        segment: Option<HistoryMessage>,
        started: Result<(PendingReply, Written), RpcError>,
        timeout: std::time::Duration,
    ) {
        // Timed from now, not from when the loop gets to it.
        let due = tokio::time::Instant::now() + timeout;
        let (reply, written): (Reply, Written) = match started {
            Ok((pending, written)) => (Box::pin(pending.wait_until(due)), written),
            Err(error) => {
                let (tx, rx) = oneshot::channel();
                #[expect(clippy::let_underscore_must_use, reason = "the receiver is right here")]
                let _ = tx.send(false);
                (Box::pin(std::future::ready(Err(error))), rx)
            }
        };
        self.inflight.push_back(InFlight {
            request,
            segment,
            reply,
            written,
            due,
        });
    }

    /// Settle the oldest sent steer with its answer. `running`: the turn is
    /// still going (its loop has not seen it end).
    pub(crate) fn settle(&mut self, answer: Result<Value, RpcError>, running: bool) -> Settled {
        let Some(mut head) = self.inflight.pop_front() else {
            return Settled::default();
        };
        // `false` only when the writer says the frame never went out.
        let reached = !matches!(
            head.written.try_recv(),
            Ok(false) | Err(oneshot::error::TryRecvError::Closed)
        );
        let delivery = match answer {
            Ok(_) => Delivery::Delivered,
            Err(_) if !reached => Delivery::Refused,
            Err(RpcError::Remote { .. }) => Delivery::Refused,
            // No answer while the turn still runs: Codex and Muse answer a
            // steer they refuse at once, so a slow answer is a slow agent.
            Err(RpcError::Timeout) if running => Delivery::Delivered,
            // On the wire, and the turn ended (or the agent went away)
            // without answering: there is no telling.
            Err(RpcError::Timeout | RpcError::Closed) => Delivery::Unconfirmed,
        };
        let mut settled = Settled::default();
        settled
            .entries
            .extend(head.segment.take().map(|e| self.in_run(e)));
        let text = head.request.steer.text.clone();
        let outcome = match delivery {
            Delivery::Delivered => {
                settled.entries.push(self.row(HistoryRole::User, text));
                SteerOutcome::Steered
            }
            Delivery::Unconfirmed => {
                settled
                    .entries
                    .push(self.row(HistoryRole::User, text.clone()));
                settled
                    .entries
                    .push(self.row(HistoryRole::System, not_confirmed(&text)));
                // It may have been read: never queued again.
                SteerOutcome::Steered
            }
            Delivery::Refused => SteerOutcome::Rejected,
        };
        settled.unread.extend(head.request.answer(outcome));
        settled
    }

    /// A steer the loop will not send (Stop won, or the turn is over).
    pub(crate) fn refuse(&self, request: SteerRequest) -> Settled {
        Settled {
            entries: Vec::new(),
            unread: request.answer(SteerOutcome::Rejected).into_iter().collect(),
        }
    }

    /// The turn ended: close the door, settle what is in flight (in order),
    /// and refuse what was never sent.
    /// `stopped`: Stop ended the turn, so nothing in flight is waited for
    /// (it is unconfirmed at once; Stop must not wait on it).
    pub(crate) async fn finish<L: Clone>(
        &mut self,
        door: &SteerDoor<L>,
        rx: &mut mpsc::UnboundedReceiver<SteerRequest>,
        stopped: bool,
    ) -> Settled {
        door.close();
        let mut settled = Settled::default();
        let grace = tokio::time::Instant::now()
            + if stopped {
                std::time::Duration::ZERO
            } else {
                FINISH_GRACE
            };
        while let Some(head) = self.inflight.front_mut() {
            let deadline = head.due.min(grace);
            let answer = match tokio::time::timeout_at(deadline, head.reply.as_mut()).await {
                Ok(answer) => answer,
                Err(_) => Err(RpcError::Timeout),
            };
            settled.extend(self.settle(answer, false));
        }
        rx.close();
        while let Ok(request) = rx.try_recv() {
            settled.extend(self.refuse(request));
        }
        settled
    }

    fn in_run(&self, entry: HistoryMessage) -> HistoryMessage {
        HistoryMessage {
            run_id: Some(self.run_id.clone()),
            ..entry
        }
    }

    fn row(&self, role: HistoryRole, text: String) -> HistoryMessage {
        self.in_run(journal::message(role, text, Vec::new()))
    }
}

/// T3 Code's notice for a steer nobody can say was read.
fn not_confirmed(text: &str) -> String {
    format!(
        "Steer delivery not confirmed: the turn ended before the agent acknowledged this \
         message, so it may not have been delivered. Resend it if needed: \"{text}\""
    )
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

    #[tokio::test]
    async fn steers_keep_their_order_and_a_closed_door_refuses() {
        let (door, mut rx) = SteerDoor::<u8>::new();
        let mut pipe = SteerPipe::new("run");
        assert!(matches!(door.knock("one"), Knock::Held));
        door.open(7);
        assert_eq!(door.live(), Some(7));
        let Knock::Sent(verdict) = door.knock("two") else {
            panic!("live");
        };
        let one = rx.recv().await.unwrap();
        let two = rx.recv().await.unwrap();
        assert_eq!(
            (one.steer.text.as_str(), two.steer.text.as_str()),
            ("one", "two")
        );
        // One refused at once, the other never sent: in order, neither lost.
        pipe.sent(
            one,
            None,
            Err(RpcError::Closed),
            std::time::Duration::from_secs(1),
        );
        let settled = pipe.finish(&door, &mut rx, false).await;
        assert_eq!(settled.unread.len(), 1, "the held one goes back");
        assert!(settled.entries.is_empty());
        // Taken from the channel but never sent (Stop won): refused.
        assert!(pipe.refuse(two).unread.is_empty());
        assert_eq!(verdict.await.unwrap(), SteerOutcome::Rejected);
        assert!(matches!(door.knock("late"), Knock::Closed));
    }

    #[tokio::test]
    async fn an_unanswered_steer_after_the_turn_ended_is_unconfirmed_not_requeued() {
        let (door, mut rx) = SteerDoor::<u8>::new();
        let mut pipe = SteerPipe::new("run");
        assert!(matches!(door.knock("maybe"), Knock::Held));
        let request = rx.recv().await.unwrap();
        let (tx, written) = oneshot::channel();
        tx.send(true).unwrap();
        pipe.inflight.push_back(InFlight {
            request,
            segment: Some(journal::message(
                HistoryRole::Assistant,
                "before".into(),
                Vec::new(),
            )),
            reply: Box::pin(std::future::pending()),
            written,
            due: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        });
        let settled = pipe.finish(&door, &mut rx, false).await;
        assert!(
            settled.unread.is_empty(),
            "never requeued: it may have been read"
        );
        let rows: Vec<(HistoryRole, &str)> = settled
            .entries
            .iter()
            .map(|e| (e.role, e.text.as_str()))
            .collect();
        assert_eq!(rows[0], (HistoryRole::Assistant, "before"));
        assert_eq!(rows[1], (HistoryRole::User, "maybe"));
        assert_eq!(rows[2].0, HistoryRole::System);
        assert!(rows[2].1.contains("not confirmed") && rows[2].1.contains("maybe"));
        assert!(
            settled
                .entries
                .iter()
                .all(|e| e.run_id.as_deref() == Some("run"))
        );
    }
}
