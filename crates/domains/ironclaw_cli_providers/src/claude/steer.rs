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

#[derive(Debug, Default)]
pub(super) struct SteerLedger {
    /// Commands written to stdin that have not completed or been dropped.
    open: HashSet<String>,
    /// Commands the CLI has acknowledged (any lifecycle state or echo).
    acked: HashSet<String>,
    /// Steers the CLI has not started, oldest first: (uuid, text).
    unread: Vec<(String, String)>,
    /// Steers accepted before the prompt was written, oldest first.
    held: Vec<HeldSteer>,
    /// The CLI reports which command it is on.
    tracked: bool,
    /// A result has arrived.
    settled: bool,
    /// Input was closed on unacknowledged steers: stop waiting for them.
    abandoned: bool,
}

impl SteerLedger {
    /// The turn's prompt, written once the hook is registered.
    pub(super) fn prompt(&mut self, uuid: &str) {
        self.open.insert(uuid.to_string());
    }

    /// A steer about to be written. Undone by [`Self::unsent`] when the
    /// write fails.
    pub(super) fn sent(&mut self, uuid: &str, text: &str) {
        self.open.insert(uuid.to_string());
        self.unread.push((uuid.to_string(), text.to_string()));
    }

    /// A steer that came before the prompt: kept until the prompt is out.
    pub(super) fn hold(&mut self, steer: HeldSteer) {
        self.held.push(steer);
    }

    /// The held steers, oldest first, to write now that the prompt is out.
    pub(super) fn take_held(&mut self) -> Vec<HeldSteer> {
        std::mem::take(&mut self.held)
    }

    pub(super) fn unsent(&mut self, uuid: &str) {
        self.open.remove(uuid);
        self.unread.retain(|(u, _)| u != uuid);
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
                    Some("completed") => {
                        self.read(uuid);
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
        self.unread.retain(|(u, _)| u != uuid);
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

    /// Steers the CLI never started, oldest first: the ones never written
    /// (held for a prompt that did not go out), then the written ones the
    /// CLI did not start. Written ones are left out for a CLI that does not
    /// report lifecycle: there is no telling, and resending a steer it did
    /// read would deliver it twice.
    pub(super) fn unread(&self) -> Vec<UnreadSteer> {
        let held = self.held.iter().map(|h| UnreadSteer {
            id: h.uuid.clone(),
            text: h.text.clone(),
        });
        let written = self
            .unread
            .iter()
            .filter(|_| self.tracked)
            .map(|(id, text)| UnreadSteer {
                id: id.clone(),
                text: text.clone(),
            });
        held.chain(written).collect()
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
        l.sent("b", "stop counting");
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
        l.sent("b", "also fix the docs");
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
        l.sent("b", "first");
        l.sent("c", "second");
        assert_eq!(texts(&l), vec!["first".to_string(), "second".to_string()]);
        l.unsent("c");
        assert_eq!(texts(&l), vec!["first".to_string()]);
    }

    #[test]
    fn an_unacknowledged_steer_after_the_result_is_bounded() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.observe(&life("a", "started"));
        l.observe(&result("a"));
        l.observe(&life("a", "completed"));
        l.sent("b", "late");
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
        l.sent("b", "queued but never started");
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
        l.hold(HeldSteer {
            uuid: "h".into(),
            text: "early".into(),
            line: "{}".into(),
        });
        assert_eq!(texts(&l), vec!["early".to_string()]);
        let held = l.take_held();
        assert_eq!(held.len(), 1);
        assert!(texts(&l).is_empty());
    }

    /// An older CLI without lifecycle or echoes: close at the result, as
    /// before, and never resend a steer it may have read.
    #[test]
    fn without_lifecycle_the_first_result_closes_and_nothing_is_unread() {
        let mut l = SteerLedger::default();
        l.prompt("a");
        l.sent("b", "x");
        l.observe(&json!({"type": "result", "subtype": "success"}));
        assert!(l.may_close());
        assert!(texts(&l).is_empty());
    }
}
