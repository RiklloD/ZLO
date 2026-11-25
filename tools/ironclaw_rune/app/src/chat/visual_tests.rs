//! Streamed words with Reduce Motion on land at once: ZLO asks for no word
//! fade, and Rune must not put a short one back. rune-kit's streaming
//! Markdown always asks for at least `reveal(0)`, which Rune keeps instant
//! under reduced motion (anomalyco/rune#2029).

use std::cell::RefCell;

use rune::{Cx, El, Env, Runtime, col};

use crate::state::State;

thread_local! {
    static TEXT: RefCell<String> = RefCell::new("The reply starts".into());
}

const DT: f64 = 1. / 60.;

fn view(s: &State, cx: &mut Cx<State>) -> El {
    crate::design::look::apply_motion(s, cx);
    let k = crate::design::kit(&s.palette);
    let text = TEXT.with(|t| t.borrow().clone());
    col()
        .w(600.)
        .child(super::prose(s, "t1", "m1", &text, true, &k))
}

/// Streams more words into a reply under `reduce_motion` (with the OS
/// setting `system`) and says whether anything still draws afterwards.
fn words_fade_in(reduce_motion: &str, system: bool) -> bool {
    TEXT.with(|t| *t.borrow_mut() = "The reply starts".into());
    let mut s = crate::chat::fixtures::test_state();
    s.prefs.reduce_motion = reduce_motion.into();
    let mut rt = Runtime::new(s, view, (600., 400.), Env::new(true, 1.));
    rt.set_system_reduced_motion(system);
    rt.frame(0.);
    for _ in 0..60 {
        rt.frame(DT);
    }

    TEXT.with(|t| t.borrow_mut().push_str(" and more words arrive"));
    rt.invalidate();
    rt.frame(DT);
    rt.frame(DT) || rt.wake_deadline().is_some()
}

#[test]
fn streamed_words_land_at_once_with_reduce_motion_on() {
    assert!(
        !words_fade_in("on", false),
        "Reduce Motion on: nothing fades in after the words land"
    );
    assert!(
        !words_fade_in("system", true),
        "Reduce Motion following the OS, with the OS reducing: nothing fades in"
    );
    assert!(
        words_fade_in("system", false),
        "the OS not reducing: the words still fade in"
    );
}
