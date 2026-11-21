//! Operator motion: tab panels crossfade in place, the log level knob and
//! the inspector's thread pill glide, a confirm row wipes open, and every
//! tab draws nothing at rest (the logs stream included).

use super::{Tab, fixtures};
use crate::pages::providers::motion_tests::{
    act, assert_at_rest, between, frames, opacity, rect, runtime, runtime_sized,
};
use crate::state::State;

fn close(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.5)
}

#[test]
fn switching_tabs_crossfades_the_panels_in_place() {
    let mut rt = runtime(fixtures::status);
    let status = rect(&rt, "operator-panel-Status");
    act(&mut rt, |s| super::show(s, Tab::Config));
    frames(&mut rt, 3);
    let config = opacity(&rt, "operator-panel-Config");
    assert!(config > 0. && config < 1., "Config fades in: {config}");
    assert!(
        rt.presented("operator-panel-Status").is_some(),
        "Status fades out meanwhile"
    );
    assert!(
        (rect(&rt, "operator-panel-Config")[1] - status[1]).abs() < 0.5,
        "in Status's place, not under it"
    );
    frames(&mut rt, 60);
    assert!(rt.presented("operator-panel-Status").is_none());
}

#[test]
fn the_inspector_pill_sits_on_the_picked_thread_and_glides() {
    let mut rt = runtime(fixtures::inspector);
    frames(&mut rt, 2);
    let pill = rect(&rt, "inspect-thread-pill");
    assert!(
        close(pill, rect(&rt, "inspect-thread-t1")),
        "the pill covers the picked row: {pill:?}"
    );
    act(&mut rt, |s| {
        if let Some(p) = s.pages.operator.inspector.as_mut() {
            p.thread = Some("t2".into());
        }
    });
    frames(&mut rt, 3);
    let mid = rect(&rt, "inspect-thread-pill");
    frames(&mut rt, 60);
    let to = rect(&rt, "inspect-thread-pill");
    assert!(close(to, rect(&rt, "inspect-thread-t2")), "it lands on t2");
    assert!(
        between(pill, mid, to, 1),
        "it glides: {pill:?} → {mid:?} → {to:?}"
    );
}

#[test]
fn the_stop_confirm_wipes_open_and_away() {
    // Tall enough to show the Service card (Rune lands motion off screen).
    let mut rt = runtime_sized(fixtures::status, (1280., 2400.));
    // The fixture shows the confirm; Cancel folds it away.
    act(&mut rt, |s| s.pages.operator.status.confirm_stop = false);
    frames(&mut rt, 2);
    let mid = opacity(&rt, "confirm-stop");
    assert!(mid > 0. && mid < 1., "it fades as it folds: {mid}");
    frames(&mut rt, 60);
    assert!(rt.presented("confirm-stop").is_none());
    // Stop service asks again: it wipes back open.
    act(&mut rt, |s| s.pages.operator.status.confirm_stop = true);
    frames(&mut rt, 2);
    let mid = opacity(&rt, "confirm-stop");
    assert!(mid > 0. && mid < 1., "it fades in as it opens: {mid}");
    frames(&mut rt, 60);
    assert!((opacity(&rt, "confirm-stop") - 1.).abs() < 1e-3);
}

#[test]
fn every_operator_tab_draws_nothing_at_rest() {
    for fill in [
        fixtures::status as fn(&mut State),
        fixtures::setup,
        fixtures::config,
        fixtures::extension_config,
        fixtures::logs,
        fixtures::inspector,
    ] {
        let mut rt = runtime(fill);
        assert_at_rest(&mut rt, "Operator");
    }
}
