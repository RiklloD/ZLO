//! The talk-mode message waiting for its reply.
//!
//! Talk mode speaks the reply to the message *it* sent, never another run
//! that happens to end meanwhile. A send gets a [`Submission`] token when
//! talk mode presses Enter; the composer that handles that Enter takes the
//! token (`voice::take_submission`) and carries it through the send, so
//! nothing else can claim it: not a thread another send creates, not a run
//! that was already going. The gateway's answer then decides:
//!
//! - accepted: the turn waits for the run the ack names (or, without a run
//!   id, the next run to end in that thread);
//! - queued behind a running turn: that running turn's end is skipped;
//! - refused (busy, error, never sent): talk mode is told so.
//!
//! A run may finish before its ack arrives, so ends seen while the send is
//! in flight are kept until it is answered. Each send has its own token, so
//! an answer deferred before the turn changed is dropped.

/// Talk mode's claim on one send, carried by the composer that sends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Submission(pub u64);

/// A reply to speak for the turn `token`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Answer {
    pub token: u64,
    pub spoken: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    /// Enter pressed; the gateway has not answered yet.
    Submitted,
    Bound {
        thread: String,
        /// The run the ack named; `None` takes the next run to end.
        run: Option<String>,
        /// A run to ignore (the one a queued message waits behind).
        skip: Option<String>,
    },
}

/// A run that ended while the send was in flight.
#[derive(Clone, Debug)]
struct Ended {
    thread: String,
    run: Option<String>,
    spoken: String,
}

/// Ends kept while a send is in flight.
const EARLY_ENDS: usize = 8;

#[derive(Default)]
pub(crate) struct PendingTurn {
    token: u64,
    state: State,
    early: Vec<Ended>,
}

impl PendingTurn {
    /// Talk mode pressed Enter: a fresh claim, nothing bound yet.
    pub fn submit(&mut self) -> Submission {
        self.token += 1;
        self.state = State::Submitted;
        self.early.clear();
        Submission(self.token)
    }

    /// Whether `sub` is the send talk mode waits on (not yet answered).
    pub fn waits_for(&self, sub: Submission) -> bool {
        self.live(sub)
    }

    fn live(&self, sub: Submission) -> bool {
        sub.0 == self.token && self.state == State::Submitted
    }

    /// The gateway accepted `sub` in `thread`, as `run` when the ack names
    /// it. The answer when that run already ended.
    pub fn accepted(
        &mut self,
        sub: Submission,
        thread: &str,
        run: Option<String>,
    ) -> Option<Answer> {
        self.bind(sub, thread, run, None)
    }

    /// `sub` waits in `thread`'s queue behind `behind` (the running turn),
    /// whose end is not the reply.
    pub fn queued(
        &mut self,
        sub: Submission,
        thread: &str,
        behind: Option<String>,
    ) -> Option<Answer> {
        self.bind(sub, thread, None, behind)
    }

    fn bind(
        &mut self,
        sub: Submission,
        thread: &str,
        run: Option<String>,
        skip: Option<String>,
    ) -> Option<Answer> {
        if !self.live(sub) {
            return None;
        }
        self.state = State::Bound {
            thread: thread.to_string(),
            run,
            skip,
        };
        let early = std::mem::take(&mut self.early);
        early
            .into_iter()
            .find(|e| self.matches(&e.thread, e.run.as_deref()))
            .map(|e| Answer {
                token: self.token,
                spoken: e.spoken,
            })
    }

    /// `sub` was not sent (refused, failed, or nobody took it). True when it
    /// was the turn waiting, so talk mode should say so.
    pub fn refused(&mut self, sub: Submission) -> bool {
        if !self.live(sub) {
            return false;
        }
        self.clear();
        true
    }

    /// A run ended in `thread`: the answer when it is this turn's reply.
    /// While the send is in flight the end is kept for its ack.
    pub fn run_ended(&mut self, thread: &str, run: Option<&str>, spoken: String) -> Option<Answer> {
        match self.state {
            State::Idle => None,
            State::Submitted => {
                if self.early.len() == EARLY_ENDS {
                    self.early.remove(0);
                }
                self.early.push(Ended {
                    thread: thread.to_string(),
                    run: run.map(str::to_string),
                    spoken,
                });
                None
            }
            State::Bound { .. } => self.matches(thread, run).then_some(Answer {
                token: self.token,
                spoken,
            }),
        }
    }

    fn matches(&self, thread: &str, ended: Option<&str>) -> bool {
        let State::Bound {
            thread: bound,
            run,
            skip,
        } = &self.state
        else {
            return false;
        };
        if bound != thread {
            return false;
        }
        match (run, ended) {
            (Some(run), ended) => ended == Some(run.as_str()),
            (None, Some(ended)) => skip.as_deref() != Some(ended),
            // An end without a run id cannot be told from the skipped one.
            (None, None) => skip.is_none(),
        }
    }

    /// Whether the waiting turn is still `token` and bound to a send.
    pub fn current(&self, token: u64) -> bool {
        self.token == token && matches!(self.state, State::Bound { .. })
    }

    /// Answered, or talk mode ended: nothing waits any more.
    pub fn clear(&mut self) {
        self.token += 1;
        self.state = State::Idle;
        self.early.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn only_the_run_the_ack_names_answers() {
        let mut t = PendingTurn::default();
        let sub = t.submit();
        assert_eq!(t.accepted(sub, "a", Some("r2".into())), None);
        // Another thread, and another run in the same thread, are not it.
        assert_eq!(t.run_ended("b", Some("r2"), said("no")), None);
        assert_eq!(t.run_ended("a", Some("r1"), said("no")), None);
        let answer = t.run_ended("a", Some("r2"), said("yes")).expect("its run");
        assert_eq!(answer.spoken, "yes");
        assert!(t.current(answer.token));
    }

    #[test]
    fn a_queued_message_skips_the_turn_it_waits_behind() {
        let mut t = PendingTurn::default();
        let sub = t.submit();
        assert_eq!(t.queued(sub, "a", Some("running".into())), None);
        assert_eq!(
            t.run_ended("a", Some("running"), said("old turn")),
            None,
            "the turn already going is not the reply"
        );
        let answer = t
            .run_ended("a", Some("next"), said("reply"))
            .expect("next run");
        assert_eq!(answer.spoken, "reply");
    }

    #[test]
    fn a_refused_send_answers_nothing() {
        let mut t = PendingTurn::default();
        let sub = t.submit();
        assert!(t.refused(sub));
        assert_eq!(t.run_ended("a", Some("r1"), said("x")), None);
        // A late ack for the refused send binds nothing.
        assert_eq!(t.accepted(sub, "a", Some("r1".into())), None);
        assert_eq!(t.run_ended("a", Some("r1"), said("x")), None);
        assert!(!t.refused(sub), "refused once");
    }

    #[test]
    fn ends_before_the_ack_are_kept_for_it() {
        let mut t = PendingTurn::default();
        let sub = t.submit();
        assert_eq!(t.run_ended("other", Some("x"), said("not it")), None);
        assert_eq!(t.run_ended("a", Some("r9"), said("fast reply")), None);
        let answer = t
            .accepted(sub, "a", Some("r9".into()))
            .expect("the run already ended");
        assert_eq!(answer.spoken, "fast reply");
    }

    #[test]
    fn an_older_claim_cannot_bind_a_newer_turn() {
        let mut t = PendingTurn::default();
        let old = t.submit();
        t.clear();
        let new = t.submit();
        // The old send's ack (e.g. a draft whose creation was slow) arrives.
        assert_eq!(t.accepted(old, "slow", Some("r1".into())), None);
        assert_eq!(t.run_ended("slow", Some("r1"), said("x")), None);
        assert_eq!(t.accepted(new, "mine", Some("r2".into())), None);
        assert!(t.run_ended("mine", Some("r2"), said("y")).is_some());
    }

    #[test]
    fn without_a_run_id_the_next_end_in_the_thread_answers() {
        let mut t = PendingTurn::default();
        let sub = t.submit();
        t.accepted(sub, "a", None);
        assert_eq!(t.run_ended("b", Some("r"), said("x")), None);
        assert!(t.run_ended("a", Some("r"), said("y")).is_some());
    }

    #[test]
    fn a_deferred_answer_from_an_older_turn_is_stale() {
        let mut t = PendingTurn::default();
        let sub = t.submit();
        t.accepted(sub, "a", Some("r1".into()));
        let old = t.run_ended("a", Some("r1"), said("x")).expect("answer");
        t.clear();
        assert!(!t.current(old.token));
        let sub = t.submit();
        t.accepted(sub, "a", Some("r1".into()));
        assert!(!t.current(old.token), "the new turn has its own token");
    }

    #[test]
    fn nothing_answers_when_no_turn_waits() {
        let mut t = PendingTurn::default();
        assert_eq!(t.run_ended("a", Some("r"), said("x")), None);
    }
}
