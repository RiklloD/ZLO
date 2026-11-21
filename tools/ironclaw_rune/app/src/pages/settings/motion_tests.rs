//! Settings → Appearance's authored motion plays: the accent ring glides
//! between swatches after a pick instead of jumping, the palette morphs, and
//! the page draws no frames at rest. Driven through `rune::Runtime` over the
//! `appearance` scene.

use rune::{Env, Runtime};

use crate::design::motion;
use crate::state::State;

const DT: f64 = 1. / 60.;

fn scene(name: &str) -> Runtime<State> {
    let mut s = crate::chat::fixtures::test_state();
    let fill = crate::scenes::SCENES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| *f)
        .expect("scene exists");
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    settle(&mut rt);
    rt
}

fn settle(rt: &mut Runtime<State>) {
    for _ in 0..120 {
        rt.frame(DT);
    }
}

fn at_rest(rt: &mut Runtime<State>) -> bool {
    !rt.frame(DT) && rt.wake_deadline().is_none()
}

fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
    rt.presented(key).map(|(r, _)| r).expect(key)
}

#[test]
fn the_accent_ring_glides_to_a_picked_swatch() {
    let mut rt = scene("appearance");
    assert!(at_rest(&mut rt), "Appearance draws nothing at rest");
    let start = rect(&rt, "accent-ring");
    let theme = rect(&rt, "accent-theme");
    assert!(
        (start[0] - theme[0]).abs() < 0.5,
        "the ring sits on the theme default"
    );

    // What the swatch's press does, without `update_prefs` writing the
    // person's preferences file from a test.
    rt.update(|s, tx| {
        motion::color_morph(tx);
        s.prefs.accent = "green".into();
        s.palette = crate::design::resolve(&s.prefs, s.system_dark);
    });
    rt.frame(DT);
    rt.frame(DT);
    let target = rect(&rt, "accent-green");
    let mid = rect(&rt, "accent-ring");
    assert!(
        mid[0] > start[0] + 0.5 && mid[0] < target[0] - 0.5,
        "the ring is mid-flight ({} → {} → {})",
        start[0],
        mid[0],
        target[0]
    );
    settle(&mut rt);
    let end = rect(&rt, "accent-ring");
    assert!((end[0] - target[0]).abs() < 0.5, "and lands on the swatch");
    assert!(at_rest(&mut rt), "and the page rests again");
}

#[test]
fn the_scheme_edge_sits_on_the_picked_card_and_glides() {
    let mut rt = scene("appearance");
    let card = |rt: &Runtime<State>, id: &str| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("card") && e.path.contains(id))
            .map(|e| e.rect)
            .expect("card")
    };
    let dark = card(&rt, "scheme-dark");
    let ring = rect(&rt, "scheme-ring");
    for i in 0..4 {
        assert!(
            (ring[i] - dark[i]).abs() < 0.5,
            "edge on the Dark card: {ring:?} vs {dark:?}"
        );
    }
    rt.update(|s, tx| {
        motion::color_morph(tx);
        s.prefs.scheme = "light".into();
        s.palette = crate::design::resolve(&s.prefs, s.system_dark);
    });
    rt.frame(DT);
    rt.frame(DT);
    let light = card(&rt, "scheme-light");
    let mid = rect(&rt, "scheme-ring");
    assert!(
        mid[0] < ring[0] - 0.5 && mid[0] > light[0] + 0.5,
        "the edge glides ({} → {} → {})",
        ring[0],
        mid[0],
        light[0]
    );
    settle(&mut rt);
    let end = rect(&rt, "scheme-ring");
    assert!((end[0] - light[0]).abs() < 0.5 && (end[2] - light[2]).abs() < 0.5);
    assert!(at_rest(&mut rt));
}

#[test]
fn permissions_and_traces_draw_nothing_at_rest() {
    for name in ["permissions", "traces", "settings"] {
        let mut rt = scene(name);
        assert!(at_rest(&mut rt), "{name} draws frames at rest");
    }
}
