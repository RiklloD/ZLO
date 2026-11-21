//! One short confirmation at a time in the side panes: a copy button's
//! check, Files' "attached" check, a commit's "committed" check. The
//! handler that did the thing [`flash`]es its key; the view asks
//! [`shows`]; the mark reverts on its own after `kit::COPIED_FOR` with one
//! timer (no standing loop), popping back as it popped in.

use std::time::Instant;

use rune::Tx;

use super::{side, side_mut, thread_id};
use crate::design::kit::glyphs::COPIED_FOR;
use crate::jobs;
use crate::state::State;

/// The key last flashed in the open chat's panes, and when.
#[derive(Default)]
pub(super) struct Flash(Option<(&'static str, Instant)>);

/// Show `key`'s confirmation, from the handler (or the person's own job
/// reply) that did the thing, and revert it after [`COPIED_FOR`].
pub(super) fn flash(s: &mut State, key: &'static str, tx: &mut Tx) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let at = Instant::now();
    x.flash = Flash(Some((key, at)));
    tx.animated();
    jobs::after(s, COPIED_FOR, move |s, tx| {
        if thread_id(s).as_deref() != Some(thread.as_str()) {
            return;
        }
        let Some(x) = side_mut(s) else { return };
        // A later flash owns the mark now.
        if x.flash.0.is_some_and(|(k, t)| k == key && t == at) {
            x.flash = Flash(None);
            // The person's own confirmation going back, not a poll.
            tx.animated();
        }
    });
}

/// Whether `key`'s confirmation shows.
pub(super) fn shows(s: &State, key: &str) -> bool {
    side(s)
        .and_then(|x| x.flash.0)
        .is_some_and(|(k, at)| k == key && at.elapsed() < COPIED_FOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flash_shows_its_own_key_and_asks_for_motion() {
        let mut s = crate::chat::fixtures::test_state();
        s.chat = Some(crate::chat::ChatState::new("t1".into()));
        let mut tx = Tx::default();
        flash(&mut s, "a", &mut tx);
        assert!(crate::workspace::asked_motion(&tx));
        assert!(shows(&s, "a"));
        assert!(!shows(&s, "b"), "another button stays as it was");
    }
}
