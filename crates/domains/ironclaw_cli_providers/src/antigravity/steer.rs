//! How a steer reaches a running Antigravity turn: a request on the turn's
//! steer channel, either held (accepted before the ACP session opened, so
//! the turn sends it once its prompt is out or reports it unread) or waited
//! on (session open: the requester waits until it is on the wire).
//!
//! A waiting requester and the drive loop agree once, through a [`Ticket`],
//! on who acts: the loop takes the request before sending it, or the
//! requester withdraws it when its wait runs out. A steer is therefore
//! never sent after its requester reported it refused (the engine queued
//! it), nor lost between the two.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironclaw_orchestrator::runtime::UnreadSteer;
use tokio::sync::{mpsc, oneshot};

use super::STEER_ACK_TIMEOUT;
use crate::rpc::Written;

/// A message for the running turn.
pub(super) struct SteerRequest {
    /// Reported back under this id if it is never delivered.
    pub(super) id: String,
    pub(super) text: String,
    /// A person's message (journaled), not scaffolding such as a rejection.
    pub(super) journal: bool,
    /// Who waits to hear whether it was sent. `None` for a steer accepted
    /// before the session opened: already answered `Steered`, so the turn
    /// either sends it or reports it unread (the engine then queues it).
    pub(super) waiter: Option<SteerWaiter>,
}

/// Where a waiting requester and the drive loop agree, once, on who acts on
/// a steer: the loop takes it before sending it, or the requester withdraws
/// it when its wait runs out. Never both, so a steer is never sent after
/// its requester reported it refused (and queued it), nor lost between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ticket {
    Waiting,
    Taken,
    Withdrawn,
}

pub(super) struct SteerWaiter {
    ticket: Arc<Mutex<Ticket>>,
    /// What the turn did with it.
    answer: oneshot::Sender<Taken>,
}

/// What the drive loop did with a steer it took.
#[derive(Clone)]
pub(super) enum Taken {
    /// Queued for the wire; the write's report says whether it went out.
    Queued(Written),
    /// Never written (no steering, the frame could not be queued, or the
    /// turn ended first): the requester queues it.
    Refused,
    /// Stop won before it was written: consumed, never queued again (the
    /// engine shows it as "Steer delivery not confirmed").
    Consumed,
}

/// Flip `ticket` from `Waiting` to `to`; the state it had otherwise.
fn settle_ticket(ticket: &Mutex<Ticket>, to: Ticket) -> Result<(), Ticket> {
    // A poisoned ticket only follows a panic elsewhere; its state is intact.
    let mut state = ticket
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *state {
        Ticket::Waiting => {
            *state = to;
            Ok(())
        }
        other => Err(other),
    }
}

impl SteerRequest {
    /// The drive loop's claim before acting: `false` when the requester
    /// already gave up (it reported the steer refused): skip it.
    pub(super) fn take(&self) -> bool {
        self.waiter
            .as_ref()
            .is_none_or(|w| settle_ticket(&w.ticket, Ticket::Taken).is_ok())
    }

    /// Tell the requester what became of the steer and enter it in the
    /// turn's ledger. Returns its index (for [`SteerLedger::reply`]).
    pub(super) fn answer(self, taken: Taken, ledger: &mut SteerLedger) -> usize {
        let waited = self.waiter.is_some();
        if let Some(waiter) = self.waiter {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the requester waits until answered once the loop took it; gone only with its task"
            )]
            let _ = waiter.answer.send(taken.clone());
        }
        ledger.enter(
            UnreadSteer {
                id: self.id,
                text: self.text,
            },
            waited,
            taken,
            self.journal,
        )
    }
}

/// What the agent answered to one steer's `session/prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Reply {
    /// No answer yet (or none ever: the agent went away).
    #[default]
    Pending,
    /// Answered: the agent took the prompt.
    Answered,
    /// An error answer: the agent refused it.
    Refused,
    /// The connection ended without an answer.
    Gone,
}

/// How a turn's steers settled, in the order they came.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Settlement {
    /// Not delivered; the engine requeues them (after Stop: shows them as
    /// "Steer delivery not confirmed" instead).
    pub(super) unread: Vec<UnreadSteer>,
    /// Written with no evidence either way: shown with the notice, never
    /// requeued (the engine's notice for steers it knows).
    pub(super) unconfirmed: Vec<UnreadSteer>,
    /// The same for a rejection's feedback, which the engine never saw: the
    /// turn shows its notice itself.
    pub(super) unconfirmed_feedback: Vec<UnreadSteer>,
}

/// Every steer of one turn the loop took (and those still in its channel at
/// the end), in the order they came, each settled from evidence only:
/// - never written: unread (its waiting requester heard `Rejected` and
///   queued it itself);
/// - written and answered, or a later prompt answered (the agent takes
///   prompts in order): read;
/// - written and refused by an error answer: unread;
/// - written with no answer either way (the agent went away, the turn
///   ended first, the write's report never came): unconfirmed.
///
/// After Stop, nothing not read is requeued: everything outstanding goes to
/// the engine as unread, which shows a stopped turn's steers as notices.
#[derive(Default)]
pub(super) struct SteerLedger {
    entries: Vec<LedgerEntry>,
}

struct LedgerEntry {
    steer: UnreadSteer,
    /// A requester waited and heard the outcome of a refusal itself.
    waited: bool,
    taken: Taken,
    reply: Arc<Mutex<Reply>>,
    /// A person's steer (the engine knows it), not a rejection's feedback.
    journal: bool,
}

impl SteerLedger {
    /// Enter a steer; returns its index (for [`Self::reply`]).
    pub(super) fn enter(
        &mut self,
        steer: UnreadSteer,
        waited: bool,
        taken: Taken,
        journal: bool,
    ) -> usize {
        self.entries.push(LedgerEntry {
            steer,
            waited,
            taken,
            reply: Arc::default(),
            journal,
        });
        self.entries.len() - 1
    }

    /// Where entry `at`'s prompt answer is noted.
    pub(super) fn reply(&self, at: usize) -> Option<Arc<Mutex<Reply>>> {
        self.entries.get(at).map(|e| Arc::clone(&e.reply))
    }

    /// Settle every steer, once each write's report is in (the writer
    /// bounds every write; a report that still has not come is no evidence).
    pub(super) async fn settle(self, stopped: bool) -> Settlement {
        let mut written = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            written.push(match &entry.taken {
                Taken::Queued(report) => {
                    let mut report = report.clone();
                    tokio::time::timeout(
                        crate::rpc::WRITE_TIMEOUT + Duration::from_secs(1),
                        report.outcome(),
                    )
                    .await
                    .ok() // silent-ok: no report in time is no evidence either way
                }
                Taken::Refused | Taken::Consumed => Some(false),
            });
        }
        let replies: Vec<Reply> = self
            .entries
            .iter()
            .map(|e| {
                *e.reply
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect();
        let mut settlement = Settlement::default();
        for (at, entry) in self.entries.into_iter().enumerate() {
            let later_answered = replies[at + 1..].contains(&Reply::Answered);
            let read = replies[at] == Reply::Answered || later_answered;
            match written[at] {
                // Never written: nothing could read it.
                Some(false) => {
                    // A waiting requester heard the refusal (and queued it)
                    // unless Stop consumed it.
                    if !entry.waited || matches!(entry.taken, Taken::Consumed) {
                        settlement.unread.push(entry.steer);
                    }
                }
                // Refused by the agent: not delivered (after Stop the engine
                // shows it instead of requeueing).
                _ if replies[at] == Reply::Refused => settlement.unread.push(entry.steer),
                _ if read => {}
                // Written, or its report never came: no evidence either way.
                Some(true) | None => {
                    if stopped {
                        settlement.unread.push(entry.steer);
                    } else if entry.journal {
                        settlement.unconfirmed.push(entry.steer);
                    } else {
                        settlement.unconfirmed_feedback.push(entry.steer);
                    }
                }
            }
        }
        settlement
    }
}

pub(super) type SteerTx = mpsc::UnboundedSender<SteerRequest>;

/// Offer a steer to a running turn and wait for it to go out. The wait for
/// the loop to take it is bounded by [`STEER_ACK_TIMEOUT`]; a request the
/// loop never took is withdrawn first, so it can no longer be sent. Once
/// taken, the loop answers without any I/O of its own (the frame is queued
/// in wire order), and this waits for the frame's actual write: a closed or
/// stalled stdin (the writer's own bound) refuses the steer. Never success
/// on a write that is still pending.
/// `id` is the engine's id for the steer (its transcript row and every
/// report carry it).
pub(super) async fn offer_steer(steer: &SteerTx, id: &str, text: String, journal: bool) -> bool {
    offer_steer_within(steer, id, text, journal, STEER_ACK_TIMEOUT).await
}

pub(super) async fn offer_steer_within(
    steer: &SteerTx,
    id: &str,
    text: String,
    journal: bool,
    wait: Duration,
) -> bool {
    let ticket = Arc::new(Mutex::new(Ticket::Waiting));
    let (answer, mut sent) = oneshot::channel::<Taken>();
    let request = SteerRequest {
        id: id.to_string(),
        text,
        journal,
        waiter: Some(SteerWaiter {
            ticket: Arc::clone(&ticket),
            answer,
        }),
    };
    if steer.send(request).is_err() {
        return false;
    }
    let taken = match tokio::time::timeout(wait, &mut sent).await {
        Ok(answer) => answer.unwrap_or(Taken::Refused), // silent-ok: a dropped answer means the turn ended without sending it
        Err(_) => match settle_ticket(&ticket, Ticket::Withdrawn) {
            Ok(()) => Taken::Refused,
            // The loop took it just now: its answer is on the way.
            Err(_) => sent.await.unwrap_or(Taken::Refused), // silent-ok: as above
        },
    };
    match taken {
        Taken::Queued(mut written) => written.outcome().await,
        Taken::Refused => false,
        // Stop won: consumed, so not queued again (see `Taken`).
        Taken::Consumed => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(ok: Option<bool>) -> (tokio::sync::watch::Sender<Option<bool>>, Written) {
        let (tx, rx) = tokio::sync::watch::channel(ok);
        (tx, Written::from_watch(rx))
    }

    async fn taken(rx: &mut mpsc::UnboundedReceiver<SteerRequest>) -> SteerRequest {
        loop {
            if let Ok(request) = rx.try_recv() {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A requester whose wait ran out withdraws its steer: the drive loop,
    /// reaching it later, must not send it (the engine already queued it).
    #[tokio::test]
    async fn a_steer_withdrawn_after_the_wait_is_never_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sent =
            offer_steer_within(&tx, "s1", "late".into(), true, Duration::from_millis(50)).await;
        assert!(!sent, "reported refused");
        let request = rx.try_recv().expect("still in the channel");
        assert!(!request.take(), "withdrawn: the loop skips it");
    }

    /// The loop took the steer just before the wait ran out: the requester
    /// waits for its answer instead of reporting it refused (no duplicate).
    #[tokio::test]
    async fn a_steer_taken_before_the_wait_ran_out_is_answered() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let offer = tokio::spawn(async move {
            offer_steer_within(&tx, "s2", "on time".into(), true, Duration::from_millis(50)).await
        });
        let request = taken(&mut rx).await;
        assert!(request.take());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (write, report) = written(None);
        let mut ledger = SteerLedger::default();
        request.answer(Taken::Queued(report), &mut ledger);
        write.send_replace(Some(true));
        assert!(offer.await.unwrap(), "sent, so not queued as well");
        let settlement = ledger.settle(false).await;
        assert!(settlement.unread.is_empty(), "written: never requeued");
    }

    /// Sol 4: a write still pending is never success: the requester waits
    /// for it (the writer bounds it), and one that fails refuses the steer.
    #[tokio::test]
    async fn a_pending_write_is_waited_for_and_a_failed_one_refuses() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let offer = tokio::spawn(async move {
            offer_steer_within(&tx, "s3", "lost".into(), true, Duration::from_millis(500)).await
        });
        let request = taken(&mut rx).await;
        assert!(request.take());
        let (write, report) = written(None);
        let mut ledger = SteerLedger::default();
        request.answer(Taken::Queued(report), &mut ledger);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!offer.is_finished(), "answered while the write was pending");
        write.send_replace(Some(false));
        assert!(!offer.await.unwrap());
        assert!(
            ledger.settle(false).await.unread.is_empty(),
            "its requester queues it, not the ledger"
        );
    }

    fn held(id: &str) -> SteerRequest {
        SteerRequest {
            id: id.into(),
            text: id.into(),
            journal: true,
            waiter: None,
        }
    }

    fn texts(steers: &[UnreadSteer]) -> Vec<&str> {
        steers.iter().map(|s| s.text.as_str()).collect()
    }

    /// Held steers settle in the order they came, each from evidence: a
    /// failed write and a refusal come back unread, an answered prompt (or
    /// one a later answered prompt follows) is read, and a written prompt
    /// with no answer either way is unconfirmed (never requeued).
    #[tokio::test]
    async fn held_steers_settle_from_evidence_in_admission_order() {
        let mut ledger = SteerLedger::default();
        let (_a, failed) = written(Some(false));
        held("failed").answer(Taken::Queued(failed), &mut ledger);
        let (_b, ok) = written(Some(true));
        let refused = held("refused").answer(Taken::Queued(ok), &mut ledger);
        let (_c, ok) = written(Some(true));
        let read = held("read").answer(Taken::Queued(ok), &mut ledger);
        let (_d, ok) = written(Some(true));
        held("silent").answer(Taken::Queued(ok), &mut ledger);
        held("never").answer(Taken::Refused, &mut ledger);
        *ledger.reply(refused).unwrap().lock().unwrap() = Reply::Refused;
        *ledger.reply(read).unwrap().lock().unwrap() = Reply::Answered;
        let settlement = ledger.settle(false).await;
        assert_eq!(texts(&settlement.unread), ["failed", "refused", "never"]);
        assert_eq!(texts(&settlement.unconfirmed), ["silent"]);
    }

    /// Sol 1: a held steer whose write is still pending when the turn ends
    /// is not settled as written: the write's report is waited for (the
    /// writer bounds it), and a write that then fails is unread.
    #[tokio::test]
    async fn a_pending_write_at_the_end_is_waited_for() {
        let mut ledger = SteerLedger::default();
        let (write, report) = written(None);
        held("late").answer(Taken::Queued(report), &mut ledger);
        let settle = tokio::spawn(ledger.settle(false));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!settle.is_finished(), "settled while its write was pending");
        write.send_replace(Some(false));
        assert_eq!(texts(&settle.await.unwrap().unread), ["late"]);
    }

    /// Rule 2: after Stop nothing outstanding is requeued as such: written
    /// or not, it goes to the engine, which shows it as a notice; a steer
    /// consumed by Stop counts too.
    #[tokio::test]
    async fn stop_sends_every_outstanding_steer_to_the_engine() {
        let mut ledger = SteerLedger::default();
        let (_a, ok) = written(Some(true));
        held("written").answer(Taken::Queued(ok), &mut ledger);
        held("consumed").answer(Taken::Consumed, &mut ledger);
        let settlement = ledger.settle(true).await;
        assert_eq!(texts(&settlement.unread), ["written", "consumed"]);
        assert!(settlement.unconfirmed.is_empty());
    }
}
