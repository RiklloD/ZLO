//! One short confirmation at a time on the pages: a copy button's check
//! (a webhook URL, a pairing link, a link code). The handler that did the
//! thing (or the person's own job reply) [`flash`]es its key; the view
//! asks [`shows`]; the mark reverts on its own after `kit::COPIED_FOR`
//! with one timer (no standing loop), popping back as it popped in. The
//! pages' counterpart of `chat/side/flash.rs`.

use std::time::Instant;

use rune::Tx;

use crate::design::kit::glyphs::COPIED_FOR;
use crate::jobs;
use crate::state::State;

/// The key last flashed on a page, and when.
#[derive(Default)]
pub struct Flash(Option<(String, Instant)>);

/// Show `key`'s confirmation and revert it after [`COPIED_FOR`]. Asks for
/// an animated commit, so it pops from a job reply too.
pub fn flash(s: &mut State, key: impl Into<String>, tx: &mut Tx) {
    let key = key.into();
    let at = Instant::now();
    s.pages.flash = Flash(Some((key.clone(), at)));
    tx.animated();
    jobs::after(s, COPIED_FOR, move |s, tx| {
        // A later flash owns the mark now.
        if s.pages
            .flash
            .0
            .as_ref()
            .is_some_and(|(k, t)| *k == key && *t == at)
        {
            s.pages.flash = Flash(None);
            // The person's own confirmation going back, not a poll.
            tx.animated();
        }
    });
}

/// Whether `key`'s confirmation shows.
pub fn shows(s: &State, key: &str) -> bool {
    s.pages
        .flash
        .0
        .as_ref()
        .is_some_and(|(k, at)| k == key && at.elapsed() < COPIED_FOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flash_shows_its_own_key_and_asks_for_motion() {
        let mut s = crate::chat::fixtures::test_state();
        let mut tx = Tx::default();
        flash(&mut s, "a", &mut tx);
        assert!(crate::workspace::asked_motion(&tx));
        assert!(shows(&s, "a"));
        assert!(!shows(&s, "b"), "another button stays as it was");
    }
}
