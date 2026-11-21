use super::*;
use super::{WireState, apply, fixture};
use crate::chat::fixtures::{close_side_chat, test_state, with_side_chat};
use crate::chat::{chat_for, chat_ref};
use crate::jobs::Generation;

fn wire() -> WireState {
    WireState {
        enabled: false,
        now: 0,
        sessions: fixture("sc2").session.clone().into_iter().collect(),
    }
}

#[test]
fn a_poll_lands_in_its_own_chat_and_is_dropped_once_that_chat_closed() {
    let mut s = test_state();
    with_side_chat(&mut s, "sc2");
    if let Some(c) = chat_for(&mut s, "sc2") {
        c.computer.thread_id = "sc2".into();
    }
    apply(&mut s, "sc2", Generation::default(), wire());
    assert!(chat_ref(&s, "sc2").is_some_and(|c| c.computer.session.is_some()));
    assert!(
        chat_ref(&s, "t1").is_some_and(|c| c.computer.session.is_none()),
        "the open chat's share is its own"
    );
    close_side_chat(&mut s);
    apply(&mut s, "sc2", Generation::default(), wire());
    assert!(chat_ref(&s, "t1").is_some_and(|c| c.computer.session.is_none()));
}

fn open_chat(s: &mut State, id: &str) {
    s.chat = Some(crate::chat::ChatState::new(id.into()));
}

#[test]
fn a_reopened_chat_does_not_match_the_old_polls() {
    let mut s = crate::chat::fixtures::test_state();
    open_chat(&mut s, "A");
    start(&mut s, "A");
    let old = slot(&mut s, "A").map(|cu| cu.generation).expect("share");
    open_chat(&mut s, "B");
    open_chat(&mut s, "A");
    start(&mut s, "A");
    let new = slot(&mut s, "A").map(|cu| cu.generation).expect("share");
    assert!(
        same(&mut s, "A", old).is_none(),
        "the old poll loop would run beside the new one"
    );
    assert!(same(&mut s, "A", new).is_some());
}

#[test]
fn a_take_over_landing_after_close_is_handed_back() {
    let mut s = crate::chat::fixtures::test_state();
    open_chat(&mut s, "A");
    start(&mut s, "A");
    let (generation, viewer) = slot(&mut s, "A")
        .map(|cu| (cu.generation, cu.viewer.clone()))
        .expect("share");
    // Take Over is pending; the chat closes (its Drop sees `mine == false`).
    open_chat(&mut s, "B");
    control_landed(
        &mut s,
        "A",
        generation,
        viewer.clone(),
        ComputerControl::TakeOver,
        Ok(()),
    );
    let sent = HANDED_BACK.with(|h| h.borrow_mut().drain(..).collect::<Vec<_>>());
    assert_eq!(sent, vec![("A".to_string(), viewer)]);

    // A failed one, or a hand back, needs nothing.
    control_landed(
        &mut s,
        "A",
        generation,
        "v".into(),
        ComputerControl::HandBack,
        Ok(()),
    );
    assert!(HANDED_BACK.with(|h| h.borrow().is_empty()));
}
