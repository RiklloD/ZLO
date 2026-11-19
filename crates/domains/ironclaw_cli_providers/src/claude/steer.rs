//! Which of a turn's commands (the prompt and every steer written into its
//! stdin) the CLI has read, finished or dropped, from the CLI's own
//! `command_lifecycle` frames and the `user_message_uuids` echoes on its
//! output (checked against 2.1.328: `queued`, `started`, `completed`; T3
//! Code also handles `cancelled`, `discarded`, `refused`).
//!
//! Two guarantees follow from it:
//! - stdin stays open while any command is still open, so the approval hook
//!   can still answer the tool calls of a steer Claude starts after the first
//!   result (closing at the first result left those calls unanswerable);
//! - a steer the CLI never started is reported as unread when the turn ends
//!   (dropped, Stop, crash) and the engine queues it as a later turn.
//!
//! A CLI that reports neither lifecycle nor echoes keeps the old behaviour:
//! stdin closes at the first result and every steer counts as read.
//!
//! A steer that arrives before the prompt is written (the CLI has not
//! acknowledged the hook registration yet) is held here and written right
//! after the prompt, in order (T3 Code's "steer when ready"); one the turn
//! never got to write is unread whatever the CLI reports.
//!
//! After the result, open commands keep stdin open only while the CLI shows
//! life: an unacknowledged steer is bounded by [`ACK_TIMEOUT`] from the
//! result, and an acknowledged one by [`IDLE_TIMEOUT`] of silence with no
//! tool call or hook answer in flight. Then input closes and every steer it
//! never started is reported unread.

use std::collections::HashSet;
use std::time::Duration;

use ironclaw_orchestrator::runtime::UnreadSteer;
use serde_json::Value;

/// After the turn's result, how long a written steer may go unacknowledged
/// (no lifecycle frame at all) before stdin closes anyway. The CLI queues a
/// steer within milliseconds of reading it; this only bounds a CLI that
/// ignores one, which would otherwise hold the turn open forever.
pub(super) const ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// After the turn's result, how long the CLI may stay silent (no output,
/// no tool call running, no hook waiting on a person) while commands it
/// acknowledged are still open. Claude answers a queued steer as its next
/// native turn and streams while it does; this only bounds a CLI that
/// acknowledged a steer and then never worked on it, which would otherwise
/// keep the turn on "Working" forever.
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// A steer accepted before the prompt was written: written right after it.
#[derive(Debug, Clone)]
pub(super) struct HeldSteer {
    pub uuid: String,
    pub text: String,
    /// The stream-json frame, ready to write.
    pub line: String,
}

/// Where one steer stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Accepted before the prompt was written; not on the wire yet.
    Held,
    /// On the wire; the CLI has not started it.
    Written,
    /// The CLI started it (`started` lifecycle or a uuid echo).
    Read,
}

#[derive(Debug, Clone)]
struct Steer {
    held: HeldSteer,
    stage: Stage,
}

#[derive(Debug, Default)]
pub(super) struct SteerLedger {
    /// Commands written to stdin that have not completed or been dropped.
    open: HashSet<String>,
    /// Commands the CLI has acknowledged (any lifecycle state or echo).
    acked: HashSet<String>,
    /// Every steer of the turn in the order it was accepted, held and
    /// written alike, so a recovery keeps the person's order.
    steers: Vec<Steer>,
    /// The CLI reports which command it is on.
    tracked: bool,
    /// A result has arrived.
    settled: bool,
    /// Input was closed on open steers: stop waiting for them.
    abandoned: bool,
}

impl SteerLedger {
    /// The turn's prompt, written once the hook is registered.
    pub(super) fn prompt(&mut self, uuid: &str) {
        self.open.insert(uuid.to_string());
    }

    /// A steer that came before the prompt: kept until the prompt is out.
    pub(super) fn hold(&mut self, steer: HeldSteer) {
        self.steers.push(Steer {
            held: steer,
            stage: Stage::Held,
        });
    }

    /// The held steers, oldest first, to write now that the prompt is out.
    /// They stay in place (and in order) until [`Self::sent`] marks them.
    pub(super) fn held(&self) -> Vec<HeldSteer> {
        self.steers
            .iter()
            .filter(|s| s.stage == Stage::Held)
            .map(|s| s.held.clone())
            .collect()
    }

    /// A steer about to be written: a held one moves on in place, a new one
    /// joins the end. Undone by [`Self::unsent`] when the write fails.
    pub(super) fn sent(&mut self, steer: &HeldSteer) {
        self.open.insert(steer.uuid.clone());
        match self.steers.iter_mut().find(|s| s.held.uuid == steer.uuid) {
            Some(known) => known.stage = Stage::Written,
            None => self.steers.push(Steer {
                held: steer.clone(),
                stage: Stage::Written,
            }),
        }
    }

    /// Its write failed: a held steer goes back to held (still in order,
    /// still unread); a direct one is forgotten (its caller queues it).
    pub(super) fn unsent(&mut self, uuid: &str, was_held: bool) {
        self.open.remove(uuid);
        if was_held {
            if let Some(known) = self.steers.iter_mut().find(|s| s.held.uuid == uuid) {
                known.stage = Stage::Held;
            }
        } else {
            self.steers.retain(|s| s.held.uuid != uuid);
        }
    }

    /// Fold one output frame in.
    pub(super) fn observe(&mut self, frame: &Value) {
        if let Some(uuids) = frame.get("user_message_uuids").and_then(Value::as_array) {
            self.tracked = true;
            for uuid in uuids.iter().filter_map(Value::as_str) {
                self.read(uuid);
            }
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("command_lifecycle") => {
                let Some(uuid) = frame.get("command_uuid").and_then(Value::as_str) else {
                    return;
                };
                self.tracked = true;
                self.acked.insert(uuid.to_string());
                match frame.get("state").and_then(Value::as_str) {
                    Some("started") => self.read(uuid),
                    // Not proof it was read: Claude also completes commands
                    // it never started (T3 Code checks the same).
                    Some("completed") => {
                        self.open.remove(uuid);
                    }
                    // Removed before it was read: it stays unread.
                    Some("cancelled" | "discarded" | "refused") => {
                        self.open.remove(uuid);
                    }
                    _ => {}
                }
            }
            Some("result") => {
                self.settled = true;
                // The result settles the commands it answers.
                if let Some(uuids) = frame.get("user_message_uuids").and_then(Value::as_array) {
                    for uuid in uuids.iter().filter_map(Value::as_str) {
                        self.open.remove(uuid);
                    }
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, uuid: &str) {
        self.acked.insert(uuid.to_string());
        if let Some(known) = self
            .steers
            .iter_mut()
            .find(|s| s.held.uuid == uuid && s.stage == Stage::Written)
        {
            known.stage = Stage::Read;
        }
    }

    /// Whether stdin may close now: a result is in and nothing is open (or
    /// the CLI does not say, as before).
    pub(super) fn may_close(&self) -> bool {
        self.settled && (!self.tracked || self.open.is_empty())
    }

    /// A result is in and only steers the CLI never acknowledged keep stdin
    /// open: the read loop bounds that wait with [`ACK_TIMEOUT`].
    pub(super) fn waiting_for_ack(&self) -> bool {
        !self.abandoned
            && self.settled
            && self.tracked
            && !self.open.is_empty()
            && self.open.iter().all(|u| !self.acked.contains(u))
    }

    /// A result is in and commands the CLI acknowledged are still open: the
    /// read loop bounds that wait by [`IDLE_TIMEOUT`] of silence.
    pub(super) fn held_open(&self) -> bool {
        !self.abandoned && self.settled && self.tracked && !self.open.is_empty()
    }

    /// Input closed without them; they stay unread.
    pub(super) fn abandon(&mut self) {
        self.abandoned = true;
    }

    /// Steers the CLI never started, in the order they were accepted: the
    /// ones never written (held for a prompt that did not go out) and the
    /// written ones the CLI did not start. Written ones are left out for a
    /// CLI that does not report lifecycle: there is no telling, and
    /// resending a steer it did read would deliver it twice.
    pub(super) fn unread(&self) -> Vec<UnreadSteer> {
        self.steers
            .iter()
            .filter(|s| match s.stage {
                Stage::Held => true,
                Stage::Written => self.tracked,
                Stage::Read => false,
            })
            .map(|s| UnreadSteer {
                id: s.held.uuid.clone(),
                text: s.held.text.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(l: &SteerLedger) -> Vec<String> {
        l.unread().into_iter().map(|s| s.text).collect()
    }

    fn life(uuid: &str, state: &str) -> Value {
        json!({"type": "command_lifecycle", "command_uuid": uuid, "state": state})
    }

    fn steer(uuid: &str, text: &str) -> HeldSteer {
        HeldSteer {
            uuid: uuid.into(),
            text: text.into(),
            line: "{}".into(),
        }
    }

    fn result(uuid: &str) -> Value {
        json!({"type": "result", "subtype": "success", "user_message_uuids": [uuid]})
    }

    /// The 2.1.328 sequence for a steer sent mid-reply: the first result
    /// arrives while the steer is queued, so stdin must stay open for it.
    #[test]
    fn a_queued_steer_keeps_stdin_open_until_it_completes() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "queued"));
        l.observe(&life("a", "started"));
        l.sent(&steer("b", "stop counting"));
        l.observe(&life("b", "queued"));
        l.observe(&result("a"));
        assert!(!l.may_close(), "steer b is still open");
        assert!(!l.waiting_for_ack(), "b was acknowledged");
        l.observe(&life("a", "completed"));
        l.observe(&life("b", "started"));
        assert!(texts(&l).is_empty(), "b was read");
        l.observe(&result("b"));
        assert!(l.may_close());
    }

    #[test]
    fn a_dropped_steer_is_unread_and_frees_stdin() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "started"));
        l.sent(&steer("b", "also fix the docs"));
        l.observe(&life("b", "queued"));
        l.observe(&result("a"));
        l.observe(&life("a", "completed"));
        l.observe(&life("b", "discarded"));
        assert!(l.may_close());
        assert_eq!(texts(&l), vec!["also fix the docs".to_string()]);
    }

    /// Stop or a crash before the CLI reached the steer.
    #[test]
    fn a_steer_never_started_is_unread_at_the_end() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "started"));
        l.sent(&steer("b", "first"));
        l.sent(&steer("c", "second"));
        assert_eq!(texts(&l), vec!["first".to_string(), "second".to_string()]);
        l.unsent("c", false);
        assert_eq!(texts(&l), vec!["first".to_string()]);
    }

    #[test]
    fn an_unacknowledged_steer_after_the_result_is_bounded() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "started"));
        l.observe(&result("a"));
        l.observe(&life("a", "completed"));
        l.sent(&steer("b", "late"));
        assert!(!l.may_close());
        assert!(l.waiting_for_ack());
        l.abandon();
        assert!(!l.waiting_for_ack(), "given up once");
        assert_eq!(texts(&l), vec!["late".to_string()]);
    }

    /// The CLI acknowledged a steer and went quiet after the result: the
    /// read loop's idle bound applies, and the steer stays unread.
    #[test]
    fn an_acknowledged_steer_held_open_after_the_result_is_idle_bounded() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "started"));
        l.sent(&steer("b", "queued but never started"));
        l.observe(&life("b", "queued"));
        l.observe(&result("a"));
        l.observe(&life("a", "completed"));
        assert!(!l.may_close());
        assert!(!l.waiting_for_ack(), "b was acknowledged");
        assert!(l.held_open());
        l.abandon();
        assert!(!l.held_open(), "given up once");
        assert_eq!(texts(&l), vec!["queued but never started".to_string()]);
    }

    /// A steer held for a prompt that never went out is unread, even from
    /// a CLI that reports no lifecycle.
    #[test]
    fn a_held_steer_never_written_is_unread() {
        let mut l = SteerLedger::default();
        l.hold(steer("h", "early"));
        assert_eq!(texts(&l), vec!["early".to_string()]);
        let held = l.held();
        assert_eq!(held.len(), 1);
        l.sent(&held[0]);
        assert!(l.held().is_empty());
        // Written without lifecycle: no telling, so not unread.
        assert!(texts(&l).is_empty());
    }

    /// Review (Sol, 8): A written and unread, B's write failed back to held:
    /// recovery keeps the order they were accepted in.
    #[test]
    fn a_partial_flush_keeps_the_admission_order() {
        let mut l = SteerLedger::default();
        l.hold(steer("a", "first"));
        l.hold(steer("b", "second"));
        l.prompt("p");
        l.observe(&life("p", "started"));
        let held = l.held();
        l.sent(&held[0]);
        l.sent(&held[1]);
        l.unsent("b", true);
        assert_eq!(l.held().len(), 1, "b is held again");
        assert_eq!(texts(&l), vec!["first".to_string(), "second".to_string()]);
    }

    /// Review (Sol, 3): Claude completes commands it never started; only
    /// `started` or an echo proves a steer was read.
    #[test]
    fn completed_alone_is_not_read() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "started"));
        l.sent(&steer("b", "never started"));
        l.observe(&life("b", "queued"));
        l.observe(&result("a"));
        l.observe(&life("a", "completed"));
        l.observe(&life("b", "completed"));
        assert!(l.may_close());
        assert_eq!(texts(&l), vec!["never started".to_string()]);
    }

    /// An older CLI without lifecycle or echoes: close at the result, as
    /// before, and never resend a steer it may have read.
    #[test]
    fn without_lifecycle_the_first_result_closes_and_nothing_is_unread() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.sent(&steer("b", "x"));
        l.observe(&json!({"type": "result", "subtype": "success"}));
        assert!(l.may_close());
        assert!(texts(&l).is_empty());
    }
}
