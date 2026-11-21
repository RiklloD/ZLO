//! Plugins motion: the list ↔ plugin page drill plays in both directions,
//! a drawer wipes open, a saved field's Save slides away, and the pages
//! draw nothing at rest.

use crate::pages::providers::motion_tests::{
    act, assert_at_rest, frames, frames_over, opacity, rect, runtime,
};
use crate::state::State;

const LIST: &str = "plugins-drill-list";
const DETAIL: &str = "plugins-drill-detail";

#[test]
fn opening_a_plugin_comes_in_from_the_right_and_back_returns_from_the_left() {
    let mut rt = runtime(super::fixtures::plugins_list);
    let list_at = rect(&rt, LIST);
    act(&mut rt, |s| super::plugin::open_detail(s, "linear".into()));
    frames(&mut rt, 2);
    let (detail, list) = (rect(&rt, DETAIL), rect(&rt, LIST));
    assert!(
        detail[0] > list_at[0] + 1.,
        "the page comes in from the right: {detail:?}"
    );
    assert!(
        list[0] < list_at[0] - 1.,
        "the list steps out to the left: {list:?}"
    );
    assert!(opacity(&rt, DETAIL) < 1., "and fades in");
    frames(&mut rt, 60);
    assert!(rt.presented(LIST).is_none(), "the list is gone");
    assert!(
        (rect(&rt, DETAIL)[0] - list_at[0]).abs() < 0.5,
        "the page lands in place"
    );

    act(&mut rt, super::plugin::close_detail);
    frames(&mut rt, 2);
    let list = rect(&rt, LIST);
    assert!(
        list[0] < list_at[0] - 1.,
        "Back brings the list in from the left: {list:?}"
    );
    assert!(
        rect(&rt, DETAIL)[0] > list_at[0] + 1.,
        "the page leaves to the right"
    );
    frames(&mut rt, 60);
    assert!(rt.presented(DETAIL).is_none());
    assert!((rect(&rt, LIST)[0] - list_at[0]).abs() < 0.5);
}

#[test]
fn a_refresh_on_the_plugin_page_does_not_replay_the_drill() {
    let mut rt = runtime(|s| super::fixtures::plugin_settings(s, false));
    frames(&mut rt, 60);
    let at = rect(&rt, DETAIL);
    // The page's refresh reopens the same plugin.
    act(&mut rt, |s| super::plugin::open_detail(s, "linear".into()));
    frames(&mut rt, 2);
    assert!(
        (rect(&rt, DETAIL)[0] - at[0]).abs() < 0.5,
        "the level stays put"
    );
    assert!((opacity(&rt, DETAIL) - 1.).abs() < 1e-3);
}

#[test]
fn the_setup_drawer_wipes_open() {
    let mut rt = runtime(super::fixtures::plugins_list);
    let id = rt.state.pages.extensions.installed[0].id().to_string();
    act(&mut rt, move |s| super::open_setup(s, id.clone()));
    frames(&mut rt, 2);
    let mid = opacity(&rt, "setup");
    assert!(mid > 0. && mid < 1., "it fades in as it opens: {mid}");
    frames(&mut rt, 60);
    assert!((opacity(&rt, "setup") - 1.).abs() < 1e-3);
}

#[test]
fn a_saved_fields_save_button_slides_away() {
    let mut rt = runtime(|s| super::fixtures::plugin_settings(s, false));
    frames(&mut rt, 60);
    let key = "plugin-setting-limit-save";
    assert!(rt.presented(key).is_some(), "the dirty field shows Save");
    // The save's reply: the field matches what is stored again.
    act(&mut rt, |s| {
        if let Some(d) = s.pages.extensions.detail.as_mut()
            && let Some((_, edit)) = d.inputs.iter_mut().find(|(id, _)| id == "limit")
        {
            edit.replace_text("25");
        }
    });
    frames(&mut rt, 2);
    let mid = opacity(&rt, key);
    assert!(mid < 1., "it fades as it slides out: {mid}");
    frames(&mut rt, 60);
    assert!(rt.presented(key).is_none());
}

#[test]
fn the_plugin_pages_draw_nothing_at_rest() {
    for fill in [
        super::fixtures::plugins_list as fn(&mut State),
        |s: &mut State| super::fixtures::plugin_settings(s, false),
    ] {
        let mut rt = runtime(fill);
        assert_at_rest(&mut rt, "Plugins");
    }
}

#[test]
fn waiting_for_the_bot_breathes_at_15_fps_only() {
    let mut rt = runtime(super::fixtures::extensions);
    let n = frames_over(&mut rt, 2.);
    assert!(
        !rt.frame(1. / 120.),
        "the waiting line never asks for display-rate frames"
    );
    assert!((10..=34).contains(&n), "2 s of waiting drew {n} frames");
}
