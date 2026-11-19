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

use std::collections::HashSet;
use std::time::Duration;

use ironclaw_orchestrator::runtime::UnreadSteer;
use serde_json::Value;

/// After the turn's result, how long a written steer may go unacknowledged
/// (no lifecycle frame at all) before stdin closes anyway. The CLI queues a
/// steer within milliseconds of reading it; this only bounds a CLI that
/// ignores one, which would otherwise hold the turn open forever.
pub(super) const ACK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Default)]
pub(super) struct SteerLedger {
    /// Commands written to stdin that have not completed or been dropped.
    open: HashSet<String>,
    /// Commands the CLI has acknowledged (any lifecycle state or echo).
    acked: HashSet<String>,
    /// Steers the CLI has not started, oldest first: (uuid, text).
    unread: Vec<(String, String)>,
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

    /// Input closed without them; they stay unread.
    pub(super) fn abandon(&mut self) {
        self.abandoned = true;
    }

    /// Steers the CLI never started, oldest first. Empty for a CLI that
    /// does not report lifecycle: there is no telling, and resending a
    /// steer it did read would deliver it twice.
    pub(super) fn unread(&self) -> Vec<UnreadSteer> {
        if !self.tracked {
            return Vec::new();
        }
        self.unread
            .iter()
            .map(|(id, text)| UnreadSteer {
                id: id.clone(),
                text: text.clone(),
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
