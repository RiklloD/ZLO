//! One holder per Codex thread session at a time: a turn (prompt or review),
//! a compaction asked for now, or an idle compaction. A holder
//! keeps its [`Claim`] until its app-server process has exited; the claim
//! releases on drop, so an early return or a panic cannot leave the
//! session reserved.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;

#[derive(Debug, Default)]
pub(super) struct Admission(Mutex<HashMap<ThreadId, &'static str>>);

/// The session of `thread`, reserved for `what` until dropped.
#[derive(Debug)]
pub(super) struct Claim {
    admission: Arc<Admission>,
    thread: ThreadId,
}

impl Admission {
    /// Reserve `thread`'s session for `what`; `None` while another holds it.
    pub(super) fn try_claim(
        self: &Arc<Self>,
        thread: &ThreadId,
        what: &'static str,
    ) -> Option<Claim> {
        // silent-ok: a poisoned map only follows a panic elsewhere; refusing is safe
        let mut held = self.0.lock().ok()?;
        if held.contains_key(thread) {
            return None;
        }
        held.insert(thread.clone(), what);
        Some(Claim {
            admission: Arc::clone(self),
            thread: thread.clone(),
        })
    }

    /// What holds `thread`'s session now, if anything.
    pub(super) fn holder(&self, thread: &ThreadId) -> Option<&'static str> {
        self.0.lock().ok().and_then(|h| h.get(thread).copied())
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut held) = self.admission.0.lock() {
            held.remove(&self.thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_per_thread_until_the_claim_drops() {
        let admission = Arc::new(Admission::default());
        let a = ThreadId::new("a").unwrap();
        let b = ThreadId::new("b").unwrap();
        let claim = admission.try_claim(&a, "review").unwrap();
        assert!(admission.try_claim(&a, "turn").is_none());
        assert_eq!(admission.holder(&a), Some("review"));
        // Other threads are independent.
        assert!(admission.try_claim(&b, "turn").is_some());
        drop(claim);
        assert!(admission.holder(&a).is_none());
        assert!(admission.try_claim(&a, "turn").is_some());
    }
}
