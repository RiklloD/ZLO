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
    /// The frame's write acknowledgement once it is queued; `None` when
    /// the turn refused the steer.
    answer: oneshot::Sender<Option<Written>>,
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

    /// Tell the requester whether the steer was queued for the wire,
    /// handing it the write's report (`None`: refused, never written), and
    /// enter the steer in the turn's ledger.
    pub(super) fn answer(self, written: Option<Written>, ledger: &mut SteerLedger) -> usize {
        let waited = self.waiter.is_some();
        if let Some(waiter) = self.waiter {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the requester waits until answered once the loop took it; gone only with its task"
            )]
            let _ = waiter.answer.send(written.clone());
        }
        ledger.enter(
            UnreadSteer {
                id: self.id,
                text: self.text,
            },
            waited,
            written,
        )
    }
}

/// Every steer of one turn the loop took (and those still in its channel at
/// the end), in the order they came, from queued to settled:
/// - never written (refused, Stop won, no steering, the write failed): not
///   delivered;
/// - written: delivered (ACP has no separate acknowledgement for a prompt),
///   unless the agent then refused that prompt with an error answer.
///
/// Who hears about one that was not delivered depends on what its
/// requester was told: a waiting requester hears `Rejected` when its write
/// fails (and queues the text itself); everything else not delivered (held
/// steers, and refusals after the requester heard `Steered`) is reported
/// unread at the end, in admission order.
#[derive(Default)]
pub(super) struct SteerLedger {
    entries: Vec<LedgerEntry>,
}

struct LedgerEntry {
    steer: UnreadSteer,
    /// A requester waited and hears the write's outcome itself.
    waited: bool,
    /// `None`: never written.
    written: Option<Written>,
    /// The agent answered this steer's prompt with an error.
    refused: Arc<Mutex<bool>>,
}

impl SteerLedger {
    /// Enter a steer; returns its index (for [`Self::refusal`]).
    pub(super) fn enter(
        &mut self,
        steer: UnreadSteer,
        waited: bool,
        written: Option<Written>,
    ) -> usize {
        self.entries.push(LedgerEntry {
            steer,
            waited,
            written,
            refused: Arc::default(),
        });
        self.entries.len() - 1
    }

    /// Where entry `at`'s prompt answer reports a refusal.
    pub(super) fn refusal(&self, at: usize) -> Option<Arc<Mutex<bool>>> {
        self.entries.get(at).map(|e| Arc::clone(&e.refused))
    }

    /// The steers that never reached the agent and that nobody else will
    /// queue, in the order they came. A write still pending at the end is
    /// queued ahead of every later frame and counts as written.
    pub(super) fn unread(self) -> Vec<UnreadSteer> {
        self.entries
            .into_iter()
            .filter(|e| {
                let written = e.written.as_ref().map(|w| w.now().unwrap_or(true));
                let refused = *e
                    .refused
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match written {
                    // Never written: a waiting requester already heard so.
                    None | Some(false) => !e.waited,
                    // Written, then refused by the agent: nobody heard.
                    Some(true) => refused,
                }
            })
            .map(|e| e.steer)
            .collect()
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
pub(super) async fn offer_steer(steer: &SteerTx, text: String, journal: bool) -> bool {
    offer_steer_within(steer, text, journal, STEER_ACK_TIMEOUT).await
}

pub(super) async fn offer_steer_within(
    steer: &SteerTx,
    text: String,
    journal: bool,
    wait: Duration,
) -> bool {
    let ticket = Arc::new(Mutex::new(Ticket::Waiting));
    let (answer, mut sent) = oneshot::channel::<Option<Written>>();
    let request = SteerRequest {
        id: uuid::Uuid::new_v4().to_string(),
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
    let queued = match tokio::time::timeout(wait, &mut sent).await {
        Ok(answer) => answer.ok().flatten(), // silent-ok: a dropped answer means the turn ended without sending it
        Err(_) => match settle_ticket(&ticket, Ticket::Withdrawn) {
            Ok(()) => None,
            // The loop took it just now: its answer is on the way.
            Err(_) => sent.await.ok().flatten(), // silent-ok: as above
        },
    };
    let Some(mut written) = queued else {
        return false;
    };
    written.outcome().await
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
        let sent = offer_steer_within(&tx, "late".into(), true, Duration::from_millis(50)).await;
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
            offer_steer_within(&tx, "on time".into(), true, Duration::from_millis(50)).await
        });
        let request = taken(&mut rx).await;
        assert!(request.take());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (write, report) = written(None);
        let mut ledger = SteerLedger::default();
        request.answer(Some(report), &mut ledger);
        write.send_replace(Some(true));
        assert!(offer.await.unwrap(), "sent, so not queued as well");
        assert!(ledger.unread().is_empty());
    }

    /// Sol 4: a write still pending is never success: the requester waits
    /// for it (the writer bounds it), and one that fails refuses the steer.
    #[tokio::test]
    async fn a_pending_write_is_waited_for_and_a_failed_one_refuses() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let offer = tokio::spawn(async move {
            offer_steer_within(&tx, "lost".into(), true, Duration::from_millis(500)).await
        });
        let request = taken(&mut rx).await;
        assert!(request.take());
        let (write, report) = written(None);
        let mut ledger = SteerLedger::default();
        request.answer(Some(report), &mut ledger);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!offer.is_finished(), "answered while the write was pending");
        write.send_replace(Some(false));
        assert!(!offer.await.unwrap());
        assert!(
            ledger.unread().is_empty(),
            "its requester queues it, not the ledger"
        );
    }

    /// Sol 4: held steers come back in the order they came, whether their
    /// write failed or the agent refused their prompt later.
    #[test]
    fn held_steers_come_back_in_admission_order() {
        let mut ledger = SteerLedger::default();
        let held = |id: &str| SteerRequest {
            id: id.into(),
            text: id.into(),
            journal: true,
            waiter: None,
        };
        let (_a, failed) = written(Some(false));
        held("first").answer(Some(failed), &mut ledger);
        let (_b, ok) = written(Some(true));
        let at = held("second").answer(Some(ok), &mut ledger);
        let (_c, ok) = written(Some(true));
        held("third").answer(Some(ok), &mut ledger);
        held("fourth").answer(None, &mut ledger);
        // The agent refuses the second one's prompt after it was written.
        *ledger.refusal(at).unwrap().lock().unwrap() = true;
        let texts: Vec<String> = ledger.unread().into_iter().map(|s| s.text).collect();
        assert_eq!(texts, ["first", "second", "fourth"]);
    }
}
