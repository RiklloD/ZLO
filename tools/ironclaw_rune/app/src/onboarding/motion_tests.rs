//! Runtime checks for setup's motion: steps arrive from the side the
//! person is going, the stepper's highlight glides, the claw mark draws
//! itself in once, setup leaves with motion, and nothing moves at rest.

use rune::{App, AppSpec, Env, Runtime};

use super::stepper::PILL_KEY;
use super::{Finish, Step, fixtures};
use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

/// The app's real view over a fixture; replies go to an app that never runs.
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

fn body_key(step: Step) -> String {
    format!("onboarding-step-body-{step:?}")
}

/// Press-like: an input-caused (animated) commit going to `step`. Returns
/// the new body's x a few frames in, and where it rests.
fn arrive(rt: &mut Runtime<State>, step: Step) -> (f32, f32) {
    rt.update(|s, tx| {
        tx.animated();
        super::go_to(s, step);
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let mid = rect(rt, &body_key(step))[0];
    settle(rt);
    (mid, rect(rt, &body_key(step))[0])
}

#[test]
fn forward_arrives_from_the_right_and_back_from_the_left() {
    let mut rt = runtime(fixtures::agents);
    let (mid, rest) = arrive(&mut rt, Step::Projects);
    assert_eq!(rt.state.onboarding.direction, 1);
    assert!(
        mid > rest + 1.,
        "going forward, the step comes in from the right ({mid} → {rest})"
    );

    let mut rt = runtime(fixtures::agents);
    let (mid, rest) = arrive(&mut rt, Step::Connect);
    assert_eq!(rt.state.onboarding.direction, -1);
    assert!(
        mid < rest - 1.,
        "going back, it comes in from the left ({mid} → {rest})"
    );
}

#[test]
fn the_panel_springs_to_the_new_steps_size() {
    let mut rt = runtime(fixtures::agents);
    let start = rect(&rt, "onboarding-dialog")[3];
    rt.update(|s, tx| {
        tx.animated();
        super::go_to(s, Step::Projects);
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let mid = rect(&rt, "onboarding-dialog")[3];
    settle(&mut rt);
    let end = rect(&rt, "onboarding-dialog")[3];
    assert!((start - end).abs() > 20., "the steps differ in height");
    assert!(
        (mid - start).abs() > 0.5 && (mid - end).abs() > 0.5,
        "the height springs ({start} → {mid} → {end})"
    );
}

#[test]
fn the_stepper_highlight_glides_to_the_new_step() {
    let mut rt = runtime(fixtures::connect);
    let start = rect(&rt, PILL_KEY);
    let first = rect(&rt, "onboarding-step-0");
    assert!(
        (start[0] - first[0]).abs() < 0.5 && (start[2] - first[2]).abs() < 0.5,
        "at rest it covers the current step"
    );
    rt.update(|s, tx| {
        tx.animated();
        super::go_to(s, Step::Agents);
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let target = rect(&rt, "onboarding-step-1");
    let mid = rect(&rt, PILL_KEY);
    assert!(
        mid[0] > start[0] + 0.5 && mid[0] < target[0] - 0.5,
        "mid-flight ({} → {} → {})",
        start[0],
        mid[0],
        target[0]
    );
    settle(&mut rt);
    let end = rect(&rt, PILL_KEY);
    assert!((end[0] - target[0]).abs() < 0.5 && (end[2] - target[2]).abs() < 0.5);
}

#[test]
fn the_claw_mark_draws_in_once_and_never_again() {
    let mut rt = runtime(|s| {
        fixtures::connect(s);
        s.onboarding.shown_at = Some(std::time::Instant::now());
    });
    // `runtime` already ran 1.5 s of frames: the entrance played and holds
    // its resting pose.
    let opacity = |rt: &Runtime<State>| rt.presented("onboarding-mark-2").map(|(_, o)| o);
    assert_eq!(opacity(&rt), Some(1.));

    // A fresh mount right at first show starts hidden.
    let app = App::new(AppSpec::new("t", (1280., 820.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fixtures::connect(&mut s);
    s.onboarding.shown_at = Some(std::time::Instant::now());
    let mut fresh = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    fresh.frame(0.);
    fresh.frame(1. / 60.);
    let o = opacity(&fresh).unwrap_or(1.);
    assert!(o < 0.5, "the last stroke waits its turn ({o})");
    settle(&mut fresh);
    assert_eq!(opacity(&fresh), Some(1.));

    // Later renders (a step change, after the entrance window) keep it
    // whole on every frame.
    std::thread::sleep(super::stepper::MARK_ENTRANCE);
    rt.update(|s, tx| {
        tx.animated();
        super::go_to(s, Step::Agents);
    });
    for _ in 0..30 {
        rt.frame(1. / 60.);
        assert_eq!(opacity(&rt), Some(1.), "the mark never replays");
    }
}

#[test]
fn no_frames_are_drawn_at_rest_on_any_step() {
    for (name, fill) in [
        ("connect", fixtures::connect as fn(&mut State)),
        ("pairing", fixtures::pairing),
        ("agents", fixtures::agents),
        ("providers", fixtures::providers),
        ("projects", fixtures::projects),
        ("toast", fixtures::toast),
    ] {
        let mut rt = runtime(fill);
        assert!(!rt.frame(1. / 60.), "{name} asks for frames at rest");
    }
}

#[test]
fn closing_setup_leaves_with_motion() {
    let mut rt = runtime(fixtures::agents);
    rt.update(|s, tx| {
        tx.animated();
        super::finish(
            s,
            Finish {
                imported: 3,
                warning: None,
            },
        );
    });
    assert!(!super::wizard_shown(&rt.state));
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let (_, dialog) = rt
        .presented("onboarding-dialog")
        .expect("the dialog is still leaving");
    assert!(dialog > 0. && dialog < 1., "it fades out ({dialog})");
    let (_, scrim) = rt.presented("onboarding-scrim").expect("scrim leaving");
    assert!(scrim > 0. && scrim < 1., "the scrim fades too ({scrim})");
    let (_, toast) = rt
        .presented("onboarding-toast-layer")
        .expect("the note is mounted");
    assert!(toast < 0.5, "the note waits for the wizard ({toast})");
    settle(&mut rt);
    assert!(rt.presented("onboarding-dialog").is_none());
    let (_, toast) = rt.presented("onboarding-toast-layer").expect("note");
    assert!((toast - 1.).abs() < 1e-3, "then rises in ({toast})");
}
