//! The frame's sidebar pane: the button and the grip move the pane, and
//! prefs follow it (the open state at once, a width once the drag ends).

use rune::{App, AppSpec, Env, Runtime};

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
    rt.frame(1. / 60.);
    rt
}

fn frames(rt: &mut Runtime<State>, n: usize) {
    for _ in 0..n {
        rt.frame(1. / 60.);
    }
}

#[test]
fn the_sidebar_button_folds_the_pane_and_prefs_follow() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    assert!(rt.state.workspace.sidebar.is_open());
    let (x, y) = rt.find("sidebar-toggle").expect("the button shows");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    frames(&mut rt, 30);
    assert!(!rt.state.workspace.sidebar.is_open(), "the pane folds");
    assert!(rt.state.prefs.sidebar_hidden, "and the choice is saved");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    frames(&mut rt, 30);
    assert!(rt.state.workspace.sidebar.is_open() && !rt.state.prefs.sidebar_hidden);
}

#[test]
fn dragging_the_grip_resizes_and_saves_the_width_on_release() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    let before = rt.state.workspace.sidebar.width();
    let (x, y) = rt.find("sidebar-grip").expect("the grip shows");
    rt.pointer_down(x, y);
    for step in 1..=8 {
        rt.pointer_move(x + step as f32 * 5., y);
        rt.frame(1. / 60.);
    }
    let dragged = rt.state.workspace.sidebar.width();
    assert!(
        (dragged - (before + 40.)).abs() <= 1.,
        "{before} → {dragged}"
    );
    assert!(rt.state.workspace.sidebar.is_resizing());
    rt.pointer_up(x + 40., y);
    frames(&mut rt, 2);
    // The app's `poll` runs before every frame; the runtime here has none.
    rt.update(|s, tx| {
        crate::workspace::poll(s, tx);
    });
    let saved = rt.state.prefs.sidebar_width;
    assert!(
        (saved - dragged).abs() <= 0.5,
        "saved {saved}, dragged to {dragged}"
    );
}

/// ⌘B is a menu item, which Rune runs as a queued update: the toggle must
/// slide the pane there too, its follow-up commits queued only once the
/// slide's own frame is committed.
#[test]
fn a_toggle_from_an_update_slides_the_pane() {
    let mut rt = runtime(crate::workspace::fixtures::fixture_shell);
    frames(&mut rt, 5);
    let width = |rt: &Runtime<State>| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("sidebar-column"))
            .map_or(0., |e| e.rect[0] + e.rect[2])
    };
    let open = width(&rt);
    // The real window commits a queued update at once unless the handler
    // asks for motion (`Tx::animated`); a bare runtime animates every
    // commit, so this runs the menu's own action.
    rt.update(crate::workspace::keys::action("Toggle Sidebar"));
    assert!(
        rt.state.workspace.sidebar_slid.get(),
        "the follow-ups wait for the view"
    );
    let mut seen = Vec::new();
    for _ in 0..30 {
        rt.frame(1. / 60.);
        seen.push(width(&rt));
    }
    assert!(
        !rt.state.workspace.sidebar_slid.get(),
        "the view queued them"
    );
    let between = seen.iter().filter(|w| **w > 1. && **w < open - 1.).count();
    assert!(
        between >= 3,
        "the pane slides through {between} frames: {seen:?}"
    );
}

/// The launch intro: a connection that lands mid-assembly mounts nothing
/// under the stage (no workspace showing through, nothing to press), and
/// the workspace mounts once the reveal starts.
#[test]
fn the_intro_stage_hides_the_workspace_until_its_reveal() {
    use crate::workspace::intro::Intro;
    fn assembling(s: &mut State) {
        crate::workspace::fixtures::fixture_shell(s);
        s.intro = Intro::frozen(0.6, None, false);
    }
    fn revealing(s: &mut State) {
        crate::workspace::fixtures::fixture_shell(s);
        s.intro = Intro::frozen(1.3, Some(1.15), false);
    }
    let shown = |fill: fn(&mut State)| {
        let mut rt = runtime(fill);
        frames(&mut rt, 3);
        assert!(matches!(rt.state.conn, crate::state::Conn::Ready));
        (
            rt.presented("intro").is_some(),
            rt.presented("sidebar-column").is_some(),
        )
    };
    assert_eq!(
        shown(assembling),
        (true, false),
        "assembling: only the stage"
    );
    assert_eq!(
        shown(revealing),
        (true, true),
        "revealing: the workspace is behind the iris"
    );
    assert_eq!(
        shown(crate::workspace::fixtures::fixture_shell),
        (false, true),
        "spent: no stage"
    );
}

/// Shortcuts don't reach the hidden workspace while the stage covers it.
#[test]
fn the_intro_stage_swallows_shortcuts_until_its_reveal() {
    use crate::workspace::intro::Intro;
    let toggle_bar = rune::KeyEvent {
        key: rune::Key::Command('s'),
        mods: rune::Mods {
            // The primary modifier alone: ⌘ on macOS, Ctrl on Linux.
            cmd: true,
            shift: true,
            ..rune::Mods::default()
        },
    };
    let mut s = crate::chat::fixtures::test_state();
    let mut tx = rune::Tx::default();
    let before = s.prefs.status_bar_hidden;
    s.intro = Intro::frozen(0.6, None, false);
    assert!(crate::on_key(&mut s, &toggle_bar, &mut tx), "taken");
    assert_eq!(
        s.prefs.status_bar_hidden, before,
        "nothing ran under the stage"
    );
    s.intro = Intro::frozen(1.3, Some(1.15), false);
    assert!(crate::on_key(&mut s, &toggle_bar, &mut tx));
    assert_ne!(
        s.prefs.status_bar_hidden, before,
        "once it opens, keys work"
    );
}
