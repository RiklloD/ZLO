//! Streamed words with Reduce Motion on land at once: ZLO asks for no word
//! fade, and Rune must not put a short one back. Upstream Rune turns
//! `reveal(0)` into a 0.15 s fade under reduced motion, and rune-kit's
//! streaming Markdown always asks for at least `reveal(0)`, so the app can't
//! opt out (anomalyco/rune#2015). Ignored until Rune ships the fix; run it
//! with `--ignored` on every Rune bump and drop the `ignore` once it passes.

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

#[test]
#[ignore = "needs anomalyco/rune#2015: reduced motion turns reveal(0) into a fade"]
fn streamed_words_land_at_once_with_reduce_motion_on() {
    TEXT.with(|t| *t.borrow_mut() = "The reply starts".into());
    let mut s = crate::chat::fixtures::test_state();
    s.prefs.reduce_motion = "on".into();
    let mut rt = Runtime::new(s, view, (600., 400.), Env::new(true, 1.));
    rt.frame(0.);
    for _ in 0..60 {
        rt.frame(DT);
    }

    TEXT.with(|t| t.borrow_mut().push_str(" and more words arrive"));
    rt.invalidate();
    rt.frame(DT);
    assert!(
        !rt.frame(DT) && rt.wake_deadline().is_none(),
        "no frames after the words land: nothing fades in"
    );
}
