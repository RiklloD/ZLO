//! The virtual inbox in the real view: only mounted rows exist, and a
//! resorted row glides to its new place instead of jumping.

use rune::{App, AppSpec, Env, Runtime};
use zlo_client::inbox::InboxState;

use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

fn runtime(fill: fn(&mut State)) -> Runtime<State> {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1280., 820.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    rt
}

/// The drawn top of the element keyed `key`.
fn top(rt: &Runtime<State>, key: &str) -> Option<f32> {
    rt.elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(key))
        .map(|e| e.rect[1])
}

#[test]
fn a_long_history_builds_only_the_rows_on_screen() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_inbox_long);
    rt.frame(1. / 60.);
    let built = rt
        .elements()
        .into_iter()
        .filter(|e| {
            e.key
                .as_deref()
                .is_some_and(|k| k.starts_with("row-t-long") || k.starts_with("slim-row-"))
        })
        .count();
    assert!(built > 0, "rows show");
    assert!(built < 100, "1,000 threads, {built} rows built");
}

#[test]
fn a_pinned_row_glides_to_the_top() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    rt.frame(1. / 60.);
    let from = top(&rt, "row-t-plain").expect("the row shows");
    rt.update(|s, _| {
        let rows = &mut s.workspace.inbox.list.rows;
        if let Some(r) = rows.iter_mut().find(|r| r.thread_id == "t-plain") {
            r.state = InboxState::Pinned;
        }
    });
    let mut seen = Vec::new();
    for _ in 0..60 {
        rt.frame(1. / 60.);
        seen.extend(top(&rt, "row-t-plain"));
    }
    let to = *seen.last().expect("still shown");
    assert!(to < from, "pinned rows sit on top: {from} → {to}");
    let between = seen
        .iter()
        .filter(|y| **y < from - 1. && **y > to + 1.)
        .count();
    assert!(between >= 3, "it glides through {between} frames: {seen:?}");
}

#[test]
fn a_short_list_seats_its_shelves_at_the_bottom() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_inbox_short);
    for _ in 0..10 {
        rt.frame(1. / 60.);
    }
    let card = top(&rt, "row-t-short-2").expect("the last card shows");
    let fold = top(&rt, "fold-settled").expect("the Settled fold shows");
    let footer = top(&rt, "sidebar-footer").expect("the footer shows");
    assert!(
        fold - card > 300.,
        "the fold leaves the cards: {card} → {fold}"
    );
    assert!(
        (footer - fold - 36.).abs() < 16.,
        "and sits on the footer, as GPUI's: fold {fold}, footer {footer}"
    );
}
