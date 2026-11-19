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

/// How long a requester waits for its taken steer's frame to be written.
const WRITE_ACK_TIMEOUT: Duration = Duration::from_secs(30);

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

/// A held steer the turn queued: it counts as sent once written, and is
/// reported unread if the write fails.
pub(super) type HeldWrite = (UnreadSteer, Written);

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

    /// Tell the requester whether the steer was queued for the wire, handing
    /// it the write's acknowledgement (`None`: refused). A held steer comes
    /// back: refused, it is unread at once; queued, the turn checks its write.
    pub(super) fn answer(self, written: Option<Written>) -> Answered {
        match self.waiter {
            Some(waiter) => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "the requester waits until answered once the loop took it; gone only with its task"
                )]
                let _ = waiter.answer.send(written);
                Answered::Waited
            }
            None => {
                let steer = UnreadSteer {
                    id: self.id,
                    text: self.text,
                };
                match written {
                    Some(written) => Answered::HeldWrite((steer, written)),
                    None => Answered::Unread(steer),
                }
            }
        }
    }
}

/// What answering a steer left for the turn to track.
pub(super) enum Answered {
    /// Its requester has the verdict.
    Waited,
    /// A held steer that will never go out: report it unread.
    Unread(UnreadSteer),
    /// A held steer queued for the wire: unread if the write fails.
    HeldWrite(HeldWrite),
}

impl Answered {
    /// File it: unread steers and held writes into the turn's lists.
    pub(super) fn file(self, unread: &mut Vec<UnreadSteer>, held: &mut Vec<HeldWrite>) {
        match self {
            Self::Waited => {}
            Self::Unread(steer) => unread.push(steer),
            Self::HeldWrite(write) => held.push(write),
        }
    }
}

pub(super) type SteerTx = mpsc::UnboundedSender<SteerRequest>;

/// Offer a steer to a running turn and wait for it to go out. The wait for
/// the loop to take it is bounded by [`STEER_ACK_TIMEOUT`]; a request the
/// loop never took is withdrawn first, so it can no longer be sent. Once
/// taken, the loop answers without any I/O of its own (the frame is queued
/// in wire order), and this waits for the frame to be written to the
/// agent's stdin: a closed stdin refuses the steer. A write still pending
/// after [`WRITE_ACK_TIMEOUT`] counts as sent: it is queued ahead of every
/// later frame and the pipe is alive, only slow.
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
    let Some(written) = queued else {
        return false;
    };
    match tokio::time::timeout(WRITE_ACK_TIMEOUT, written).await {
        Ok(written) => written.unwrap_or(false), // silent-ok: a dropped ack means the writer is gone, so not written
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let request = loop {
            if let Ok(request) = rx.try_recv() {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(request.take());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (tx, written) = oneshot::channel();
        assert!(matches!(request.answer(Some(written)), Answered::Waited));
        tx.send(true).unwrap();
        assert!(offer.await.unwrap(), "sent, so not queued as well");
    }

    /// Review (Sol, 5): queued is not sent. A frame the agent's closed
    /// stdin never took refuses the steer, so the engine queues it.
    #[tokio::test]
    async fn a_steer_whose_frame_is_never_written_is_refused() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let offer = tokio::spawn(async move {
            offer_steer_within(&tx, "lost".into(), true, Duration::from_millis(500)).await
        });
        let request = loop {
            if let Ok(request) = rx.try_recv() {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(request.take());
        let (written_tx, written) = oneshot::channel();
        assert!(matches!(request.answer(Some(written)), Answered::Waited));
        written_tx.send(false).unwrap();
        assert!(!offer.await.unwrap());
    }

    /// A steer held before the session opened that the turn could not send
    /// comes back for the engine to queue.
    #[test]
    fn a_held_steer_not_sent_is_unread() {
        let request = SteerRequest {
            id: "h".into(),
            text: "early".into(),
            journal: true,
            waiter: None,
        };
        assert!(request.take());
        let Answered::Unread(unread) = request.answer(None) else {
            panic!("unread");
        };
        assert_eq!(unread.text, "early");
    }
}
