//! The talk-mode turn waiting for its reply.
//!
//! Talk mode speaks the reply to the message *it* sent, never another
//! chat's run that happens to finish meanwhile. A turn sent from a thread's
//! composer waits for that thread; one sent from the new-thread or quick
//! composer waits for the thread that send creates, which claims it with its
//! first message (and becomes where talk mode listens next). Each send gets
//! a fresh token, so an answer deferred before the turn changed is dropped.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Awaiting {
    Thread(String),
    /// Sent from a composer that creates the thread.
    NewThread,
}

#[derive(Default)]
pub(crate) struct PendingTurn {
    token: u64,
    awaiting: Option<Awaiting>,
}

impl PendingTurn {
    /// Talk mode sent a message from `thread`'s composer (`None`: the
    /// new-thread or quick composer).
    pub fn sent(&mut self, thread: Option<String>) {
        self.token += 1;
        self.awaiting = Some(thread.map_or(Awaiting::NewThread, Awaiting::Thread));
    }

    /// `thread` was just created and got its first message. True when it
    /// is the thread the turn was waiting for.
    pub fn claim_new(&mut self, thread: &str) -> bool {
        if self.awaiting != Some(Awaiting::NewThread) {
            return false;
        }
        self.awaiting = Some(Awaiting::Thread(thread.to_string()));
        true
    }

    /// A run in `thread` ended: the token to answer with when it is this
    /// turn's.
    pub fn answers(&self, thread: &str) -> Option<u64> {
        match &self.awaiting {
            Some(Awaiting::Thread(t)) if t == thread => Some(self.token),
            _ => None,
        }
    }

    /// Whether `token` is still the turn waiting (not answered, replaced or
    /// abandoned since).
    pub fn current(&self, token: u64) -> bool {
        self.awaiting.is_some() && self.token == token
    }

    /// The turn was answered or talk mode ended.
    pub fn clear(&mut self) {
        self.token += 1;
        self.awaiting = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_thread_talk_mode_sent_to_answers() {
        let mut t = PendingTurn::default();
        t.sent(Some("a".into()));
        assert_eq!(t.answers("b"), None, "another chat's run is not the reply");
        let token = t.answers("a").expect("its own thread answers");
        assert!(t.current(token));
    }

    #[test]
    fn nothing_answers_when_no_turn_waits() {
        let t = PendingTurn::default();
        assert_eq!(t.answers("a"), None);
    }

    #[test]
    fn a_new_thread_turn_waits_for_the_thread_it_creates() {
        let mut t = PendingTurn::default();
        t.sent(None);
        // A run elsewhere finishes before the thread exists.
        assert_eq!(t.answers("other"), None);
        assert!(t.claim_new("created"));
        // Only the first created thread is the turn's.
        assert!(!t.claim_new("later"));
        assert_eq!(t.answers("later"), None);
        assert!(t.answers("created").is_some());
    }

    #[test]
    fn a_thread_turn_is_not_claimed_by_a_new_thread() {
        let mut t = PendingTurn::default();
        t.sent(Some("a".into()));
        assert!(!t.claim_new("b"));
        assert!(t.answers("a").is_some());
    }

    #[test]
    fn a_deferred_answer_from_an_older_turn_is_stale() {
        let mut t = PendingTurn::default();
        t.sent(Some("a".into()));
        let old = t.answers("a").expect("answers");
        // Talk mode ended and began again, sending to the same thread.
        t.clear();
        assert!(!t.current(old));
        t.sent(Some("a".into()));
        assert!(!t.current(old), "the new turn has its own token");
        let new = t.answers("a").expect("answers");
        assert!(t.current(new));
        t.clear();
        assert!(!t.current(new), "answered once");
    }
}
