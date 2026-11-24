//! Runtime checks for setup's frame on small windows: the card never
//! outgrows the window, the stepper and the step's actions stay pinned, and
//! the step's body scrolls under them by wheel and by keyboard focus.

use rune::{App, AppSpec, Env, Key, KeyEvent, Mods, Runtime};

use super::view::{EDGE_X, EDGE_Y, scroll_key};
use super::{Step, fixtures};
use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

/// The app's real view over a fixture at a window size.
fn runtime(fill: fn(&mut State), size: (f32, f32)) -> Runtime<State> {
    let app = App::new(AppSpec::new("ZLO test", size, no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, size, Env::new(true, 1.));
    rt.frame(0.);
    settle(&mut rt);
    rt
}

fn settle(rt: &mut Runtime<State>) {
    for _ in 0..90 {
        rt.frame(1. / 60.);
    }
}

fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
    rt.presented(key)
        .map(|(r, _)| r)
        .unwrap_or_else(|| panic!("{key} is drawn"))
}

fn bottom(r: [f32; 4]) -> f32 {
    r[1] + r[3]
}

/// `inner` lies inside `outer` (half a point of slack for rounding).
fn inside(inner: [f32; 4], outer: [f32; 4]) -> bool {
    inner[1] >= outer[1] - 0.5
        && bottom(inner) <= bottom(outer) + 0.5
        && inner[0] >= outer[0] - 0.5
        && inner[0] + inner[2] <= outer[0] + outer[2] + 0.5
}

const SIZES: [(f32, f32); 4] = [(800., 600.), (1100., 700.), (1280., 820.), (1512., 945.)];

/// Every step, short and long.
const STEPS: [crate::scenes::Scene; 8] = [
    ("connect", fixtures::connect),
    ("pairing", fixtures::pairing),
    ("agents", fixtures::agents),
    ("agents-long", fixtures::agents_long),
    ("providers", fixtures::providers),
    ("projects", fixtures::projects),
    ("projects-long", fixtures::projects_long),
    ("projects-loading", fixtures::projects_loading),
];

#[test]
fn the_card_fits_the_window_with_the_stepper_and_actions_in_view() {
    for (w, h) in SIZES {
        for (name, fill) in STEPS {
            let rt = runtime(fill, (w, h));
            let card = rect(&rt, "onboarding-dialog");
            assert!(
                card[1] >= EDGE_Y - 0.5 && bottom(card) <= h - EDGE_Y + 0.5,
                "{name} at {w}x{h}: the card {card:?} stays {EDGE_Y} pt inside the window"
            );
            assert!(
                card[0] >= EDGE_X - 0.5 && card[0] + card[2] <= w - EDGE_X + 0.5,
                "{name} at {w}x{h}: the card {card:?} fits the width"
            );
            for pinned in ["onboarding-steps", "onboarding-footer"] {
                let r = rect(&rt, pinned);
                assert!(
                    inside(r, card),
                    "{name} at {w}x{h}: {pinned} {r:?} is inside the card {card:?}"
                );
            }
            let body = rect(&rt, &scroll_key(&rt.state.onboarding));
            assert!(
                body[3] > 80.,
                "{name} at {w}x{h}: the body keeps room to read ({body:?})"
            );
        }
    }
}

/// The friend's case: a 13-inch window, every agent and the provider list
/// open; the last provider row starts below the fold.
#[test]
fn the_step_body_scrolls_by_wheel_under_a_pinned_stepper_and_footer() {
    let mut rt = runtime(fixtures::agents_long, (800., 600.));
    let viewport = rect(&rt, &scroll_key(&rt.state.onboarding));
    let last = "agents-all-providers";
    assert!(
        bottom(rect(&rt, last)) > bottom(viewport) + 1.,
        "the list is longer than the window: {last} starts out of view"
    );
    let steps = rect(&rt, "onboarding-steps");
    let footer = rect(&rt, "onboarding-footer");
    let (x, y) = (
        viewport[0] + viewport[2] / 2.,
        viewport[1] + viewport[3] / 2.,
    );
    for _ in 0..40 {
        // Negative is toward the end of the content (a trackpad swipe up).
        rt.wheel(x, y, -120.);
        rt.frame(1. / 60.);
    }
    settle(&mut rt);
    let shown = rect(&rt, last);
    assert!(
        inside(shown, rect(&rt, &scroll_key(&rt.state.onboarding))),
        "scrolled to the end, {last} {shown:?} is in view"
    );
    assert_eq!(
        rect(&rt, "onboarding-steps"),
        steps,
        "the stepper stays put"
    );
    assert_eq!(rect(&rt, "onboarding-footer"), footer, "Continue stays put");
}

#[test]
fn keyboard_focus_scrolls_the_focused_control_into_view() {
    let mut rt = runtime(fixtures::agents_long, (800., 600.));
    let target = "agents-all-providers";
    let viewport = rect(&rt, &scroll_key(&rt.state.onboarding));
    assert!(bottom(rect(&rt, target)) > bottom(viewport) + 1.);
    let mut reached = false;
    for _ in 0..80 {
        rt.key(KeyEvent {
            key: Key::Tab,
            mods: Mods::default(),
        });
        rt.frame(1. / 60.);
        if rt.presented_focus_key() == Some(target) {
            reached = true;
            break;
        }
    }
    assert!(reached, "Tab reaches {target}");
    settle(&mut rt);
    let shown = rect(&rt, target);
    assert!(
        inside(shown, rect(&rt, &scroll_key(&rt.state.onboarding))),
        "the focused {target} {shown:?} is scrolled into view"
    );
}

/// Review P3: each step opens at its top. Scrolled to the end of a long
/// project list on a small window, going back to Agents shows the Agents
/// heading, not the Projects offset.
#[test]
fn a_new_step_opens_scrolled_to_its_top() {
    let mut rt = runtime(fixtures::projects_long, (720., 480.));
    let viewport = rect(&rt, &scroll_key(&rt.state.onboarding));
    let (x, y) = (
        viewport[0] + viewport[2] / 2.,
        viewport[1] + viewport[3] / 2.,
    );
    for _ in 0..60 {
        rt.wheel(x, y, -120.);
        rt.frame(1. / 60.);
    }
    settle(&mut rt);
    assert!(
        rect(&rt, "step-title")[1] < viewport[1] - 1.,
        "the Projects heading scrolled out of view"
    );
    rt.update(|s, _| super::go_to(s, Step::Agents));
    settle(&mut rt);
    let viewport = rect(&rt, &scroll_key(&rt.state.onboarding));
    let title = rect(&rt, "step-title");
    assert!(
        inside(title, viewport),
        "the Agents heading {title:?} is in view in {viewport:?}"
    );
}

/// Every step keeps its primary action in the pinned footer, never in the
/// scrolling body.
#[test]
fn each_steps_primary_action_lives_in_the_footer() {
    for (fill, key) in [
        (fixtures::connect as fn(&mut State), "connect-continue"),
        (fixtures::agents_long, "agents-continue"),
        (fixtures::projects_long, "projects-import"),
        (fixtures::projects_loading, "projects-skip"),
    ] {
        let rt = runtime(fill, (800., 600.));
        assert!(
            inside(rect(&rt, key), rect(&rt, "onboarding-footer")),
            "{key} sits in the footer"
        );
    }
    let rt = runtime(fixtures::agents_long, (800., 600.));
    assert_eq!(rt.state.onboarding.step, Step::Agents);
}

/// Agents at 800x600 scrolled to its end.
fn agents_scrolled_to_the_end() -> Runtime<State> {
    let mut rt = runtime(fixtures::agents_long, (800., 600.));
    let viewport = rect(&rt, &scroll_key(&rt.state.onboarding));
    let (x, y) = (
        viewport[0] + viewport[2] / 2.,
        viewport[1] + viewport[3] / 2.,
    );
    for _ in 0..40 {
        rt.wheel(x, y, -120.);
        rt.frame(1. / 60.);
    }
    settle(&mut rt);
    assert!(
        rect(&rt, "step-title")[1] < viewport[1] - 1.,
        "the Agents heading scrolled out of view"
    );
    rt
}

/// A press-like step change.
fn press_go(rt: &mut Runtime<State>, step: Step) {
    rt.update(|s, tx| {
        tx.animated();
        super::go_to(s, step);
    });
}

/// Review P3: coming back to a step before its old body finished fading
/// opens it at its top, not at the offset the fading one still holds.
#[test]
fn a_quick_return_opens_the_step_at_its_top() {
    let mut rt = agents_scrolled_to_the_end();
    press_go(&mut rt, Step::Projects);
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    press_go(&mut rt, Step::Agents);
    settle(&mut rt);
    let viewport = rect(&rt, &scroll_key(&rt.state.onboarding));
    let title = rect(&rt, "step-title");
    assert!(
        inside(title, viewport),
        "the Agents heading {title:?} is in view in {viewport:?}"
    );
}

/// Review P3: the step fading out keeps its old, taller rect while the
/// card shrinks to the next one; none of its text shows over the footer
/// or the stepper.
#[test]
fn the_outgoing_step_never_draws_over_the_footer_or_header() {
    let mut rt = agents_scrolled_to_the_end();
    press_go(&mut rt, Step::Projects);
    let mut seen_exiting = false;
    for frame in 0..30 {
        rt.frame(1. / 60.);
        let header = rect(&rt, "onboarding-header");
        let footer = rect(&rt, "onboarding-footer");
        let elements = rt.elements();
        // Inside a step's scroll viewport (not the stepper's own motion).
        let in_a_step = |mut i: usize| loop {
            let e = &elements[i];
            if e.key
                .as_deref()
                .is_some_and(|k| k.starts_with("onboarding-scroll-"))
            {
                return true;
            }
            match e.parent {
                Some(p) => i = p,
                None => return false,
            }
        };
        for (i, e) in elements.iter().enumerate() {
            if e.text.is_none()
                || !e.exiting
                || e.opacity < 0.01
                || e.visible[2] <= 0.
                || e.visible[3] <= 0.
                || !in_a_step(i)
            {
                continue;
            }
            seen_exiting = true;
            assert!(
                e.visible[1] >= bottom(header) - 0.5 && bottom(e.visible) <= footer[1] + 0.5,
                "frame {frame}: outgoing {:?} {:?} shows at {:?}, outside the body between \
                 the header {header:?} and the footer {footer:?}",
                e.key,
                e.text,
                e.visible
            );
        }
    }
    assert!(seen_exiting, "the old step was seen fading out");
}
