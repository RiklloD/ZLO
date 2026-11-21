//! Workspace motion that has to keep playing: the gliding selection pills
//! sit on their rows and glide, the ⌘ keycaps pop in the profile menu, the
//! boot reply crossfades, the hero staggers only on a new draft.

use rune::{App, AppSpec, Env, Runtime, Tx};

use crate::state::{Conn, Page, State};

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
    rt.frame(1. / 60.);
    rt
}

fn frames(rt: &mut Runtime<State>, n: usize) {
    for _ in 0..n {
        rt.frame(1. / 60.);
    }
}

fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
    rt.presented(key)
        .unwrap_or_else(|| panic!("{key} is drawn"))
        .0
}

fn close(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.5)
}

/// The element under the middle of `key`'s rect, by path.
fn under(rt: &Runtime<State>, key: &str) -> String {
    let [x, y, w, h] = rect(rt, key);
    rt.element_at(x + w / 2., y + h / 2.)
        .map(|p| p.path().to_string())
        .unwrap_or_default()
}

#[test]
fn the_settings_pill_covers_the_open_page_and_glides_to_the_next() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_settings_nav);
    assert!(
        close(rect(&rt, "nav-pill"), rect(&rt, "nav-Appearance")),
        "the pill sits exactly on the open page's row"
    );
    let from = rect(&rt, "nav-pill");
    rt.update(|s, tx| {
        tx.animated();
        crate::workspace::show(s, Page::Providers);
    });
    frames(&mut rt, 3);
    let mid = rect(&rt, "nav-pill");
    frames(&mut rt, 60);
    let to = rect(&rt, "nav-pill");
    assert!(close(to, rect(&rt, "nav-Providers")));
    assert!(
        mid[1] > from[1] + 1. && mid[1] < to[1] - 1.,
        "it glides: {from:?} → {mid:?} → {to:?}"
    );
}

#[test]
fn the_command_pill_sits_on_the_selected_row_and_glides() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_command_menu);
    assert!(under(&rt, "cmdk-pill").contains("cmdk-row-"));
    let from = rect(&rt, "cmdk-pill");
    rt.update(|s, tx| {
        tx.animated();
        if let Some(m) = s.workspace.command.as_mut() {
            m.selected = 0;
        }
    });
    frames(&mut rt, 3);
    let mid = rect(&rt, "cmdk-pill");
    assert!(
        mid[1] < from[1] && mid[1] > from[1] - 36.,
        "{from:?} → {mid:?}"
    );
    frames(&mut rt, 60);
    assert!(under(&rt, "cmdk-pill").contains("cmdk-row-"));
}

#[test]
fn the_profile_menu_glides_its_pill_and_pops_its_keycaps() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_profile_menu);
    assert!(under(&rt, "profile-pill").contains("pick-profile-id-"));
    assert!(rt.presented("shortcut-badge").is_none() || !rt.state.workspace.inbox.cmd_held);
    // ⌘ goes down: the menu's keycaps grow in rather than flashing.
    rt.update(|s, tx| {
        tx.animated();
        s.workspace.inbox.picker.fixture_cmd(true);
    });
    frames(&mut rt, 2);
    let pose = rt
        .presented_pose("shortcut-badge")
        .expect("a keycap is drawn");
    assert!(pose.scale() < 0.99, "it pops in: {}", pose.scale());
    frames(&mut rt, 60);
    let pose = rt.presented_pose("shortcut-badge").expect("still drawn");
    assert!((pose.scale() - 1.).abs() < 1e-3);
}

#[test]
fn the_connect_reply_animates_only_when_it_replaces_the_launch_screen() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    let session = || -> zlo_client::wire::SessionInfo {
        serde_json::from_str(r#"{"tenant_id":"t","user_id":"u"}"#).expect("session")
    };
    let list = |s: &State| s.workspace.inbox.list.clone();
    rt.update(|s, _| s.conn = Conn::Connecting);
    let mut tx = Tx::default();
    let l = list(&rt.state);
    crate::workspace::connected(&mut rt.state, session(), l, None, &mut tx);
    assert!(
        crate::workspace::asked_motion(&tx),
        "loading → app crossfades"
    );
    let mut tx = Tx::default();
    let l = list(&rt.state);
    crate::workspace::connected(&mut rt.state, session(), l, None, &mut tx);
    assert!(!crate::workspace::asked_motion(&tx), "a refresh lands");
}

#[test]
fn the_hero_staggers_on_a_new_draft_only() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    rt.update(|s, tx| {
        tx.animated();
        s.chat = None;
        crate::workspace::show(s, Page::Draft);
    });
    frames(&mut rt, 1);
    let greeting = rt.presented("draft-greeting").expect("greeting").1;
    let card = rt.presented("draft-composer").expect("card").1;
    assert!(
        card < greeting,
        "the composer waits its turn: {greeting} vs {card}"
    );
    frames(&mut rt, 60);
    assert!(!rt.state.workspace.draft.as_ref().is_some_and(|d| d.fresh));
    assert!(!crate::workspace::draft::entering(&rt.state));
}

#[test]
/// Send's plate eases on its live colours (`PlateTint`, paint-only), so the
/// keystroke's commit itself never animates.
fn the_first_keystroke_in_the_hero_eases_send_and_later_ones_land() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_empty);
    let fill = |rt: &Runtime<State>| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("round-draft-send"))
            .expect("send")
            .fill
    };
    let type_text = |rt: &mut Runtime<State>, t: &str| {
        rt.key(rune::KeyEvent {
            key: rune::Key::Text(t.into()),
            mods: Default::default(),
        })
    };
    let [x, y, w, h] = rect(&rt, "composer-input");
    rt.pointer_down(x + w / 2., y + h / 2.);
    rt.pointer_up(x + w / 2., y + h / 2.);
    frames(&mut rt, 30);
    let idle = fill(&rt);
    type_text(&mut rt, "a");
    frames(&mut rt, 2);
    let mid = fill(&rt);
    frames(&mut rt, 60);
    let ready = fill(&rt);
    assert_ne!(idle, ready, "the plate differs from the wash");
    assert!(
        mid != ready && mid != idle,
        "the first keystroke eases the plate's paint ({mid:?})"
    );
    type_text(&mut rt, "b");
    frames(&mut rt, 1);
    assert_eq!(fill(&rt), ready, "later keystrokes land");
    let text = rt
        .state
        .workspace
        .draft
        .as_ref()
        .map(|d| d.input.text.clone());
    assert_eq!(text.as_deref(), Some("ab"));
}

#[test]
fn the_sidebar_crossfades_between_the_inbox_and_settings() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    rt.update(|s, tx| {
        tx.animated();
        crate::workspace::show(s, Page::Settings);
    });
    frames(&mut rt, 3);
    let (_, nav) = rt.presented("settings-nav").expect("the nav mounts");
    let leaving = rt.presented("inbox").map_or(0., |(_, o)| o);
    assert!(nav > 0. && nav < 1., "the nav fades in: {nav}");
    assert!(
        leaving > 0. && leaving < 1.,
        "the inbox fades out: {leaving}"
    );
    frames(&mut rt, 60);
    assert!(rt.presented("inbox").is_none());
    assert_eq!(rt.presented("settings-nav").map(|(_, o)| o), Some(1.));
}

/// Edits that change the hero composer's height land in their own frame:
/// the card and the greeting above it take their places at once, while
/// Send's plate still eases on its live colours. A multiline paste into an
/// empty draft and a select-all delete of a multiline one both flip Send.
#[test]
fn editing_the_hero_lands_its_layout_at_once() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_empty);
    frames(&mut rt, 90);
    let [x, y, w, h] = rect(&rt, "composer-input");
    rt.pointer_down(x + w / 2., y + h / 2.);
    rt.pointer_up(x + w / 2., y + h / 2.);
    frames(&mut rt, 30);
    let rects = |rt: &Runtime<State>| (rect(rt, "draft-composer"), rect(rt, "draft-greeting"));
    let fill = |rt: &Runtime<State>| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("round-draft-send"))
            .expect("send")
            .fill
    };
    let empty = rects(&rt);
    let idle = fill(&rt);

    rt.paste("one\ntwo\nthree\nfour");
    frames(&mut rt, 1);
    let pasted = rects(&rt);
    let easing = fill(&rt);
    frames(&mut rt, 60);
    let tall = rects(&rt);
    assert!(tall.0[3] > empty.0[3] + 20., "four lines grow the card");
    assert_eq!(pasted, tall, "the paste's frame has the final layout");
    let ready = fill(&rt);
    assert!(
        easing != idle && easing != ready,
        "Send's plate eases on its own ({easing:?})"
    );

    rt.key(rune::KeyEvent {
        key: rune::Key::Command('a'),
        mods: rune::Mods {
            cmd: true,
            ..Default::default()
        },
    });
    frames(&mut rt, 1);
    rt.key(rune::KeyEvent {
        key: rune::Key::Backspace,
        mods: Default::default(),
    });
    frames(&mut rt, 1);
    let cleared = rects(&rt);
    frames(&mut rt, 60);
    assert_eq!(rects(&rt), empty, "back to the one-line card");
    assert_eq!(cleared, empty, "the delete's frame has the final layout");
}
