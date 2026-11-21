//! Motion on the pages of round 3 builder D (Automations, Memory, Files,
//! Skills, Projects, Notifications, Computers, Hooks, plugin surfaces):
//! the person's own changes move (a row keeps its identity and glides, a
//! form wipes open, a selection pill glides), and every page draws nothing
//! at rest. Driven through `rune::Runtime` over the pages' fixture scenes.

use rune::{ElementInfo, Env, Runtime};

use crate::state::State;

pub(crate) const DT: f64 = 1. / 60.;

/// The fixture scene `name` in a 1280×820 runtime, settled.
pub(crate) fn scene(name: &str) -> Runtime<State> {
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

pub(crate) fn settle(rt: &mut Runtime<State>) {
    for _ in 0..120 {
        rt.frame(DT);
    }
}

pub(crate) fn frames(rt: &mut Runtime<State>, n: usize) {
    for _ in 0..n {
        rt.frame(DT);
    }
}

/// Frames are asked for after the page settled: none means it is idle.
pub(crate) fn at_rest(rt: &mut Runtime<State>) -> bool {
    !rt.frame(DT) && rt.wake_deadline().is_none()
}

/// The live (not leaving) element whose own key is `key`.
pub(crate) fn live(rt: &Runtime<State>, key: &str) -> Option<ElementInfo> {
    rt.elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(key))
}

/// Click the live element keyed `key` whose path also contains `within`.
pub(crate) fn click_in(rt: &mut Runtime<State>, within: &str, key: &str) {
    let e = rt
        .elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(key) && e.path.contains(within))
        .unwrap_or_else(|| panic!("{key} in {within} is drawn"));
    let [x, y, w, h] = e.rect;
    let (x, y) = (x + w / 2., y + h / 2.);
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
}

pub(crate) fn click(rt: &mut Runtime<State>, key: &str) {
    click_in(rt, "", key);
}

/// `mid` sits strictly between `from` and `to` (by more than a hair).
pub(crate) fn between(mid: f32, from: f32, to: f32) -> bool {
    let (lo, hi) = if from < to { (from, to) } else { (to, from) };
    mid > lo + 0.25 && mid < hi - 0.25
}

#[test]
fn every_page_draws_nothing_at_rest() {
    for name in [
        "automations",
        "automation-form",
        "memory",
        "memory-draft",
        "memory-supermemory",
        "files",
        "skills",
        "skills-editor",
        "projects",
        "notifications",
        "hooks",
        "computers",
        "computers-pairing",
        "computers-remote",
        "plugin-list",
        "plugin-settings",
        "plugin-panel",
    ] {
        let mut rt = scene(name);
        assert!(at_rest(&mut rt), "{name} draws nothing at rest");
    }
}

#[test]
fn a_project_card_keeps_its_identity_and_glides_when_one_above_it_goes() {
    let mut rt = scene("projects");
    let before = live(&rt, "project-p2").expect("Tax return card");
    // The reply to the person's Delete (the gateway is not asked in a
    // fixture): it removes the first card in an animated commit.
    rt.update(|s, tx| {
        tx.animated();
        s.pages.projects.items.retain(|p| p.project_id != "p1");
    });
    frames(&mut rt, 3);
    let mid = live(&rt, "project-p2").expect("still there");
    assert_eq!(mid.path, before.path, "the same card, not a rebuilt one");
    assert!(
        mid.target[1] < before.rect[1] - 1.,
        "it has a new place above"
    );
    assert!(
        between(mid.rect[1], before.rect[1], mid.target[1]),
        "seen gliding: {} between {} and {}",
        mid.rect[1],
        before.rect[1],
        mid.target[1]
    );
    settle(&mut rt);
    assert!(live(&rt, "project-p1").is_none(), "the deleted card left");
    assert!(at_rest(&mut rt));
}

#[test]
fn archived_projects_wipe_in_when_the_person_shows_them() {
    let mut rt = scene("projects");
    assert!(
        live(&rt, "project-p3").is_none(),
        "archived ones start hidden"
    );
    click(&mut rt, "show-archived");
    frames(&mut rt, 2);
    let card = live(&rt, "project-p3").expect("the archived card mounted");
    assert!(
        card.opacity < 0.99,
        "it arrives, it does not pop: {}",
        card.opacity
    );
    settle(&mut rt);
    assert!(live(&rt, "project-p3").is_some_and(|c| c.opacity > 0.99));
    assert!(at_rest(&mut rt));
}

#[test]
fn archiving_a_notification_folds_it_away_and_the_rest_glide_up() {
    let mut rt = scene("notifications");
    let before = live(&rt, "notification-n2").expect("second row");
    click_in(&mut rt, "notification-n1", "archive");
    frames(&mut rt, 3);
    assert!(
        rt.elements()
            .iter()
            .any(|e| e.exiting && e.key.as_deref() == Some("notification-n1")),
        "the archived row plays its exit"
    );
    let mid = live(&rt, "notification-n2").expect("still there");
    assert_eq!(mid.path, before.path, "the same row");
    assert!(mid.target[1] < before.rect[1] - 1., "it moves up");
    assert!(
        between(mid.rect[1], before.rect[1], mid.target[1]),
        "seen gliding: {} between {} and {}",
        mid.rect[1],
        before.rect[1],
        mid.target[1]
    );
    settle(&mut rt);
    assert!(at_rest(&mut rt));
}

#[test]
fn the_new_automation_form_wipes_open_on_the_press() {
    let mut rt = scene("automations");
    click(&mut rt, "new-automation");
    frames(&mut rt, 2);
    let form = live(&rt, "automation-form-box").expect("the form mounted");
    assert!(form.opacity < 0.99, "it wipes in: {}", form.opacity);
    settle(&mut rt);
    assert!(at_rest(&mut rt));
}

#[test]
fn the_files_pill_glides_to_the_file_the_person_picks() {
    let mut rt = scene("files");
    let before = live(&rt, "file-pill").expect("a file is selected");
    click(&mut rt, "entry-threads/daily-2026-11-06.md");
    frames(&mut rt, 3);
    let mid = live(&rt, "file-pill").expect("one pill");
    assert_eq!(mid.path, before.path, "the same pill moves");
    assert!(mid.target[1] > before.rect[1] + 1., "its new row is below");
    assert!(
        between(mid.rect[1], before.rect[1], mid.target[1]),
        "seen gliding: {} between {} and {}",
        mid.rect[1],
        before.rect[1],
        mid.target[1]
    );
    settle(&mut rt);
    let row = live(&rt, "entry-threads/daily-2026-11-06.md").expect("row");
    let end = live(&rt, "file-pill").expect("pill");
    assert!(
        (end.rect[1] - row.rect[1]).abs() < 0.5,
        "it lands on its row"
    );
    assert!(
        (end.rect[3] - row.rect[3]).abs() < 0.5,
        "as tall as the row"
    );
    // The preview's spinner (a fixture never answers) is the one loop left.
}
