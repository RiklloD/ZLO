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
    answer: oneshot::Sender<bool>,
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

    /// Tell the requester whether the steer went out. A held steer that did
    /// not is returned, for the turn to report unread.
    pub(super) fn answer(self, sent: bool) -> Option<UnreadSteer> {
        match self.waiter {
            Some(waiter) => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "the requester waits until answered once the loop took it; gone only with its task"
                )]
                let _ = waiter.answer.send(sent);
                None
            }
            None => (!sent).then_some(UnreadSteer {
                id: self.id,
                text: self.text,
            }),
        }
    }
}

pub(super) type SteerTx = mpsc::UnboundedSender<SteerRequest>;

/// Offer a steer to a running turn and wait for it to go out. The wait for
/// the loop to take it is bounded by [`STEER_ACK_TIMEOUT`]; once taken, the
/// answer follows as soon as the frame is on the wire. A request the loop
/// never took is withdrawn first, so it can no longer be sent.
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
    let (answer, mut sent) = oneshot::channel();
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
    match tokio::time::timeout(wait, &mut sent).await {
        Ok(answer) => answer.unwrap_or(false), // silent-ok: a dropped answer means the turn ended without sending it
        Err(_) => match settle_ticket(&ticket, Ticket::Withdrawn) {
            Ok(()) => false,
            // The loop took it just now: its answer is on the way.
            Err(_) => sent.await.unwrap_or(false), // silent-ok: as above
        },
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
        assert!(request.answer(true).is_none());
        assert!(offer.await.unwrap(), "sent, so not queued as well");
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
        let unread = request.answer(false).expect("unread");
        assert_eq!(unread.text, "early");
    }
}
