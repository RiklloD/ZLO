//! The chat's authored motion actually plays: elements that change state
//! keep their identity (a keyed element moved under a new parent re-enters
//! and cuts its siblings' exits), and the springs this module asks for run
//! where they should and nowhere else. Driven through `rune::Runtime` over
//! the chat fixture scenes.

use rune::{Env, Key, KeyEvent, Runtime};

use super::fixtures::{self, test_state};
use std::rc::Rc;

use zlo_client::transcript::Row;

use super::{chat_for, sync_items};
use crate::state::State;

const DT: f64 = 1. / 60.;

struct Run {
    rt: Runtime<State>,
    t: f64,
}

impl Run {
    /// The window at 1440×900 with `scene` loaded, its first motion settled.
    fn new(scene: impl FnOnce(&mut State)) -> Self {
        let mut s = test_state();
        scene(&mut s);
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        rt.frame(0.);
        let mut run = Run { rt, t: 0. };
        run.frames(90);
        run
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.t += DT;
            self.rt.frame(DT);
        }
    }

    /// An animated change to the open chat, as a click's handler makes.
    fn change(&mut self, f: impl FnOnce(&mut super::ChatState) + 'static) {
        self.rt.update(move |s, tx| {
            if let Some(c) = s.chat.as_mut() {
                f(c);
            }
            tx.animated();
        });
    }

    /// The tree path of the live (not exiting) element keyed `key`.
    fn path(&self, key: &str) -> Option<String> {
        self.rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key) && !e.exiting)
            .map(|e| e.path)
    }

    fn info(&self, key: &str) -> Option<rune::ElementInfo> {
        self.rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
    }

    fn center(&self, key: &str) -> (f32, f32) {
        let (r, _) = self.rt.presented(key).expect("drawn");
        (r[0] + r[2] / 2., r[1] + r[3] / 2.)
    }

    fn type_text(&mut self, text: &str) {
        self.rt.key(KeyEvent {
            key: Key::Text(text.into()),
            mods: Default::default(),
        });
    }
}

#[test]
fn jump_to_latest_drops_away_instead_of_vanishing() {
    let mut run = Run::new(fixtures::chat);
    run.change(|c| c.far_from_bottom = true);
    run.frames(60);
    let rest = run.info("jump-latest").expect("the jump strip shows").rect[1];
    run.change(|c| c.far_from_bottom = false);
    run.frames(3);
    let leaving = run.info("jump-latest").expect("still drawn as it leaves");
    assert!(leaving.exiting, "it plays an exit");
    assert!(
        leaving.rect[1] > rest + 0.5,
        "the keyed strip itself carries the pop: it moves down as it fades ({} vs {rest})",
        leaving.rect[1]
    );
}

#[test]
fn opening_reject_feedback_keeps_the_gate_card() {
    let mut run = Run::new(fixtures::gate);
    let before = run.path("gate:1-gate").expect("the gate card");
    run.change(|c| {
        c.reject = Some(super::reject_feedback::RejectEditor::new(
            "g1".into(),
            "run-2".into(),
        ));
    });
    run.frames(2);
    assert!(run.path("g1-reject-editor").is_some(), "the field opened");
    assert_eq!(run.path("gate:1-gate"), Some(before.clone()), "same card");
    run.change(|c| c.reject = None);
    run.frames(2);
    assert_eq!(run.path("gate:1-gate"), Some(before), "still the same card");
    assert!(
        run.info("g1-reject-editor").is_some_and(|e| e.exiting),
        "the field plays its exit"
    );
}

#[test]
fn opening_a_step_keeps_its_line() {
    let mut run = Run::new(long_thought);
    run.change(|c| {
        c.toggles.insert("act-think:1".into(), true);
    });
    run.frames(30);
    let before = run.path("think:1-leaf").expect("the thought's line");
    assert!(
        run.path("think:1-detail").is_none(),
        "a finished thought is folded"
    );
    run.change(|c| {
        c.toggles.insert("leaf:think:1".into(), true);
    });
    run.frames(2);
    assert!(run.path("think:1-detail").is_some(), "it opened");
    assert_eq!(run.path("think:1-leaf"), Some(before.clone()), "same line");
    run.change(|c| {
        c.toggles.insert("leaf:think:1".into(), false);
    });
    run.frames(2);
    assert_eq!(
        run.path("think:1-leaf"),
        Some(before),
        "still the same line"
    );
    // Inside a growth row the row's own height carries the fold
    // (`fold_glide`): it glides shut rather than jumping.
    assert!(
        run.info("row:act-think:1").is_some_and(|e| e.moving),
        "the group folds shut on its spring"
    );
}

#[test]
fn send_and_stop_stay_one_group() {
    let mut run = Run::new(fixtures::running);
    let stop = run.path("round-stop").expect("Stop while the turn runs");
    let group = run.path("send-stop").expect("the group");
    assert!(run.path("round-send").is_some(), "a draft brings Send");
    run.change(|c| c.composer.replace_text(""));
    run.frames(2);
    assert_eq!(run.path("round-stop"), Some(stop.clone()), "Stop stays put");
    assert_eq!(run.path("send-stop"), Some(group));
    assert!(
        run.info("round-send").is_some_and(|e| e.exiting),
        "Send pops out rather than cutting"
    );
    run.frames(60);
    assert!(run.info("round-send").is_none());
    assert_eq!(run.path("round-stop"), Some(stop));
}

#[test]
fn the_first_keystroke_eases_send_into_its_plate_and_later_ones_land() {
    let mut run = Run::new(fixtures::chat);
    let (x, y) = run.center("composer-input");
    run.rt.pointer_down(x, y);
    run.rt.pointer_up(x, y);
    run.frames(30);
    let idle = run.info("round-send").expect("send").fill;
    run.type_text("a");
    run.frames(2);
    let mid = run.info("round-send").expect("send").fill;
    run.frames(60);
    let ready = run.info("round-send").expect("send").fill;
    assert_ne!(idle, ready, "the plate differs from the wash");
    assert!(
        mid != ready && mid != idle,
        "the first keystroke animates the fill ({mid:?})"
    );
    // A keystroke that changes nothing about Send keeps typing instant.
    run.type_text("b");
    run.frames(1);
    assert_eq!(run.info("round-send").expect("send").fill, ready);
    let text = run.rt.state.chat.as_ref().map(|c| c.composer.text.clone());
    assert_eq!(text.as_deref(), Some("ab"));
}

#[test]
fn the_activity_chevron_turns() {
    let mut run = Run::new(fixtures::chat);
    let head = run
        .rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref().is_some_and(|k| k.ends_with("-head")) && !e.exiting)
        .and_then(|e| e.key)
        .expect("an activity group");
    let group = head.trim_end_matches("-head").to_string();
    let open = run
        .rt
        .state
        .chat
        .as_ref()
        .is_some_and(|c| c.activity_open(&group, false));
    run.change(move |c| {
        c.toggles.insert(group, !open);
    });
    run.frames(3);
    let turning = run
        .rt
        .elements()
        .into_iter()
        .any(|e| e.key.as_deref() == Some("chevron") && e.path.contains(&head) && e.moving);
    assert!(turning, "the chevron springs between its turns");
}

/// The chat with its first thought long enough to open.
fn long_thought(s: &mut State) {
    fixtures::chat(s);
    let Some(c) = s.chat.as_mut() else { return };
    let mut rows = (*c.rows).clone();
    for r in rows.iter_mut() {
        if let Row::Thinking { key, text } = r
            && key == "think:1"
        {
            *text = "First the summary line, then the tree under it, then every row of the transcript in its place.".into();
        }
    }
    c.rows = Rc::new(rows);
    c.rows_gen += 1;
    let mut tx = rune::Tx::default();
    sync_items(s, "t1", &mut tx);
}

/// The open chat with four more prompts, each answered.
fn many_prompts(s: &mut State) {
    fixtures::chat(s);
    let Some(c) = s.chat.as_mut() else { return };
    let mut rows = (*c.rows).clone();
    for i in 0..4 {
        rows.push(Row::User {
            key: format!("msg:p{i}"),
            text: format!("prompt number {i}"),
            pending: false,
            attachments: Vec::new(),
            steer: None,
        });
        rows.push(Row::Assistant {
            key: format!("msg:a{i}"),
            text: format!("answer number {i}"),
            streaming: false,
        });
    }
    c.rows = Rc::new(rows);
    c.rows_gen += 1;
    let mut tx = rune::Tx::default();
    sync_items(s, "t1", &mut tx);
}

#[test]
fn one_outline_card_glides_along_the_rail() {
    let mut run = Run::new(many_prompts);
    assert!(run.path("outline").is_some(), "the rail shows");
    run.rt.update(|s, tx| {
        tx.animated();
        if let Some(c) = chat_for(s, "t1") {
            c.outline.hovered = Some(0);
        }
    });
    run.frames(30);
    let card = run.path("outline-card").expect("the card");
    let first = run.info("outline-card").expect("card").rect[1];
    run.rt.update(|s, tx| {
        tx.animated();
        if let Some(c) = chat_for(s, "t1") {
            c.outline.hovered = Some(1);
        }
    });
    run.frames(2);
    let gliding = run.info("outline-card").expect("card");
    assert_eq!(run.path("outline-card"), Some(card), "the same card");
    assert!(!gliding.exiting);
    run.frames(60);
    let second = run.info("outline-card").expect("card").rect[1];

    assert!(second > first, "it moved down to the next tick");
    assert!(
        gliding.rect[1] > first && gliding.rect[1] < second,
        "it glides there ({first} → {} → {second})",
        gliding.rect[1]
    );
    // Ticks near the pointer lift on their own spring.
    let ticks_moving = run
        .rt
        .elements()
        .iter()
        .any(|e| e.key.as_deref() == Some("tick") && e.moving);
    assert!(!ticks_moving, "at rest nothing moves");
    run.rt.update(|s, tx| {
        tx.animated();
        if let Some(c) = chat_for(s, "t1") {
            c.outline.hovered = None;
        }
    });
    run.frames(2);
    let ticks_moving = run
        .rt
        .elements()
        .iter()
        .any(|e| e.key.as_deref() == Some("tick") && e.moving);
    assert!(ticks_moving, "the lift eases back down");
}
