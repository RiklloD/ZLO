//! Sends, steers and the queue, driven through the composer's callers
//! (`actions::send_as`, the queue panel's actions) and settled through the
//! same answer handlers the jobs call. Each GPT-review finding has its own
//! regression test here.

use rune::Tx;
use zlo_client::attachments::StagedFile;
use zlo_client::orchestration::QueueDoc;
use zlo_client::transcript::Row;

use super::*;
use crate::chat::fixtures::{running, test_state};
use crate::design::prefs::FollowUps;

const ID: &str = "t1";

fn chat(s: &State) -> &ChatState {
    chat_ref(s, ID).expect("the open chat")
}

fn queue_of(texts: &[&str]) -> QueueDoc {
    let items: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| serde_json::json!({"id": format!("q{i}"), "text": t}))
        .collect();
    serde_json::from_value(serde_json::json!({"items": items})).expect("queue decodes")
}

fn ack(v: serde_json::Value) -> SubmitAck {
    serde_json::from_value(v).expect("ack decodes")
}

fn file(name: &str) -> StagedFile {
    StagedFile::from_bytes(name.into(), "image/png".into(), b"png".to_vec())
}

fn pending_rows(s: &State) -> Vec<(String, Option<bool>)> {
    chat(s)
        .transcript
        .rows()
        .into_iter()
        .filter_map(|r| match r {
            Row::User {
                key, text, steer, ..
            } if key.starts_with("pending:") => Some((text, steer.map(|m| m.waiting))),
            _ => None,
        })
        .collect()
}

fn outbox(s: &State) -> Vec<String> {
    crate::drafts::keys_with_prefix(s, &outbox_prefix(ID))
}

fn instance(s: &mut State) -> jobs::Instance {
    super::super::orchestration::instance_of(s, ID).expect("extras")
}

/// A message in flight the way `deliver` leaves it.
fn in_flight(s: &mut State) -> (u64, Draft) {
    let draft = take(s, ID).expect("a draft");
    let c = chat_for(s, ID).expect("chat");
    let names: Vec<String> = draft.files.iter().map(|f| f.filename.clone()).collect();
    let pending = c.transcript.push_pending_user(&draft.text, &names);
    c.transcript.set_pending_steer(pending, Some(true));
    (pending, draft)
}

fn type_text(s: &mut State, text: &str) {
    if let Some(c) = chat_for(s, ID) {
        c.composer.replace_text(text);
    }
}

#[test]
fn enter_while_running_steers_the_turn_by_default() {
    let mut s = test_state();
    running(&mut s);
    let mut tx = Tx::default();
    actions::send_as(&mut s, ID, false, &mut tx);
    assert!(
        chat(&s).composer.text.is_empty(),
        "the words left the composer"
    );
    assert_eq!(
        pending_rows(&s),
        vec![("also check clippy".into(), Some(true))],
        "shown inside the turn as a steer, waiting to be read"
    );
    assert!(!chat(&s).orchestration.busy, "nothing was queued");
    assert_eq!(outbox(&s).len(), 1, "kept until the gateway answers");
}

#[test]
fn the_alternate_queues_and_the_queue_setting_flips_both() {
    let mut tx = Tx::default();
    let mut s = test_state();
    running(&mut s);
    actions::send_as(&mut s, ID, true, &mut tx);
    assert!(chat(&s).orchestration.busy, "⌘↵ queues it");
    assert!(
        pending_rows(&s).is_empty(),
        "a queued message is not in the turn"
    );

    let mut s = test_state();
    running(&mut s);
    s.prefs.follow_ups = FollowUps::Queue;
    actions::send_as(&mut s, ID, false, &mut tx);
    assert!(
        chat(&s).orchestration.busy,
        "Enter queues under Follow-ups: Queue"
    );

    let mut s = test_state();
    running(&mut s);
    s.prefs.follow_ups = FollowUps::Queue;
    actions::send_as(&mut s, ID, true, &mut tx);
    assert_eq!(pending_rows(&s).len(), 1, "⌘↵ steers under Queue");
}

/// P1 (queued files): a message with files can wait in the queue now; the
/// files go with it.
#[test]
fn files_can_wait_in_the_queue() {
    let mut s = test_state();
    running(&mut s);
    actions::place(&mut s, ID, vec![file("shot.png")]);
    let mut tx = Tx::default();
    actions::send_as(&mut s, ID, true, &mut tx);
    assert!(chat(&s).error.is_none(), "{:?}", chat(&s).error);
    assert!(chat(&s).orchestration.busy, "queued");
    assert!(chat(&s).staged.is_empty() && chat(&s).composer.text.is_empty());
    let kept = crate::drafts::restore(&s, &outbox(&s)[0]).expect("outbox");
    assert_eq!(
        kept.files.len(),
        1,
        "the outbox keeps the file until the ack"
    );
}

/// P1 (Send now): it is one atomic backend call. The words never leave the
/// queue in Rune, the composer is untouched, and an item a delivery holds
/// (`sending`) or a compacting turn can't be sent.
#[test]
fn send_now_is_one_atomic_call_that_never_takes_the_words() {
    let mut s = test_state();
    running(&mut s);
    let mut tx = Tx::default();
    assert!(!send_first_queued(&mut s, ID, &mut tx), "nothing queued");
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue = queue_of(&["first", "second"]);
    }
    assert!(send_first_queued(&mut s, ID, &mut tx));
    assert!(chat(&s).orchestration.busy);
    assert_eq!(
        chat(&s).composer.text,
        "also check clippy",
        "the draft stays"
    );
    assert!(pending_rows(&s).is_empty(), "no optimistic copy to strand");
    assert_eq!(
        chat(&s).orchestration.queue.items.len(),
        2,
        "still queued until the answer"
    );

    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        let mut q = queue_of(&["held"]);
        q.items[0].status = "sending".into();
        c.orchestration.queue = q;
    }
    assert!(
        !send_first_queued(&mut s, ID, &mut tx),
        "a sending item is not offered"
    );
    send_now(&mut s, ID, "q0", &mut tx);
    assert!(!chat(&s).orchestration.busy);

    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue = queue_of(&["later"]);
        c.orchestration.queue.compacting = true;
    }
    send_now(&mut s, ID, "q0", &mut tx);
    assert!(!chat(&s).orchestration.busy, "compaction takes no steer");
}

/// P1 (queued edit): the edit stays in the composer until the gateway
/// answers; a second submit meanwhile does nothing; a refused save keeps
/// the revision with the error; only a confirmed save brings the set-aside
/// draft back.
#[test]
fn a_queued_edit_waits_for_the_ack_and_a_refused_save_keeps_the_revision() {
    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue = queue_of(&["first"]);
    }
    let inst = instance(&mut s);
    edit_queued(&mut s, ID, "q0");
    type_text(&mut s, "first, reworded");
    let mut tx = Tx::default();
    actions::send_as(&mut s, ID, false, &mut tx);
    assert!(
        chat(&s)
            .orchestration
            .editing
            .as_ref()
            .is_some_and(|e| e.saving)
    );
    assert_eq!(
        chat(&s).composer.text,
        "first, reworded",
        "kept until the ack"
    );
    actions::send_as(&mut s, ID, true, &mut tx);
    assert_eq!(
        chat(&s).composer.text,
        "first, reworded",
        "a second submit does nothing"
    );

    settle_edit(&mut s, ID, inst, Err("server said no".into()), &mut tx);
    assert_eq!(
        chat(&s).composer.text,
        "first, reworded",
        "the revision survives"
    );
    assert!(
        chat(&s)
            .orchestration
            .editing
            .as_ref()
            .is_some_and(|e| !e.saving)
    );
    assert!(
        chat(&s)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("server said no"))
    );

    actions::send_as(&mut s, ID, false, &mut tx);
    settle_edit(
        &mut s,
        ID,
        inst,
        Ok((queue_of(&["first, reworded"]), None)),
        &mut tx,
    );
    assert!(chat(&s).orchestration.editing.is_none());
    assert_eq!(
        chat(&s).composer.text,
        "also check clippy",
        "the set-aside draft is back"
    );
}

/// P1 (queued edit): save-and-steer is the backend's edit then `send_now`;
/// a refused send after a good save keeps it queued and says so.
#[test]
fn save_and_steer_keeps_the_item_queued_when_the_send_is_refused() {
    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue = queue_of(&["first"]);
    }
    let inst = instance(&mut s);
    edit_queued(&mut s, ID, "q0");
    type_text(&mut s, "steer this");
    let mut tx = Tx::default();
    actions::send_as(&mut s, ID, true, &mut tx);
    assert!(
        chat(&s)
            .orchestration
            .editing
            .as_ref()
            .is_some_and(|e| e.saving)
    );
    let saved = queue_of(&["steer this"]);
    settle_edit(
        &mut s,
        ID,
        inst,
        Ok((saved, Some(Err("turn_ended".into())))),
        &mut tx,
    );
    assert_eq!(chat(&s).orchestration.queue.items[0].text, "steer this");
    assert!(
        chat(&s)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("stays queued"))
    );
    assert_eq!(chat(&s).composer.text, "also check clippy");
}

/// P1 (queued edit): leaving the chat mid-edit keeps both drafts under
/// their own keys, and reopening brings the edit back below the draft.
#[test]
fn a_thread_switch_mid_edit_keeps_both_drafts() {
    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue = queue_of(&["first"]);
    }
    edit_queued(&mut s, ID, "q0");
    type_text(&mut s, "half-done edit");
    actions::record_draft(&mut s, ID);
    assert_eq!(
        crate::drafts::restore(&s, ID).map(|d| d.text).as_deref(),
        Some("also check clippy"),
        "the thread's draft is the set-aside composer"
    );
    assert_eq!(
        crate::drafts::restore(&s, &edit_key(ID))
            .map(|d| d.text)
            .as_deref(),
        Some("half-done edit"),
        "the edit has its own key"
    );
    // Reopened: a fresh chat gets its draft, then the edit comes back.
    s.chat = Some(ChatState::new(ID.into()));
    if let Some(c) = chat_for(&mut s, ID) {
        c.composer.replace_text("also check clippy");
    }
    recover(&mut s, ID);
    assert_eq!(
        chat(&s).composer.text,
        "also check clippy\n\nhalf-done edit"
    );
    assert!(!crate::drafts::has(&s, &edit_key(ID)));
}

/// P1 (queued edit): the item draining before the save keeps the edit and
/// brings back the set-aside draft; switching to another item with unsaved
/// changes, or Esc with changes, never drops them.
#[test]
fn a_drained_item_or_a_switch_never_drops_an_edit() {
    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue = queue_of(&["first", "second"]);
    }
    edit_queued(&mut s, ID, "q0");
    type_text(&mut s, "changed");
    edit_queued(&mut s, ID, "q1");
    assert_eq!(chat(&s).composer.text, "changed", "switch refused");
    assert!(escape_edit(&mut s, ID));
    assert_eq!(chat(&s).composer.text, "changed", "Esc keeps changes");
    assert!(chat(&s).orchestration.editing.is_some());

    edit_target_gone(&mut s, ID);
    assert!(chat(&s).orchestration.editing.is_none());
    assert_eq!(chat(&s).composer.text, "changed\n\nalso check clippy");
    assert!(
        chat(&s)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("still here"))
    );
}

/// P1 (failed follow-ups): a refused queue add puts words and files back
/// below a newer draft, never over it, and clears its outbox copy.
#[test]
fn a_refused_queue_add_keeps_words_and_files_below_a_newer_draft() {
    let mut s = test_state();
    running(&mut s);
    actions::place(&mut s, ID, vec![file("shot.png")]);
    let inst = instance(&mut s);
    let draft = take(&mut s, ID).expect("draft");
    type_text(&mut s, "typed since");
    settle_queue_add(&mut s, ID, inst, Err("queue full".into()), draft);
    assert!(
        chat(&s)
            .composer
            .text
            .starts_with("typed since\n\nalso check clippy")
    );
    assert_eq!(chat(&s).staged.len(), 1, "the pasted picture came back");
    assert_eq!(
        inline_files::pills(&chat(&s).composer).len(),
        1,
        "with its card, moved below the newer draft"
    );
    assert!(
        chat(&s)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("queue full"))
    );
    assert!(outbox(&s).is_empty());
}

/// P1 (failed follow-ups): a failed steer with a newer draft present keeps
/// the newer draft and adds the words and files below it.
#[test]
fn a_failed_steer_keeps_files_below_a_newer_draft() {
    let mut s = test_state();
    running(&mut s);
    actions::place(&mut s, ID, vec![file("paste.png")]);
    let (pending, draft) = in_flight(&mut s);
    type_text(&mut s, "newer");
    let mut tx = Tx::default();
    apply_landed(&mut s, ID, pending, Err("offline".into()), draft, &mut tx);
    assert!(
        chat(&s)
            .composer
            .text
            .starts_with("newer\n\nalso check clippy")
    );
    assert_eq!(chat(&s).staged.len(), 1);
    assert_eq!(inline_files::pills(&chat(&s).composer).len(), 1);
    assert!(pending_rows(&s).is_empty());
    assert!(outbox(&s).is_empty());
}

/// P1 (failed follow-ups): no channel and a busy queue are found before
/// anything leaves the composer.
#[test]
fn a_blocked_send_takes_nothing_from_the_composer() {
    let mut tx = Tx::default();
    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.channel = None;
    }
    actions::send_as(&mut s, ID, false, &mut tx);
    assert_eq!(chat(&s).composer.text, "also check clippy");
    assert!(outbox(&s).is_empty());

    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.busy = true;
    }
    actions::send_as(&mut s, ID, true, &mut tx);
    assert_eq!(
        chat(&s).composer.text,
        "also check clippy",
        "busy queue: kept"
    );
    assert!(
        chat(&s)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("busy"))
    );
}

/// P1 (failed follow-ups): with the chat closed, a failed send lands in
/// the thread's saved draft; an outbox copy left by a quit comes back when
/// the chat opens, and one still in flight does not.
#[test]
fn unsent_words_survive_a_closed_chat_and_a_quit() {
    let mut s = test_state();
    running(&mut s);
    let (pending, draft) = in_flight(&mut s);
    s.chat = None;
    let mut tx = Tx::default();
    apply_landed(&mut s, ID, pending, Err("offline".into()), draft, &mut tx);
    assert_eq!(
        crate::drafts::restore(&s, ID).map(|d| d.text).as_deref(),
        Some("also check clippy")
    );

    let mut s = test_state();
    running(&mut s);
    let draft = take(&mut s, ID).expect("draft");
    s.chat = Some(ChatState::new(ID.into()));
    recover(&mut s, ID);
    assert!(
        chat(&s).composer.text.is_empty(),
        "still in flight: not recovered"
    );
    // A quit: this process no longer has it in flight.
    IN_FLIGHT.with(|f| f.borrow_mut().clear());
    recover(&mut s, ID);
    assert_eq!(chat(&s).composer.text, "also check clippy");
    assert!(outbox(&s).is_empty());
    drop(draft);
}

/// P2 (one settlement path): each typed delivery settles the row once; a
/// steer the turn takes once ready shows only as its queue row; an older
/// gateway's busy answer is not guessed.
#[test]
fn the_typed_delivery_settles_the_row() {
    let mut tx = Tx::default();
    let settle = |answer: serde_json::Value| {
        let mut s = test_state();
        running(&mut s);
        let (pending, draft) = in_flight(&mut s);
        let mut tx = Tx::default();
        apply_landed(&mut s, ID, pending, Ok(ack(answer)), draft, &mut tx);
        s
    };
    let s = settle(
        serde_json::json!({"outcome": "deferred_busy", "delivery": "steered",
        "steer_id": "s1"}),
    );
    assert_eq!(
        pending_rows(&s),
        vec![("also check clippy".into(), Some(true))]
    );
    assert!(outbox(&s).is_empty(), "confirmed");
    let s = settle(
        serde_json::json!({"outcome": "deferred_busy", "delivery": "steered",
        "waiting_for_turn": true, "queued_id": "q9"}),
    );
    assert!(pending_rows(&s).is_empty(), "shown once, as its queue row");
    let s = settle(
        serde_json::json!({"outcome": "deferred_busy", "delivery": "queued",
        "queued_id": "q9", "notice": "Sent to the running turn"}),
    );
    assert!(pending_rows(&s).is_empty(), "the notice text never decides");
    let s = settle(serde_json::json!({"outcome": "submitted", "delivery": "started"}));
    assert_eq!(pending_rows(&s), vec![("also check clippy".into(), None)]);
    let s = settle(serde_json::json!({"outcome": "deferred_busy"}));
    assert_eq!(
        pending_rows(&s),
        vec![("also check clippy".into(), None)],
        "unresolved"
    );
    let mut s = settle(serde_json::json!({"outcome": "rejected_busy"}));
    assert_eq!(chat(&s).composer.text, "also check clippy");
    assert!(
        chat(&s)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("⌘↵ to queue"))
    );
    assert!(pending_rows(&s).is_empty());
    let _ = &mut s;
    let _ = &mut tx;
}

/// P2 (one settlement path): a send Rune thought idle goes through the
/// same settlement, so a gateway that had a turn running after all is
/// believed: queued drops the bubble, refused restores the words.
#[test]
fn an_idle_send_settles_like_any_other() {
    let mut s = test_state();
    crate::chat::fixtures::chat(&mut s);
    type_text(&mut s, "hello");
    let mut tx = Tx::default();
    actions::send_as(&mut s, ID, false, &mut tx);
    let pending = pending_rows(&s);
    assert_eq!(
        pending,
        vec![("hello".into(), None)],
        "idle: a prompt, not a steer"
    );
    let key = outbox(&s)[0].clone();
    let sub = key.trim_start_matches(&outbox_prefix(ID)).to_string();
    let draft = Draft {
        text: "hello".into(),
        files: Vec::new(),
        draft_text: "hello".into(),
        draft_pills: Vec::new(),
        sub,
        idle: true,
    };
    let id = chat(&s)
        .transcript
        .rows()
        .iter()
        .find_map(|r| match r {
            Row::User { key, .. } => key.strip_prefix("pending:").and_then(|n| n.parse().ok()),
            _ => None,
        })
        .expect("pending id");
    apply_landed(
        &mut s,
        ID,
        id,
        Ok(ack(
            serde_json::json!({"outcome": "deferred_busy", "delivery": "queued",
            "queued_id": "q1"}),
        )),
        draft,
        &mut tx,
    );
    assert!(
        pending_rows(&s).is_empty(),
        "queued after all: no sent bubble"
    );
    assert!(outbox(&s).is_empty());
}

/// P2 (compaction): the backend's `compacting` makes every send queue.
#[test]
fn a_compacting_turn_queues_even_on_steer() {
    let mut s = test_state();
    running(&mut s);
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.queue.compacting = true;
    }
    assert_eq!(mode_of(&s, ID, false), Mode::Queue);
    assert_eq!(mode_of(&s, ID, true), Mode::Queue);
}

/// P2 (Stop): for 500 ms after Stop the button in its place ignores
/// clicks, so a double-click on Stop can't send or resume.
#[test]
fn the_click_after_stop_is_ignored() {
    let mut s = test_state();
    running(&mut s);
    let mut tx = Tx::default();
    press_stop(&mut s, ID, &mut tx);
    press_send(&mut s, ID, false, false, &mut tx);
    assert_eq!(chat(&s).composer.text, "also check clippy", "guarded");
    if let Some(c) = chat_for(&mut s, ID) {
        c.orchestration.stopped_at = Some(
            std::time::Instant::now() - STOP_CLICK_GUARD - std::time::Duration::from_millis(1),
        );
    }
    press_send(&mut s, ID, false, false, &mut tx);
    assert!(
        chat(&s).composer.text.is_empty(),
        "after the guard it sends"
    );
}
