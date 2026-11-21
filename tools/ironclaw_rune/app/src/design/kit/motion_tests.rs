//! Runtime checks for the kit's motion: the spinner's energy, the
//! segmented knob's glide and the chevron's turn.

use std::cell::Cell;

use rune::{App, AppSpec, Cx, El, Env, Runtime, col};

use super::{Turn, chevron, segmented};
use crate::design::prefs::Prefs;
use crate::state::State;

thread_local! {
    static PICKED: Cell<usize> = const { Cell::new(0) };
    static OPEN: Cell<bool> = const { Cell::new(false) };
    static REDUCED: Cell<bool> = const { Cell::new(false) };
}

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

fn runtime(view: fn(&State, &mut Cx<State>) -> El) -> Runtime<State> {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, view));
    let s = State::new(crate::jobs::Rt::new(app.handle()));
    let mut rt = Runtime::new(s, view, (400., 300.), Env::new(true, 1.));
    rt.frame(0.);
    rt
}

/// Frames a host draws over `secs`: every frame while the runtime asks for
/// continuous frames, else one at each wake deadline.
fn frames_over(rt: &mut Runtime<State>, secs: f64) -> usize {
    let (mut t, mut frames, mut continuous) = (0., 0, rt.frame(0.));
    while t < secs {
        let dt = if continuous {
            1. / 120.
        } else {
            match rt.wake_deadline() {
                Some(due) => (due - t).max(1. / 120.),
                None => break,
            }
        };
        t += dt;
        continuous = rt.frame(dt);
        frames += 1;
    }
    frames
}

fn spinner_view(s: &State, cx: &mut Cx<State>) -> El {
    cx.reduced_motion(REDUCED.get());
    let k = crate::design::kit(&s.palette);
    col().child(super::Spinner::new().size(16.).build(&k))
}

/// One cell as the spinner drew it before (`LoopSpec::opacity`).
fn old_cell_view(_: &State, _: &mut Cx<State>) -> El {
    col().child(
        rune::el()
            .key("old")
            .size(4., 4.)
            .fill(rune::Color::WHITE)
            .looping(rune::LoopSpec::opacity(1.37, 0., 0.4, 1.)),
    )
}

#[test]
fn the_spinner_draws_at_15_fps_and_rests_under_reduced_motion() {
    let mut rt = runtime(spinner_view);
    assert!(
        !rt.frame(1. / 120.),
        "the spinner never asks for display-rate frames"
    );
    let frames = frames_over(&mut rt, 2.);
    eprintln!("spinner frames over 2 s: {frames}");
    // Twelve `LoopSpec::opacity` cells drew 240 frames in these 2 s on a
    // 120 Hz display; the cycles share one 15 fps clock.
    assert!(
        (20..=34).contains(&frames),
        "a spinner on screen for 2 s drew {frames} frames"
    );

    // The measure tells the two apart: one cell of the old spinner.
    let mut old = runtime(old_cell_view);
    let old_frames = frames_over(&mut old, 2.);
    assert!(
        old_frames >= 200,
        "a LoopSpec::opacity cell drew {old_frames}"
    );

    REDUCED.set(true);
    let mut rt = runtime(spinner_view);
    rt.frame(1. / 60.);
    assert_eq!(rt.wake_deadline(), None, "reduced motion holds it still");
    REDUCED.set(false);
}

fn segmented_view(s: &State, _: &mut Cx<State>) -> El {
    col().child(segmented(
        s,
        "seg",
        &["On", "Accounts and keys"],
        PICKED.get(),
        false,
        |_, _| {},
    ))
}

#[test]
fn the_segmented_knob_glides_and_lands_on_a_wider_segment() {
    PICKED.set(0);
    let mut rt = runtime(segmented_view);
    rt.frame(1. / 60.);
    let rect = |rt: &Runtime<State>, key: &str| rt.presented(key).map(|(r, _)| r);
    let start = rect(&rt, "seg-knob").expect("knob");
    let first = rect(&rt, "seg-seg-0").expect("segment 0");
    assert!((start[0] - first[0]).abs() < 0.5 && (start[2] - first[2]).abs() < 0.5);

    rt.update(|_, tx| {
        tx.animated();
        PICKED.set(1);
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let target = rect(&rt, "seg-seg-1").expect("segment 1");
    let mid = rect(&rt, "seg-knob").expect("knob");
    assert!(
        mid[0] > start[0] + 0.5 && mid[0] < target[0] - 0.5,
        "the knob glides ({} → {} → {})",
        start[0],
        mid[0],
        target[0]
    );
    for _ in 0..60 {
        rt.frame(1. / 60.);
    }
    let end = rect(&rt, "seg-knob").expect("knob");
    assert!((end[0] - target[0]).abs() < 0.5, "lands on the segment");
    assert!(
        (end[2] - target[2]).abs() < 0.5 && end[2] > start[2],
        "and takes its width"
    );
    PICKED.set(0);
}

fn chevron_view(s: &State, _: &mut Cx<State>) -> El {
    col().child(chevron(
        "chev",
        OPEN.get(),
        Turn::Disclose,
        14.,
        s.palette.text,
    ))
}

#[test]
fn a_chevron_turns_open_on_a_spring() {
    OPEN.set(false);
    let mut rt = runtime(chevron_view);
    rt.frame(1. / 60.);
    let turn = |rt: &Runtime<State>| rt.presented_pose("chev").map(|p| p.rotation());
    let shut = turn(&rt).expect("pose");
    assert!(
        (shut + std::f32::consts::FRAC_PI_2).abs() < 1e-3,
        "points right"
    );

    rt.update(|_, tx| {
        tx.animated();
        OPEN.set(true);
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let mid = turn(&rt).expect("pose");
    assert!(mid > shut + 0.01 && mid < -0.01, "mid-turn at {mid}");
    for _ in 0..60 {
        rt.frame(1. / 60.);
    }
    assert!(turn(&rt).expect("pose").abs() < 1e-3, "points down");
    OPEN.set(false);
}

#[test]
fn default_prefs_keep_the_style_springs() {
    // `motion::HOVER` / `SLIDE` are the style's: palette-only helpers and
    // `Style` move together.
    let st = crate::design::styles::Style::of(&Prefs::default());
    assert_eq!(st.slide, crate::design::motion::SLIDE);
}
