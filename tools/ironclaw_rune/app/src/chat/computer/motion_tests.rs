//! The screen share's motion: the deck re-fans on a click, the lightbox
//! zooms out of its card and back, frames streaming in land at once, and
//! nothing draws at rest.

use std::time::{Duration, Instant};

use rune::{App, AppSpec, Env, Runtime, Tx};

use super::{FRAME, TRAIL, TRAIL_FADE, click_card, close_big, trail_alpha};
use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

fn share_poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    super::poll(s, "t1", tx)
}

/// The `computer` scene in the app's real view, with only the share's
/// poll (so "at rest" means the share's own wakes).
fn runtime() -> Runtime<State> {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1280., 820.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    super::scene(&mut s, false);
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.poll = Some(share_poll);
    rt.frame(0.);
    settle(&mut rt);
    rt
}

/// Frames until nothing animates; how many it took.
fn settle(rt: &mut Runtime<State>) -> usize {
    for n in 0..600 {
        if !rt.frame(1. / 120.) {
            return n;
        }
    }
    panic!("still animating after 5 s");
}

fn rect(rt: &Runtime<State>, key: &str) -> Option<[f32; 4]> {
    rt.presented(key).map(|(r, _)| r)
}

fn near(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.75)
}

fn share(rt: &mut Runtime<State>) -> &mut super::ComputerState {
    &mut rt.state.chat.as_mut().expect("chat").computer
}

/// At rest the only clock is the status dot's 15 fps breath while the agent
/// works or waits: never a display-rate frame.
fn resting(rt: &mut Runtime<State>) -> bool {
    !rt.frame(1. / 60.)
        && rt
            .wake_deadline()
            .is_none_or(|due| due - rt_now(rt) >= 1. / 20.)
}

/// The runtime clock, read back through a zero step.
fn rt_now(rt: &mut Runtime<State>) -> f64 {
    let mut now = 0.;
    rt.update(|_, tx| now = tx.clock());
    rt.frame(0.);
    now
}

#[test]
fn the_share_draws_nothing_at_rest() {
    let mut rt = runtime();
    assert!(resting(&mut rt), "only the working dot's breath");
    // The run ended: nothing breathes, nothing wakes.
    if let Some(session) = share(&mut rt).session.as_mut() {
        session.ended = true;
    }
    rt.invalidate();
    settle(&mut rt);
    assert!(!rt.frame(1. / 60.), "no animation frames");
    assert_eq!(rt.wake_deadline(), None, "and no wakes");
}

#[test]
fn a_rear_card_click_refans_the_deck_with_motion() {
    let mut rt = runtime();
    let front = rect(&rt, "cu-card-11").expect("front card");
    let rear = rect(&rt, "cu-card-12").expect("second card");
    assert!(rear[2] < front[2], "the rear card is smaller");

    rt.update(|s, tx| {
        tx.animated();
        click_card(s, "t1", 12, false);
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let (mid, opacity) = rt
        .presented("cu-card-12")
        .expect("the same card, mid-shuffle");
    assert!(
        (opacity - 1.).abs() < 0.01,
        "it moves, it does not remount and fade in: {opacity}"
    );
    assert!(
        mid[2] > rear[2] + 1. && mid[2] < front[2] - 1.,
        "it grows toward the front: {rear:?} -> {mid:?} -> {front:?}"
    );
    let frames = settle(&mut rt);
    assert!(
        (10..=90).contains(&frames),
        "a calm spring: {frames} frames"
    );
    let landed = rect(&rt, "cu-card-12").expect("card 12");
    assert!(
        near(landed, front),
        "it lands in the front slot: {landed:?} vs {front:?}"
    );
    assert!(
        rect(&rt, "cu-card-11").is_some_and(|r| r[2] < front[2]),
        "and the old front card stepped back, same key"
    );
    assert!(resting(&mut rt), "then rests");
}

#[test]
fn the_lightbox_zooms_out_of_its_card_and_back() {
    let mut rt = runtime();
    let card = rect(&rt, "cu-card-11").expect("front card");

    rt.update(|s, tx| {
        tx.animated();
        click_card(s, "t1", 11, true);
    });
    rt.frame(1. / 60.);
    assert!(
        rect(&rt, "cu-card-11").is_none(),
        "the deck hands its card to the lightbox"
    );
    rt.frame(1. / 60.);
    let mid = rect(&rt, "cu-big").expect("the enlarged card");
    let (_, veil) = rt.presented("cu-big-veil").expect("the veil");
    assert!(veil < 0.99, "the veil fades in: {veil}");
    settle(&mut rt);
    let open = rect(&rt, "cu-big").expect("the enlarged card");
    assert!(
        mid[2] > card[2] + 1. && mid[2] < open[2] - 1.,
        "it grows out of the card: {card:?} -> {mid:?} -> {open:?}"
    );
    assert!(open[2] > 900., "to the lightbox: {open:?}");

    rt.update(|s, tx| {
        tx.animated();
        close_big(s, "t1");
    });
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    let back = rect(&rt, "cu-card-11").expect("the card flies home");
    assert!(
        back[2] > card[2] + 1. && back[2] < open[2] - 1.,
        "shrinking back into the deck: {back:?}"
    );
    settle(&mut rt);
    let home = rect(&rt, "cu-card-11").expect("front card");
    assert!(near(home, card), "home: {home:?} vs {card:?}");
    assert!(rect(&rt, "cu-big").is_none() && rect(&rt, "cu-big-veil").is_none());
}

/// A frame the fetch loop lands (a new picture, even of another shape)
/// never moves, resizes or remounts its card, and never opens or closes
/// the lightbox. (Whether its commit animates at all is the window's call:
/// a headless `Runtime` animates every commit, while a window lands those
/// no input caused at once, and the fetch loop never asks for motion.)
#[test]
fn frames_streaming_in_leave_the_deck_alone() {
    let mut rt = runtime();
    let before = rect(&rt, "cu-card-11").expect("front card");
    let rear = rect(&rt, "cu-card-12").expect("second card");
    rt.update(|s, _| {
        if let Some(card) = s.chat.as_mut().and_then(|c| c.computer.cards.get_mut(&11)) {
            card.natural = (900, 900);
            card.pixels.set(4, 4, [90u8; 64].to_vec());
        }
    });
    for _ in 0..30 {
        rt.frame(1. / 120.);
        let (now, opacity) = rt.presented("cu-card-11").expect("the same card");
        assert!(
            near(now, before) && opacity > 0.99,
            "the card holds still: {now:?}"
        );
        assert!(near(rect(&rt, "cu-card-12").expect("card 12"), rear));
        assert!(rect(&rt, "cu-big").is_none());
    }
}

#[test]
fn the_drag_trail_fades_out_without_a_standing_timer() {
    assert_eq!(trail_alpha(Duration::ZERO), 1.);
    assert_eq!(trail_alpha(TRAIL - TRAIL_FADE), 1.);
    let half = trail_alpha(TRAIL - TRAIL_FADE / 2);
    assert!((half - 0.5).abs() < 0.01, "{half}");
    assert_eq!(trail_alpha(TRAIL), 0.);

    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    super::scene(&mut s, false);
    let set_trail = |s: &mut State, age: Duration| {
        if let Some(card) = s.chat.as_mut().and_then(|c| c.computer.cards.get_mut(&11)) {
            let drawn = Instant::now().checked_sub(age).expect("a past instant");
            card.trail = Some(((0.1, 0.1), (0.5, 0.5), drawn));
        }
    };
    let wakes = |tx: &Tx| {
        let d = format!("{tx:?}");
        d[d.find("wakes: [").map_or(0, |i| i + 8)..]
            .split(']')
            .next()
            .unwrap_or("")
            .to_string()
    };

    // A fresh trail stands still: one wake when its fade begins.
    set_trail(&mut s, Duration::ZERO);
    let mut tx = Tx::default();
    assert!(!super::poll(&mut s, "t1", &mut tx), "nothing to redraw yet");
    let w = wakes(&tx);
    assert!(
        w.starts_with("1.3") || w.starts_with("1.4"),
        "one wake ~1.4 s on: {w}"
    );

    // Fading: a frame at a time.
    set_trail(&mut s, TRAIL - TRAIL_FADE / 2);
    let mut tx = Tx::default();
    assert!(super::poll(&mut s, "t1", &mut tx));
    assert_eq!(wakes(&tx), format!("{FRAME:?}"));

    // Gone: one last redraw to clear it, then no wake.
    set_trail(&mut s, TRAIL + Duration::from_millis(10));
    let mut tx = Tx::default();
    assert!(
        super::poll(&mut s, "t1", &mut tx),
        "the expired trail is cleared"
    );
    assert_eq!(wakes(&tx), "");
    let mut tx = Tx::default();
    assert!(!super::poll(&mut s, "t1", &mut tx));
    assert_eq!(wakes(&tx), "");
}
