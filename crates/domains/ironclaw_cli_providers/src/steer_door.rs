//! The input side of one Codex or Muse turn: everything that goes to the
//! agent besides gate replies' own wire answers passes one door, in one
//! order, and every steer has one ledger entry until it settles.
//!
//! **Admission.** A runtime steer first takes the door's admission lock and
//! holds it while it declines pending gates, waits for those replies to be
//! on the wire, and knocks; a second steer waits behind it, so no steer
//! overtakes another one's declines. Knocking puts the steer on the one
//! channel the turn's loop reads:
//! - **Opening** (host spawning, session opening, `turn/start` unanswered):
//!   accepted at once (held), never a separate later turn;
//! - **Open**: the requester waits for the loop's verdict;
//! - **Closed**: refused; the engine queues it.
//!
//! **Sending.** The loop folds what the agent already sent, cuts the reply
//! there, and submits `turn/steer` under the door's lock, which Stop also
//! takes to queue its interrupt: no steer is queued after an interrupt.
//!
//! **Settling.** Each steer moves `queued → written → acknowledged`, or ends
//! `refused` / `unconfirmed`, on evidence only:
//! - *delivered*: the frame was written and the agent answered it (and,
//!   where the protocol says so, for this turn);
//! - *refused*: an error answer, a wrong turn, or the frame never written:
//!   not delivered, so a held steer (or one whose requester was already
//!   told `Steered`) goes back through [`report_unread`] and the engine
//!   requeues it; a waiting requester hears `Rejected` and queues it;
//! - *unconfirmed*: written, and the turn ended (or the agent went away)
//!   without an answer: kept in the run with a "Steer delivery not
//!   confirmed" notice, never requeued (it may have been read).
//!
//! A requester is told `Steered` early only once its frame is written and
//! the answer is late; the entry stays tracked, so a refusal after that
//! still brings the steer back unread. Entries reach the transcript in
//! admission order: the reply each one cut, then its row.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::runtime::{
    HistoryMessage, HistoryRole, SteerId, SteerOutcome, SteerReadState, TurnEventSink, TurnOutcome,
    UnreadSteer,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::journal;
use crate::rpc::{PendingReply, RpcError, Written};

/// One accepted steer on its way to the turn's loop.
pub(crate) struct SteerRequest {
    pub(crate) steer: UnreadSteer,
    /// Who waits for the verdict; `None` once told (or for a steer accepted
    /// while the turn was starting, which was answered `Steered` at once).
    waiter: Option<oneshot::Sender<SteerOutcome>>,
    /// What the requester was told, once told.
    told: Option<SteerOutcome>,
    /// A person's message, journaled as a row; `false` for scaffolding (a
    /// rejection's feedback), which only reaches the agent.
    journal: bool,
}

impl SteerRequest {
    fn tell(&mut self, outcome: SteerOutcome) {
        if let Some(waiter) = self.waiter.take() {
            self.told = Some(outcome);
            if waiter.send(outcome).is_err() {
                tracing::debug!("steer: the requester stopped waiting");
            }
        }
    }

    /// Not delivered: a waiting requester hears `Rejected` and queues it
    /// itself; one already told `Rejected` has; a held steer, or one whose
    /// requester was told `Steered`, comes back unread for the engine.
    ///
    /// After Stop it is consumed instead: never queued again. A waiting
    /// requester hears `Steered`, and the steer goes to the engine, which
    /// shows a stopped turn's unread steers as "Steer delivery not
    /// confirmed" notices (one already told `Rejected` was queued before
    /// the Stop and stays so).
    fn refused(mut self, stopped: bool) -> Option<UnreadSteer> {
        if stopped {
            if self.waiter.is_some() {
                self.tell(SteerOutcome::Steered);
            }
            return (self.told != Some(SteerOutcome::Rejected)).then_some(self.steer);
        }
        if self.waiter.is_some() {
            self.tell(SteerOutcome::Rejected);
            return None;
        }
        (self.told != Some(SteerOutcome::Rejected)).then_some(self.steer)
    }
}

enum State<L> {
    Opening,
    Open(L),
    Closed,
}

struct Inner<L> {
    state: State<L>,
    /// Stop reached the turn: nothing more is submitted.
    stopping: bool,
    tx: Option<mpsc::UnboundedSender<SteerRequest>>,
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
    inner: Mutex<Inner<L>>,
    /// Held by a runtime steer from its gate declines to its knock.
    admission: tokio::sync::Mutex<()>,
}

impl<L: Clone> SteerDoor<L> {
    /// A turn's door and the receiver its loop reads steers from.
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<SteerRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let door = Self {
            inner: Mutex::new(Inner {
                state: State::Opening,
                stopping: false,
                tx: Some(tx),
            }),
            admission: tokio::sync::Mutex::new(()),
        };
        (door, rx)
    }

    /// A door that never opens: for work that takes no steers (an idle
    /// compaction), so a steer is refused and queued, never held.
    pub(crate) fn closed() -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: State::Closed,
                stopping: false,
                tx: None,
            }),
            admission: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<L>> {
        // A poisoned lock only follows a panic elsewhere; the state is intact.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One steer at a time, from its gate declines to its knock.
    pub(crate) async fn admit(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.admission.lock().await
    }

    /// Accept `text` for the turn, decided under the door's lock: a close
    /// either sees it in the channel (and reports it) or came first.
    /// `id` is the engine's id for the steer ([`SteerId`]): its row in
    /// the transcript and every report about it carry that id.
    pub(crate) fn knock(&self, id: &str, text: &str) -> Knock {
        self.knock_as(id, text, true)
    }

    /// [`Self::knock`] for a rejection's feedback: delivered the same way,
    /// in the same order, but never journaled as the person's message.
    pub(crate) fn knock_feedback(&self, text: &str) -> Knock {
        self.knock_as(&uuid::Uuid::new_v4().to_string(), text, false)
    }

    fn knock_as(&self, id: &str, text: &str, journal: bool) -> Knock {
        let mut inner = self.lock();
        if inner.stopping {
            return Knock::Closed;
        }
        let steer = UnreadSteer {
            id: id.to_string(),
            text: text.to_string(),
        };
        let (waiter, knock) = match &inner.state {
            State::Closed => return Knock::Closed,
            State::Opening => (None, Knock::Held),
            State::Open(_) => {
                let (tx, rx) = oneshot::channel();
                (Some(tx), Knock::Sent(rx))
            }
        };
        let Some(tx) = inner.tx.as_ref() else {
            return Knock::Closed;
        };
        if tx
            .send(SteerRequest {
                steer,
                waiter,
                told: None,
                journal,
            })
            .is_err()
        {
            inner.state = State::Closed;
            return Knock::Closed;
        }
        knock
    }

    /// Queue the turn's first frame (`turn/start`), unless Stop already
    /// reached the turn: under the lock Stop takes, so the prompt is either
    /// queued before Stop or never.
    pub(crate) fn start<R>(&self, send: impl FnOnce() -> R) -> Option<R> {
        let inner = self.lock();
        (!inner.stopping).then(send)
    }

    /// The turn has an id: Stop can interrupt it, steers wait for verdicts.
    /// `false` when Stop came while the start was in flight: it found no
    /// turn to interrupt, so the caller interrupts it now.
    pub(crate) fn open(&self, live: L) -> bool {
        let mut inner = self.lock();
        if matches!(inner.state, State::Opening) {
            inner.state = State::Open(live);
        }
        !inner.stopping
    }

    /// Stop reached the turn.
    pub(crate) fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// The live turn, once the door is open.
    pub(crate) fn live(&self) -> Option<L> {
        match &self.lock().state {
            State::Open(live) => Some(live.clone()),
            State::Opening | State::Closed => None,
        }
    }

    /// Change the live turn in place (no-op unless open).
    pub(crate) fn update(&self, change: impl FnOnce(&mut L)) {
        if let State::Open(live) = &mut self.lock().state {
            change(live);
        }
    }

    /// Submit a steer's frame (`send` queues it on the wire), unless Stop
    /// has reached the turn. Under the lock Stop takes to queue its
    /// interrupt: a steer is either queued before the interrupt or not at
    /// all.
    pub(crate) fn submit<R>(&self, send: impl FnOnce(&L) -> R) -> Option<R> {
        let inner = self.lock();
        match &inner.state {
            State::Open(live) if !inner.stopping => Some(send(live)),
            _ => None,
        }
    }

    /// Stop: from now on nothing is submitted; `send` queues the interrupt
    /// (only once the turn has an id). Under the same lock as [`Self::submit`].
    pub(crate) fn interrupt<R>(&self, send: impl FnOnce(&L) -> R) -> Option<R> {
        let mut inner = self.lock();
        inner.stopping = true;
        match &inner.state {
            State::Open(live) => Some(send(live)),
            State::Opening | State::Closed => None,
        }
    }

    /// The turn ended: nothing more joins the channel.
    pub(crate) fn close(&self) {
        let mut inner = self.lock();
        inner.state = State::Closed;
        inner.tx = None;
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

/// Checks the agent's answer to a steer (Muse: the same turn).
pub(crate) type Accept = Box<dyn Fn(&Value) -> bool + Send + Sync>;

/// Once the turn has ended, how long a steer still in flight may take to be
/// answered: an agent that has not answered by its turn's end rarely will,
/// and the turn's end waits on it.
const FINISH_GRACE: Duration = Duration::from_secs(2);
/// How often a requester's verdict looks again at a write still pending.
const WRITE_POLL: Duration = Duration::from_millis(50);

/// One steer's ledger entry, from submission to settlement.
struct Entry {
    request: SteerRequest,
    /// The reply the steer cut short, journaled ahead of its row.
    segment: Vec<HistoryMessage>,
    /// The agent's answer, until it came.
    reply: Option<Reply>,
    written: Written,
    /// When the requester is told, if the answer is late.
    verdict_due: Instant,
    /// Settled (answer in, or never written).
    outcome: Option<Delivery>,
    /// The agent went away without answering; the write's report decides.
    gone: bool,
}

/// What settling steers produced, for the loop to act on in order.
#[derive(Default)]
pub(crate) struct Settled {
    /// Transcript entries, in order, under the run.
    pub(crate) entries: Vec<HistoryMessage>,
    /// Steers that never reached the agent and whose requester was already
    /// told `Steered` (held, or told on a late answer).
    pub(crate) unread: Vec<UnreadSteer>,
    /// Written with no word either way: the engine keeps them as "Steer
    /// delivery not confirmed" notices and never sends them again.
    pub(crate) unconfirmed: Vec<UnreadSteer>,
}

impl Settled {
    pub(crate) fn extend(&mut self, other: Settled) {
        self.entries.extend(other.entries);
        self.unread.extend(other.unread);
        self.unconfirmed.extend(other.unconfirmed);
    }
}

/// What the loop's wait on the pipe produced.
pub(crate) enum PipeEvent {
    /// The oldest entry's answer.
    Answer(Result<Value, RpcError>),
    /// A requester's verdict is due.
    VerdictDue,
}

/// The turn loop's side: the ledger of sent steers, settled in order.
pub(crate) struct SteerPipe {
    entries: VecDeque<Entry>,
    /// Stop reached the turn: what is not delivered is consumed.
    stopped: bool,
    run_id: String,
    accept: Accept,
    /// How long a requester waits before a late answer is reported.
    verdict_after: Duration,
}

impl SteerPipe {
    pub(crate) fn new(run_id: &str, verdict_after: Duration, accept: Accept) -> Self {
        Self {
            entries: VecDeque::new(),
            stopped: false,
            run_id: run_id.to_string(),
            accept,
            verdict_after,
        }
    }

    /// Stop reached the turn (see [`SteerRequest::refused`]).
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
    }

    /// Steers sent and not settled yet.
    pub(crate) fn busy(&self) -> bool {
        !self.entries.is_empty()
    }

    /// The next thing to act on (cancel-safe: futures stay with their
    /// entries). Only call while [`Self::busy`].
    pub(crate) async fn event(&mut self) -> PipeEvent {
        let now = Instant::now();
        let due = self
            .entries
            .iter()
            .filter_map(|e| {
                if e.gone && e.outcome.is_none() {
                    // Waiting on the write's report.
                    Some(now + WRITE_POLL)
                } else {
                    e.request.waiter.is_some().then_some(e.verdict_due)
                }
            })
            .min();
        let head = self.entries.front_mut().and_then(|e| e.reply.as_mut());
        tokio::select! {
            answer = async {
                match head {
                    Some(reply) => reply.as_mut().await,
                    None => std::future::pending().await,
                }
            } => PipeEvent::Answer(answer),
            () = async {
                match due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending().await,
                }
            } => PipeEvent::VerdictDue,
        }
    }

    /// Act on [`Self::event`]; returns what settled (in order).
    pub(crate) fn on_event(&mut self, event: PipeEvent) -> Settled {
        match event {
            PipeEvent::Answer(answer) => {
                if let Some(head) = self.entries.front_mut() {
                    head.reply = None;
                    match classify(&self.accept, answer, &head.written) {
                        Some(delivery) => head.outcome = Some(delivery),
                        // Gone without an answer, the write still reporting:
                        // decided once it has (see below).
                        None => head.gone = true,
                    }
                }
            }
            PipeEvent::VerdictDue => {
                let now = Instant::now();
                for entry in self.entries.iter_mut() {
                    if entry.gone && entry.outcome.is_none() {
                        entry.outcome = match entry.written.now() {
                            Some(true) => Some(Delivery::Unconfirmed),
                            Some(false) => Some(Delivery::Refused),
                            None => None,
                        };
                    }
                    if entry.request.waiter.is_none() || entry.verdict_due > now {
                        continue;
                    }
                    match entry.written.now() {
                        // On the wire, answer late: accepted into the turn,
                        // still tracked (a refusal later brings it back).
                        Some(true) => entry.request.tell(SteerOutcome::Steered),
                        Some(false) => {
                            entry.reply = None;
                            entry.outcome = Some(Delivery::Refused);
                            entry.request.tell(SteerOutcome::Rejected);
                        }
                        // The write itself is pending (bounded by the
                        // writer): look again shortly.
                        None => entry.verdict_due = now + WRITE_POLL,
                    }
                }
            }
        }
        self.settle_ready()
    }

    /// `request` was submitted (`Some`) or not (`None`: Stop won, or the
    /// frame could not be queued), cutting the reply before it into
    /// `segment`.
    pub(crate) fn sent(
        &mut self,
        request: SteerRequest,
        segment: Vec<HistoryMessage>,
        started: Option<Result<(PendingReply, Written), RpcError>>,
    ) -> Settled {
        let (reply, written, outcome): (Option<Reply>, Written, Option<Delivery>) = match started {
            // The answer is awaited for as long as the turn runs.
            Some(Ok((pending, written))) => (
                Some(Box::pin(pending.wait(Duration::from_secs(24 * 3600)))),
                written,
                None,
            ),
            Some(Err(_)) | None => (None, Written::failed(), Some(Delivery::Refused)),
        };
        self.entries.push_back(Entry {
            request,
            segment,
            reply,
            written,
            verdict_due: Instant::now() + self.verdict_after,
            outcome,
            gone: false,
        });
        self.settle_ready()
    }

    /// The turn ended: close the door, settle what is in flight (in order),
    /// and refuse what was never sent. `stopped`: Stop ended it, so no
    /// answer is waited for.
    pub(crate) async fn finish<L: Clone>(
        &mut self,
        door: &SteerDoor<L>,
        rx: &mut mpsc::UnboundedReceiver<SteerRequest>,
        stopped: bool,
    ) -> Settled {
        door.close();
        let mut settled = self.settle_ready();
        let grace = Instant::now()
            + if stopped {
                Duration::ZERO
            } else {
                FINISH_GRACE
            };
        while let Some(head) = self.entries.front_mut() {
            if head.outcome.is_none() {
                let answer = match head.reply.as_mut() {
                    Some(reply) => match tokio::time::timeout_at(grace, reply.as_mut()).await {
                        Ok(answer) => answer,
                        Err(_) => Err(RpcError::Timeout),
                    },
                    None => Err(RpcError::Timeout),
                };
                head.reply = None;
                // Without an answer, the write's own report decides; it is
                // waited for (the writer bounds every write), so nothing is
                // settled while its frame may still land. A report that
                // still has not come is no evidence either way.
                let written = tokio::time::timeout(
                    crate::rpc::WRITE_TIMEOUT + Duration::from_secs(1),
                    head.written.outcome(),
                )
                .await;
                head.outcome = Some(match answer {
                    Ok(_) | Err(RpcError::Remote { .. }) => {
                        classify(&self.accept, answer, &head.written)
                            .unwrap_or(Delivery::Unconfirmed)
                    }
                    Err(_) => match written {
                        Ok(false) => Delivery::Refused,
                        Ok(true) | Err(_) => Delivery::Unconfirmed,
                    },
                });
            }
            settled.extend(self.settle_ready());
        }
        rx.close();
        while let Ok(request) = rx.try_recv() {
            settled.extend(self.refuse(request));
        }
        settled
    }

    /// A steer the loop will not send (Stop won, or the turn is over).
    pub(crate) fn refuse(&self, request: SteerRequest) -> Settled {
        Settled {
            unread: request.refused(self.stopped).into_iter().collect(),
            ..Settled::default()
        }
    }

    /// Settle entries from the front while their outcome is known.
    fn settle_ready(&mut self) -> Settled {
        let mut settled = Settled::default();
        while self.entries.front().is_some_and(|e| e.outcome.is_some()) {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            settled.extend(self.settle(entry));
        }
        settled
    }

    fn settle(&self, mut entry: Entry) -> Settled {
        let mut settled = Settled::default();
        let segment = std::mem::take(&mut entry.segment);
        settled
            .entries
            .extend(segment.into_iter().map(|e| self.in_run(e)));
        let text = entry.request.steer.text.clone();
        let journal = entry.request.journal;
        let steer_id = entry.request.steer.id.clone();
        match entry.outcome.unwrap_or(Delivery::Refused) {
            Delivery::Delivered => {
                if journal {
                    settled.entries.push(self.steer_row(text, steer_id));
                }
                entry.request.tell(SteerOutcome::Steered);
            }
            Delivery::Unconfirmed => {
                if journal {
                    settled.entries.push(self.steer_row(text.clone(), steer_id));
                }
                entry.request.tell(SteerOutcome::Steered);
                if self.stopped {
                    // The engine shows a stopped turn's unread steers as
                    // the notice: one notice, never requeued.
                    settled.unread.push(entry.request.steer);
                } else if journal {
                    // It may have been read: never queued again. The engine
                    // shows the notice (its ledger knows this steer).
                    settled.unconfirmed.push(entry.request.steer);
                } else {
                    // A rejection's feedback the engine never saw: shown
                    // here, since they are the person's words too.
                    settled
                        .entries
                        .push(self.row(HistoryRole::System, not_confirmed(&text)));
                }
            }
            Delivery::Refused => settled.unread.extend(entry.request.refused(self.stopped)),
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

    /// A steer's own row, marked with its id so the timeline marks it.
    fn steer_row(&self, text: String, steer_id: String) -> HistoryMessage {
        HistoryMessage {
            steer_id: Some(steer_id),
            ..self.row(HistoryRole::User, text)
        }
    }
}

/// A steer's answer, judged on evidence: an answer means it was written.
/// `None`: the agent went away without answering while the write is still
/// reporting; only its report can decide (written: maybe read, so
/// unconfirmed; not written: refused).
fn classify(
    accept: &Accept,
    answer: Result<Value, RpcError>,
    written: &Written,
) -> Option<Delivery> {
    match answer {
        Ok(value) if accept(&value) => Some(Delivery::Delivered),
        // An answer for another turn, or an error: not folded into this one.
        Ok(_) | Err(RpcError::Remote { .. }) => Some(Delivery::Refused),
        Err(RpcError::Timeout | RpcError::Closed) => match written.now() {
            Some(true) => Some(Delivery::Unconfirmed),
            Some(false) => Some(Delivery::Refused),
            None => None,
        },
    }
}

/// T3 Code's notice for a steer nobody can say was read.
fn not_confirmed(text: &str) -> String {
    format!(
        "Steer delivery not confirmed: the turn ended before the agent acknowledged this \
         message, so it may not have been delivered. Resend it if needed: \"{text}\""
    )
}

/// Report how a turn's steers settled, before the turn is reported
/// finished (so no queue drain runs between the two): unconfirmed ones as
/// [`SteerReadState::Unconfirmed`] (a notice, never resent), unread ones
/// through [`report_unread`].
pub(crate) async fn report_settlement(
    sink: &Weak<dyn TurnEventSink>,
    thread: &ThreadId,
    run: &str,
    unread: Vec<UnreadSteer>,
    unconfirmed: Vec<UnreadSteer>,
    outcome: TurnOutcome,
) -> TurnOutcome {
    if let Some(sink) = sink.upgrade() {
        for steer in &unconfirmed {
            match SteerId::new(steer.id.clone()) {
                Ok(id) => {
                    sink.steer_updated(thread, run, &id, SteerReadState::Unconfirmed, now_ms())
                        .await;
                }
                Err(error) => tracing::debug!(%error, "steer id not reportable"),
            }
        }
    }
    report_unread(sink, thread, run, unread, outcome).await
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

    fn accept_all() -> Accept {
        Box::new(|_| true)
    }

    fn written(ok: Option<bool>) -> (tokio::sync::watch::Sender<Option<bool>>, Written) {
        let (tx, rx) = tokio::sync::watch::channel(ok);
        (tx, Written(rx))
    }

    fn entry(
        text: &str,
        waiter: Option<oneshot::Sender<SteerOutcome>>,
        reply: Reply,
        w: Written,
    ) -> Entry {
        Entry {
            request: SteerRequest {
                steer: UnreadSteer {
                    id: text.into(),
                    text: text.into(),
                },
                waiter,
                told: None,
                journal: true,
            },
            segment: Vec::new(),
            reply: Some(reply),
            written: w,
            verdict_due: Instant::now() + Duration::from_millis(20),
            outcome: None,
            gone: false,
        }
    }

    #[tokio::test]
    async fn stop_reaching_the_turn_refuses_every_later_submission() {
        let (door, _rx) = SteerDoor::<u8>::new();
        door.open(7);
        assert_eq!(door.submit(|l| *l), Some(7));
        assert_eq!(door.interrupt(|l| *l), Some(7));
        assert_eq!(door.submit(|l| *l), None, "no steer after the interrupt");
        assert!(matches!(door.knock("l", "late"), Knock::Closed));
    }

    /// Sol 3: a late answer never makes a pending write count as success:
    /// the requester hears only once the frame is written, and a write
    /// that fails refuses it.
    #[tokio::test]
    async fn a_verdict_waits_for_the_write_and_a_failed_write_refuses() {
        let mut pipe = SteerPipe::new("run", Duration::from_millis(20), accept_all());
        let (tx, w) = written(None);
        let (waiter, verdict) = oneshot::channel();
        pipe.entries.push_back(entry(
            "s",
            Some(waiter),
            Box::pin(std::future::pending()),
            w,
        ));
        // Due, but the write is pending: no verdict yet.
        let event = pipe.event().await;
        assert!(pipe.on_event(event).entries.is_empty());
        assert!(pipe.entries[0].request.waiter.is_some());
        tx.send_replace(Some(false));
        tokio::time::sleep(WRITE_POLL * 2).await;
        let event = pipe.event().await;
        let settled = pipe.on_event(event);
        assert!(settled.entries.is_empty() && settled.unread.is_empty());
        assert_eq!(verdict.await.unwrap(), SteerOutcome::Rejected);
    }

    /// Sol 3: told `Steered` on a late answer, then refused: it comes back
    /// unread (the engine requeues it), never lost.
    #[tokio::test]
    async fn a_refusal_after_a_late_verdict_brings_the_steer_back() {
        let mut pipe = SteerPipe::new("run", Duration::from_millis(20), accept_all());
        let (_tx, w) = written(Some(true));
        let (answer_tx, answer_rx) = oneshot::channel::<Result<Value, RpcError>>();
        let reply: Reply =
            Box::pin(async move { answer_rx.await.unwrap_or(Err(RpcError::Closed)) });
        let (waiter, verdict) = oneshot::channel();
        pipe.entries.push_back(entry("s", Some(waiter), reply, w));
        let event = pipe.event().await;
        assert!(matches!(event, PipeEvent::VerdictDue));
        pipe.on_event(event);
        assert_eq!(verdict.await.unwrap(), SteerOutcome::Steered);
        answer_tx
            .send(Err(RpcError::Remote {
                code: 1,
                message: "no".into(),
                data: None,
            }))
            .unwrap();
        let event = pipe.event().await;
        let settled = pipe.on_event(event);
        assert_eq!(settled.unread.len(), 1);
        assert!(settled.entries.is_empty());
    }

    #[tokio::test]
    async fn an_unanswered_written_steer_after_the_turn_ended_is_unconfirmed() {
        let (door, mut rx) = SteerDoor::<u8>::new();
        let mut pipe = SteerPipe::new("run", Duration::from_secs(60), accept_all());
        let (_tx, w) = written(Some(true));
        let mut e = entry("maybe", None, Box::pin(std::future::pending()), w);
        e.segment = vec![journal::message(
            HistoryRole::Assistant,
            "before".into(),
            Vec::new(),
        )];
        pipe.entries.push_back(e);
        let settled = pipe.finish(&door, &mut rx, true).await;
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
        assert_eq!(rows.len(), 2, "the notice is the engine's");
        assert_eq!(settled.entries[1].steer_id.as_deref(), Some("maybe"));
        let unconfirmed: Vec<&str> = settled.unconfirmed.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(unconfirmed, ["maybe"]);
    }

    /// Sol 3: Muse answers with a turn id; another turn's is a refusal.
    #[tokio::test]
    async fn an_answer_for_another_turn_is_refused() {
        let (door, mut rx) = SteerDoor::<u8>::new();
        let accept: Accept = Box::new(|v| v.get("turnId").and_then(Value::as_str) == Some("t1"));
        let mut pipe = SteerPipe::new("run", Duration::from_secs(60), accept);
        let (_tx, w) = written(Some(true));
        let reply: Reply = Box::pin(std::future::ready(Ok(serde_json::json!({"turnId": "t2"}))));
        pipe.entries.push_back(entry("s", None, reply, w));
        let event = pipe.event().await;
        let mut settled = pipe.on_event(event);
        settled.extend(pipe.finish(&door, &mut rx, true).await);
        assert_eq!(settled.unread.len(), 1);
    }

    /// Sol 3: the agent got the frame and exited before the writer
    /// reported it. The reader's `Closed` comes first, while the write is
    /// still reporting: the entry waits for the report instead of settling
    /// as refused (a replay), and a write that did land is unconfirmed.
    #[tokio::test]
    async fn closed_before_the_write_reports_waits_for_the_report() {
        let mut pipe = SteerPipe::new("run", Duration::from_secs(60), accept_all());
        let (write, w) = written(None);
        pipe.entries.push_back(entry(
            "landed",
            None,
            Box::pin(std::future::ready(Err(RpcError::Closed))),
            w,
        ));
        let event = pipe.event().await;
        let settled = pipe.on_event(event);
        assert!(
            settled.unread.is_empty() && settled.entries.is_empty(),
            "settled early"
        );
        write.send_replace(Some(true));
        let event = pipe.event().await;
        let settled = pipe.on_event(event);
        assert!(
            settled.unread.is_empty(),
            "replayed though it may have been read"
        );
        assert_eq!(settled.unconfirmed.len(), 1, "the engine's notice");
    }

    /// Rule 2: after Stop nothing outstanding is requeued as such; a waiting
    /// requester hears `Steered` and the steer goes to the engine (which
    /// shows it as "Steer delivery not confirmed"), written or not.
    #[tokio::test]
    async fn stop_consumes_every_outstanding_steer() {
        let (door, mut rx) = SteerDoor::<u8>::new();
        door.open(1);
        let Knock::Sent(verdict) = door.knock("u", "unsent") else {
            panic!("open");
        };
        let mut pipe = SteerPipe::new("run", Duration::from_secs(60), accept_all());
        let (_w, w) = written(Some(true));
        pipe.entries
            .push_back(entry("written", None, Box::pin(std::future::pending()), w));
        pipe.stop();
        let settled = pipe.finish(&door, &mut rx, true).await;
        let unread: Vec<&str> = settled.unread.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(unread, ["written", "unsent"]);
        assert!(
            !settled
                .entries
                .iter()
                .any(|e| e.text.contains("not confirmed")),
            "one notice, the engine's"
        );
        assert_eq!(verdict.await.unwrap(), SteerOutcome::Steered);
    }

    /// Rule 2 / Sol 4: Stop before the start: the first frame is never queued.
    #[test]
    fn stop_before_the_start_sends_no_prompt() {
        let (door, _rx) = SteerDoor::<u8>::new();
        assert_eq!(door.interrupt(|l| *l), None, "no turn yet");
        assert_eq!(door.start(|| 1), None);
        assert!(!door.open(5), "the caller interrupts the late turn");
    }
}
