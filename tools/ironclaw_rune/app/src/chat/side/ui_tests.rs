//! A tab's scrolling column keeps its offset when it unmounts and mounts
//! again (a tab switch, a dock closing and reopening): Rune drops the
//! viewport's own state on unmount, so [`super::scroll`] restores it.

use std::cell::Cell;

use rune::{Cx, El, Env, Runtime, col, el};

use crate::state::State;

thread_local! {
    static SHOWN: Cell<bool> = const { Cell::new(true) };
}

const DT: f64 = 1. / 60.;

fn view(_: &State, _: &mut Cx<State>) -> El {
    if !SHOWN.with(Cell::get) {
        return el().key("gone");
    }
    let rows = (0..60).map(|i| el().key(format!("row-{i}")).w_full().h(40.));
    col().key("frame").w(300.).h(400.).child(super::scroll(
        "keeps-offset-test",
        col().w_full().children(rows),
    ))
}

fn row_top(rt: &Runtime<State>, key: &str) -> Option<f32> {
    rt.elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(key))
        .map(|e| e.rect[1])
}

fn frames(rt: &mut Runtime<State>, n: usize) {
    for _ in 0..n {
        rt.frame(DT);
    }
}

#[test]
fn a_tab_column_comes_back_where_it_was_scrolled() {
    SHOWN.with(|c| c.set(true));
    let s = crate::chat::fixtures::test_state();
    let mut rt = Runtime::new(s, view, (300., 400.), Env::new(true, 1.));
    rt.frame(0.);
    frames(&mut rt, 10);
    let top = row_top(&rt, "row-10").expect("row 10 is laid out");

    rt.update(|_, tx| tx.scroll_to_instant("keeps-offset-test", 0., 300.));
    frames(&mut rt, 10);
    let scrolled = row_top(&rt, "row-10").expect("row 10 is still laid out");
    assert!(
        (top - scrolled - 300.).abs() < 1.,
        "scrolled 300 pt: {top} -> {scrolled}"
    );

    // Unmount it instantly, the way a tab switch does, then bring it back.
    SHOWN.with(|c| c.set(false));
    rt.update(|_, tx| tx.instant());
    frames(&mut rt, 10);
    assert!(row_top(&rt, "row-10").is_none(), "the column unmounted");

    SHOWN.with(|c| c.set(true));
    rt.update(|_, tx| tx.instant());
    frames(&mut rt, 10);
    let back = row_top(&rt, "row-10").expect("row 10 is laid out again");
    assert!(
        (back - scrolled).abs() < 1.,
        "came back at {back}, was scrolled to {scrolled}"
    );
}
