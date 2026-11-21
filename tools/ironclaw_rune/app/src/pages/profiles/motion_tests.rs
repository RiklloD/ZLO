//! Profiles motion: list ↔ detail plays in both directions, "Saved."
//! slides in, a picker's chevron turns and its menu pops in, a check box
//! morphs, and the page draws nothing at rest.

use super::pickers::Menu;
use crate::pages::providers::motion_tests::{
    act, assert_at_rest, frames, opacity, rect, runtime, runtime_sized,
};
use crate::state::{Conn, Page, State};

const LIST: &str = "profiles-drill-list";
const DETAIL: &str = "profiles-drill-detail";

fn on_page(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Profiles;
}

fn list(s: &mut State) {
    on_page(s);
    super::fixture_list(s);
}

fn editor(s: &mut State) {
    on_page(s);
    super::fixture_editor(s);
}

#[test]
fn a_profile_opens_from_the_right_and_back_returns_from_the_left() {
    let mut rt = runtime(list);
    let at = rect(&rt, LIST);
    act(&mut rt, |s| super::select(s, Some("work".into())));
    frames(&mut rt, 2);
    assert!(
        rect(&rt, DETAIL)[0] > at[0] + 1.,
        "the detail comes in from the right"
    );
    assert!(
        rect(&rt, LIST)[0] < at[0] - 1.,
        "the list steps out to the left"
    );
    frames(&mut rt, 60);
    assert!(rt.presented(LIST).is_none());
    assert!((rect(&rt, DETAIL)[0] - at[0]).abs() < 0.5);

    act(&mut rt, super::back);
    frames(&mut rt, 2);
    assert!(
        rect(&rt, LIST)[0] < at[0] - 1.,
        "Back brings the list in from the left"
    );
    assert!(
        rect(&rt, DETAIL)[0] > at[0] + 1.,
        "the detail leaves to the right"
    );
    frames(&mut rt, 60);
    assert!(rt.presented(DETAIL).is_none());
    assert!((rect(&rt, LIST)[0] - at[0]).abs() < 0.5);
}

#[test]
fn saved_slides_in_beside_the_buttons() {
    // Tall enough to show the footer under the long SOUL.md (Rune lands
    // motion off screen at once).
    let mut rt = runtime_sized(editor, (1280., 3200.));
    frames(&mut rt, 60);
    // The save's reply.
    act(&mut rt, |s| s.pages.profiles.notice = Some("Saved.".into()));
    frames(&mut rt, 2);
    let mid = opacity(&rt, "profile-notice");
    assert!(mid > 0. && mid < 1., "it fades in: {mid}");
    frames(&mut rt, 60);
    assert!((opacity(&rt, "profile-notice") - 1.).abs() < 1e-3);
}

#[test]
fn a_pickers_chevron_turns_and_its_menu_pops_in() {
    let mut rt = runtime(editor);
    frames(&mut rt, 60);
    let chev = format!("profile-menu-{}-chev", Menu::Tools as u8);
    let pop = format!("profile-menu-{}-pop", Menu::Tools as u8);
    act(&mut rt, |s| super::pickers::toggle_menu(s, Menu::Tools));
    frames(&mut rt, 3);
    let turn = rt.presented_pose(&chev).expect("chevron").rotation();
    assert!(turn > 0.1 && turn < 3.0, "it turns over: {turn}");
    let menu = opacity(&rt, &pop);
    assert!(menu > 0. && menu < 1., "the menu pops in: {menu}");
    frames(&mut rt, 60);
    let turn = rt.presented_pose(&chev).expect("chevron").rotation();
    assert!(
        (turn - std::f32::consts::PI).abs() < 1e-3,
        "it points up: {turn}"
    );
}

#[test]
fn ticking_a_check_box_pops_its_tick() {
    let mut rt = runtime(|s| {
        on_page(s);
        super::fixture_pickers(s);
    });
    frames(&mut rt, 60);
    let tick = format!("check-{}-shell.exec-tick", Menu::Tools as u8);
    assert!(rt.presented(&tick).is_none(), "shell.exec starts unticked");
    act(&mut rt, |s| {
        s.pages.profiles.picks.tools.insert("shell.exec".into());
    });
    frames(&mut rt, 2);
    let scale = rt.presented_pose(&tick).expect("the tick is drawn").scale();
    assert!(scale < 0.99, "it pops in: {scale}");
    frames(&mut rt, 60);
    let scale = rt.presented_pose(&tick).expect("still drawn").scale();
    assert!((scale - 1.).abs() < 1e-3);
    assert_at_rest(&mut rt, "the checklist");
}

#[test]
fn the_profiles_pages_draw_nothing_at_rest() {
    for fill in [list as fn(&mut State), editor] {
        let mut rt = runtime(fill);
        assert_at_rest(&mut rt, "Profiles");
    }
}
