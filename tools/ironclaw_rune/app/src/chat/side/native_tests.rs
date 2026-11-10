//! The native previews follow the side pane with no standing timer: every
//! way what they show can change reconciles them at once. A test run has no
//! window, so these read what the reconcile planned (`native_view::probe`):
//! each slot's rect, or `None` when its view is hidden.

use std::time::{Duration, Instant};

use rune::{Env, Runtime};

use super::{Dock, NATIVE_SETTLE, Tab, browser, side_mut};
use crate::native_view::probe;
use crate::state::{Page, State};

const FRAME: f64 = 1. / 60.;
const BROWSER: usize = 0;

/// The app's real view and poll over a chat that is not a fixture (a
/// fixture never shows a native view), in a 1280 × 820 window, with the
/// Browser tab already open on a page when the first frame draws: the tab
/// is laid out in place from the start, with no slide for its drawn rect
/// to wait on. The side dock counts as opened at `opened`.
fn runtime(opened: Option<Instant>) -> Runtime<State> {
    let mut s = crate::chat::fixtures::test_state();
    crate::chat::fixtures::chat(&mut s);
    s.fixture = false;
    browser::go(&mut s, "http://localhost:3000".into());
    if let Some(x) = side_mut(&mut s) {
        assert!(x.opened_at[Dock::Side.ix()].is_some(), "the dock opened");
        x.opened_at[Dock::Side.ix()] = opened;
    }
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.poll = Some(crate::poll);
    rt.frame(0.);
    for _ in 0..30 {
        rt.frame(FRAME);
    }
    rt
}

/// The Browser tab open on a page, its dock long settled and drawn.
fn browsing() -> Runtime<State> {
    let rt = runtime(None);
    assert!(
        probe(&rt.state).planned[BROWSER].is_some(),
        "the page shows once its tab is drawn"
    );
    rt
}

fn shown(rt: &Runtime<State>) -> bool {
    probe(&rt.state).planned[BROWSER].is_some()
}

/// One frame: Rune polls first, then runs the queued handler, then the
/// view. Only the view run can have reconciled what the handler changed.
fn one_frame(rt: &mut Runtime<State>, change: impl FnOnce(&mut State) + 'static) {
    rt.update(move |s, _| change(s));
    rt.frame(FRAME);
}

#[test]
fn a_tab_switch_hides_and_shows_the_page_in_the_same_frame() {
    let mut rt = browsing();
    one_frame(&mut rt, |s| super::show(s, Tab::Activity));
    assert!(!shown(&rt), "another tab: the page hides at once");
    one_frame(&mut rt, |s| super::show(s, Tab::Browser));
    assert!(shown(&rt), "back on its tab: it shows at once");
}

#[test]
fn an_overlay_hides_the_page_in_the_same_frame_and_closing_it_shows_it_again() {
    let mut rt = browsing();
    one_frame(&mut rt, |s| {
        s.workspace.confirm_delete = Some("t-pin".into())
    });
    assert!(!shown(&rt), "a dialog covers the window: the page hides");
    one_frame(&mut rt, |s| s.workspace.confirm_delete = None);
    assert!(shown(&rt), "the dialog closed: the page is back");
}

#[test]
fn leaving_the_chat_and_closing_the_page_hide_it_in_the_same_frame() {
    let mut rt = browsing();
    one_frame(&mut rt, |s| s.page = Page::Usage);
    assert!(!shown(&rt), "another page of the app");
    one_frame(&mut rt, |s| s.page = Page::Chat);
    assert!(shown(&rt));
    one_frame(&mut rt, |s| {
        s.update_prefs(|p| p.side_open = false);
    });
    assert!(!shown(&rt), "its dock closed");
}

#[test]
fn a_resized_window_moves_the_page_to_its_new_place() {
    let mut rt = browsing();
    let before = probe(&rt.state).planned[BROWSER].expect("shown");
    rt.set_viewport((1000., 700.), 1.);
    for _ in 0..30 {
        rt.frame(FRAME);
    }
    let after = probe(&rt.state).planned[BROWSER].expect("still shown");
    assert_ne!(before, after, "the planned rect follows the layout");
    assert!(after[0] + after[2] <= 1000.5 && after[1] + after[3] <= 700.5);
}

/// A dock that just opened holds its native view back until the slide is
/// over. Nothing in the state changes at that moment, so it takes a wake:
/// exactly one, for that moment.
#[test]
fn a_dock_that_just_opened_shows_its_page_once_the_slide_has_settled() {
    // The settle time runs on the wall clock, the frames on the frame
    // clock: the dock "opens" a minute from now, so however long the frames
    // take it has not settled.
    let opened = Instant::now() + Duration::from_secs(60);
    let mut rt = runtime(Some(opened));
    assert!(!shown(&rt), "still sliding open");
    let p = probe(&rt.state);
    assert_eq!(p.next_change, Some(opened + NATIVE_SETTLE));
    assert_eq!(p.wake_asked, p.next_change, "one wake, for that moment");

    // The slide is over. No handler runs and no view: only the poll the
    // wake brings.
    if let Some(c) = rt.state.chat.as_mut() {
        c.side.opened_at[Dock::Side.ix()] = Some(Instant::now() - NATIVE_SETTLE);
    }
    rt.frame(FRAME);
    assert!(shown(&rt), "settled: the page shows");
}

/// The point of the change: a page that is simply open costs no wakes.
#[test]
fn an_open_page_at_rest_asks_for_no_wake() {
    let rt = browsing();
    assert_eq!(probe(&rt.state).next_change, None);
}
