//! The Usage page's authored motion plays: a daily bar keeps its identity
//! from Cost to Tokens and morphs to its new height, and the page draws no
//! frames at rest. Driven through `rune::Runtime` over the `usage-history`
//! scene, with real clicks on the metric switch.

use rune::{ElementInfo, Env, Runtime};

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

/// The live bar segment for `provider` in the column of `day`.
fn segment(rt: &Runtime<State>, day: &str, provider: &str) -> Option<ElementInfo> {
    let column = format!("usage-day-{day}");
    rt.elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(provider) && e.path.contains(&column))
}

fn click(rt: &mut Runtime<State>, key: &str) {
    let (r, _) = rt.presented(key).expect("drawn");
    let (x, y) = (r[0] + r[2] / 2., r[1] + r[3] / 2.);
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
}

/// Frames are asked for after the page settled: none means it is idle.
fn at_rest(rt: &mut Runtime<State>) -> bool {
    !rt.frame(DT) && rt.wake_deadline().is_none()
}

#[test]
fn a_bar_keeps_its_identity_from_cost_to_tokens_and_morphs() {
    let mut rt = scene("usage-history");
    assert!(at_rest(&mut rt), "the Cost view draws nothing at rest");
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let before = segment(&rt, &today, "claude_cli").expect("today's Claude segment");

    click(&mut rt, "usage-metric-seg-2");
    rt.frame(DT);
    rt.frame(DT);
    rt.frame(DT);
    let mid = segment(&rt, &today, "claude_cli").expect("still there");
    assert_eq!(
        mid.path, before.path,
        "the segment is the same element under Tokens, not a rebuilt one"
    );
    assert!(
        (mid.target[3] - before.rect[3]).abs() > 1.,
        "Tokens gives the segment a new height ({} → {})",
        before.rect[3],
        mid.target[3]
    );
    let (lo, hi) = if before.rect[3] < mid.target[3] {
        (before.rect[3], mid.target[3])
    } else {
        (mid.target[3], before.rect[3])
    };
    assert!(
        mid.rect[3] > lo + 0.25 && mid.rect[3] < hi - 0.25,
        "seen mid-morph: {} between {lo} and {hi}",
        mid.rect[3]
    );
    assert!(mid.opacity > 0.99, "it morphs, it does not fade back in");

    settle(&mut rt);
    let end = segment(&rt, &today, "claude_cli").expect("still there");
    assert!((end.rect[3] - end.target[3]).abs() < 0.5, "lands");
    assert_eq!(end.path, before.path);
    assert!(at_rest(&mut rt), "the Tokens view draws nothing at rest");
}

#[test]
fn the_limits_view_draws_nothing_at_rest() {
    let mut rt = scene("usage");
    assert!(at_rest(&mut rt));
}
