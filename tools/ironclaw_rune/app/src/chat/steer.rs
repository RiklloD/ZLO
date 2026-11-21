//! Sends and follow-ups, as t3code does them. Enter does Settings →
//! Follow-ups (Steer by default) while a turn runs: the message joins the
//! running turn and shows inside it with a "Steer" marker. ⌘↵ or ⌘-click on
//! the send button does the other (queue) for one message. Nothing restarts
//! the turn, and the backend declines an approval the steer replaces.
//! Steering waits only for compaction (the queue view's `compacting`); then
//! the message queues.
//!
//! **One settlement path.** Every composer send, idle or not, goes through
//! [`deliver`] and settles in [`apply_landed`] from the ack's typed
//! `delivery` ([`Landed`]), never from its notice text or a guess at the
//! running run.
//!
//! **No words are lost.** A message leaving the composer is first written
//! to an outbox draft (`unsent:<thread>:<submission>`, on disk with the
//! other drafts) and removed only once the gateway confirmed it. A failure
//! puts the words, pills and files back below whatever the composer holds
//! now ([`actions::put_back`]), or into the thread's saved draft when the
//! chat is closed; one left over by a quit comes back when the chat opens.
//!
//! The queue panel's Send now (the backend's atomic `send_now`), Edit and
//! the shortcuts (⇧⌘↵ sends the first queued message, ⌥↑ edits the last)
//! are here too.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use rune::{El, Tx, row};
use rune_kit::Kit;
use zlo_client::attachments::StagedFile;
use zlo_client::orchestration::{QueueAction, QueueDoc};
use zlo_client::wire::{Landed, SubmitAck};

use super::dispatch::{self, Mode};
use super::{
    ChatState, actions, chat_for, chat_ref, composer::round_button_tinted, inline_files, set_error,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

/// t3code's `STOP_LANDED_CLICK_GUARD_MS`: the button that takes Stop's
/// place ignores clicks this long, so a double-click on Stop cannot send or
/// resume.
pub(crate) const STOP_CLICK_GUARD: Duration = Duration::from_millis(500);

type Pills = Vec<(std::ops::Range<usize>, String)>;

/// A message on its way out, with what the composer held, so a failure can
/// put it back. `sub` keys its outbox draft.
pub(crate) struct Draft {
    text: String,
    files: Vec<StagedFile>,
    draft_text: String,
    draft_pills: Pills,
    sub: String,
    /// Sent to an idle thread: a failure also undoes the new turn's layout.
    idle: bool,
}

thread_local! {
    /// Outbox keys whose send is still in flight in this process: not
    /// recovered on reopen while their answer may still come.
    static IN_FLIGHT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

fn outbox_prefix(thread: &str) -> String {
    format!("unsent:{thread}:")
}

fn outbox_key(thread: &str, sub: &str) -> String {
    format!("{}{sub}", outbox_prefix(thread))
}

/// The thread's unsaved edit of a queued message, kept apart from its draft.
fn edit_key(thread: &str) -> String {
    format!("queued-edit:{thread}")
}

/// A submission id: unique in this process, ordered, readable in a file name.
fn new_sub() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{n}", chrono::Utc::now().timestamp_millis())
}

fn as_saved(text: &str, pills: &Pills, files: &[StagedFile]) -> crate::drafts::Draft {
    let tokens = pills
        .iter()
        .filter_map(|(range, id)| {
            let shown = text.get(range.clone())?.to_string();
            Some(crate::drafts::DraftToken {
                range: range.clone(),
                id: id.clone(),
                label: shown.clone(),
                text: shown,
            })
        })
        .collect();
    crate::drafts::Draft {
        text: text.to_string(),
        tokens,
        files: files.to_vec(),
        profile: None,
    }
}

/// Keep `draft` on disk until its send is confirmed.
fn outbox_put(s: &mut State, thread: &str, draft: &Draft) {
    let key = outbox_key(thread, &draft.sub);
    IN_FLIGHT.with(|f| f.borrow_mut().insert(key.clone()));
    crate::drafts::record(
        s,
        &key,
        as_saved(&draft.draft_text, &draft.draft_pills, &draft.files),
    );
}

/// The send of `sub` settled (sent, or its words put back).
fn outbox_done(s: &mut State, thread: &str, sub: &str) {
    let key = outbox_key(thread, sub);
    IN_FLIGHT.with(|f| f.borrow_mut().remove(&key));
    crate::drafts::remove(s, &key);
}

/// The chat opened: unsent words a quit (or a closed chat) left in the
/// outbox, and an unsaved edit of a queued message, come back below the
/// composer's draft with a note. Sends still in flight are left alone.
pub(crate) fn recover(s: &mut State, id: &str) {
    let mut keys = crate::drafts::keys_with_prefix(s, &outbox_prefix(id));
    keys.retain(|k| !IN_FLIGHT.with(|f| f.borrow().contains(k)));
    let edit = edit_key(id);
    if crate::drafts::has(s, &edit) {
        keys.push(edit);
    }
    let mut found = false;
    for key in keys {
        let Some(saved) = crate::drafts::restore(s, &key) else {
            continue;
        };
        let pills: Pills = saved
            .tokens
            .iter()
            .map(|t| (t.range.clone(), t.id.clone()))
            .collect();
        if let Some(c) = chat_for(s, id) {
            actions::put_back(c, &saved.text, &pills, saved.files);
            found = true;
            crate::drafts::remove(s, &key);
        }
    }
    if found {
        actions::record_draft(s, id);
        set_error(
            s,
            id,
            "A message that wasn't confirmed sent is back in the composer.",
        );
    }
}

/// What a send from `id`'s composer does now; `alternate` is ⌘↵ / ⌘-click.
pub(crate) fn mode_of(s: &State, id: &str, alternate: bool) -> Mode {
    let Some(c) = chat_ref(s, id) else {
        return Mode::Start;
    };
    dispatch::resolve(
        c.running_run().is_some(),
        alternate,
        s.prefs.follow_ups,
        dispatch::can_steer(c),
    )
}

/// A send while a turn runs: steer it or queue it, as `mode_of` says.
pub(crate) fn send_running(
    s: &mut State,
    id: &str,
    alternate: bool,
    voice: Option<crate::voice::Submission>,
    tx: &mut Tx,
) {
    match mode_of(s, id, alternate) {
        Mode::Queue => queue(s, id, voice),
        Mode::Steer | Mode::Start => steer(s, id, voice, tx),
    }
}

/// Why `id` can't send right now, checked before anything leaves the
/// composer. An empty reason is a silent refusal (the Stop click guard).
pub(crate) fn send_blocked(s: &State, id: &str) -> Option<&'static str> {
    let c = chat_ref(s, id)?;
    if c.channel.is_none() {
        return Some("This gateway has no chat channel configured.");
    }
    if c.orchestration.stop_guard() {
        return Some("");
    }
    None
}

/// Take the composer's message, emptying it; the outbox keeps a copy until
/// the send is confirmed. `None` when there is nothing.
pub(crate) fn take(s: &mut State, id: &str) -> Option<Draft> {
    let c = chat_for(s, id)?;
    let (text, files) = inline_files::outgoing(&c.composer, &c.staged);
    if text.is_empty() && files.is_empty() {
        return None;
    }
    let draft = Draft {
        text,
        files,
        draft_text: c.composer.text.clone(),
        draft_pills: inline_files::pills(&c.composer),
        sub: new_sub(),
        idle: false,
    };
    c.composer.clear();
    c.error = None;
    c.staged.clear();
    outbox_put(s, id, &draft);
    actions::record_draft(s, id);
    Some(draft)
}

impl Draft {
    /// Mark a send to an idle thread (a new turn's layout to undo on
    /// failure).
    pub(crate) fn idle(mut self) -> Self {
        self.idle = true;
        self
    }
}

/// The message joins the running turn, shown as the person's row inside
/// it. `voice` is talk mode's claim on it.
fn steer(s: &mut State, id: &str, voice: Option<crate::voice::Submission>, tx: &mut Tx) {
    if let Some(why) = send_blocked(s, id) {
        refuse_voice(s, voice, why);
        if !why.is_empty() {
            set_error(s, id, why);
        }
        return;
    }
    match take(s, id) {
        Some(draft) => deliver(s, id, draft, voice, tx),
        None => refuse_voice(s, voice, "There was nothing to send."),
    }
}

fn refuse_voice(s: &mut State, voice: Option<crate::voice::Submission>, why: &str) {
    if let Some(sub) = voice {
        let why = if why.is_empty() { "Not sent." } else { why };
        crate::voice::submission_refused(s, sub, why);
    }
}

/// Send `draft` through the chat channel. While a turn runs its row shows
/// as a steer, waiting to be read; the ack says where it really went.
pub(crate) fn deliver(
    s: &mut State,
    id: &str,
    draft: Draft,
    voice: Option<crate::voice::Submission>,
    tx: &mut Tx,
) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(channel) = c.channel.clone() else {
        refuse_voice(s, voice, "This gateway has no chat channel configured.");
        rescue(
            s,
            id,
            draft,
            "This gateway has no chat channel configured.".into(),
        );
        return;
    };
    let running = c.running_run();
    let names: Vec<String> = draft.files.iter().map(|f| f.filename.clone()).collect();
    let pending = c.transcript.push_pending_user(&draft.text, &names);
    if running.is_some() {
        c.transcript.set_pending_steer(pending, Some(true));
    }
    super::refresh_rows(s, id, tx);

    let thread_id = id.to_string();
    let req = thread_id.clone();
    let body = draft.text.clone();
    let wire = draft.files.iter().map(StagedFile::to_wire).collect();
    let roots = actions::mention_roots(s, id, &draft.text);
    jobs::run(
        s,
        "chat",
        move |c| async move {
            c.send_message_with_roots(&channel, &req, &body, wire, roots)
                .await
        },
        move |s, result, tx| {
            match (&result, voice) {
                (Ok(ack), Some(sub)) => report_voice(s, &thread_id, sub, ack, running.clone()),
                (Err(e), Some(sub)) => crate::voice::submission_refused(s, sub, &e.to_string()),
                (_, None) => {}
            }
            let landed = result.map_err(|e| e.to_string());
            apply_landed(s, &thread_id, pending, landed, draft, tx);
        },
    );
}

/// The gateway answered a send: settle its row by where it went, and keep
/// the words wherever it did not take them. The one settlement of every
/// composer send.
pub(crate) fn apply_landed(
    s: &mut State,
    thread_id: &str,
    pending: u64,
    answer: Result<SubmitAck, String>,
    draft: Draft,
    tx: &mut Tx,
) {
    let failure = match &answer {
        Ok(ack) => match ack.landed() {
            // The turn refused it: never show it as sent.
            Landed::Refused => Some(
                "The running turn didn't take this message. Press Stop to end it, or ⌘↵ to queue it for after the turn.".to_string(),
            ),
            _ => None,
        },
        Err(e) => Some(e.clone()),
    };
    if let Some(error) = failure {
        if let Some(c) = chat_for(s, thread_id) {
            c.transcript.drop_pending_user(pending);
            if draft.idle {
                if let Some(r) = c.reserve.as_mut() {
                    r.held = false;
                }
                c.climb_until = None;
            }
        }
        if draft.idle {
            crate::workspace::send_failed(s, thread_id);
        }
        rescue(s, thread_id, draft, error);
        super::refresh_rows(s, thread_id, tx);
        super::load_timeline(s, thread_id);
        return;
    }
    outbox_done(s, thread_id, &draft.sub);
    let Ok(ack) = answer else { return };
    let Some(c) = chat_for(s, thread_id) else {
        return;
    };
    if let Some(steer) = ack.steer_id.clone() {
        c.transcript.set_pending_steer_id(pending, Some(steer));
    }
    match ack.landed() {
        // It waits in the queue panel until the turn ends; a steer the turn
        // takes once it can shows there too (as "steers in once ready"),
        // never as a second bubble.
        Landed::Queued => c.transcript.drop_pending_user(pending),
        Landed::Steered { waiting: true } if ack.queued_id.is_some() => {
            c.transcript.drop_pending_user(pending);
        }
        Landed::Steered { .. } => {
            c.transcript.ack_pending_user(pending);
            c.transcript.set_pending_steer(pending, Some(true));
        }
        // Too late to steer, or an idle send: it started the next turn.
        // An older gateway's busy answer is unresolved until read back.
        Landed::Started | Landed::Unconfirmed | Landed::Refused => {
            c.transcript.ack_pending_user(pending);
            c.transcript.set_pending_steer(pending, None);
        }
    }
    if ack.outcome.as_deref() == Some("submitted")
        && let Some(notice) = ack.notice.filter(|n| !n.trim().is_empty())
    {
        c.error = Some(notice);
    }
    super::refresh_rows(s, thread_id, tx);
    super::load_timeline(s, thread_id);
    super::refresh_extras(s, thread_id);
}

/// Tell talk mode what the gateway made of its message: refused, queued
/// behind the busy run, or taken (a steer joins the run the gateway names).
pub(crate) fn report_voice(
    s: &mut State,
    id: &str,
    sub: crate::voice::Submission,
    ack: &SubmitAck,
    running: Option<String>,
) {
    let composer = crate::voice::Composer::Chat(id.to_string());
    let busy_run = ack.active_run_id.clone().or(running);
    match ack.landed() {
        Landed::Refused => crate::voice::submission_refused(
            s,
            sub,
            "The running turn didn't take your message; it's back in the composer.",
        ),
        Landed::Queued => crate::voice::submission_queued(s, sub, id, busy_run, composer),
        Landed::Steered { .. } | Landed::Unconfirmed => {
            crate::voice::submission_accepted(s, sub, id, busy_run, composer)
        }
        Landed::Started => {
            crate::voice::submission_accepted(s, sub, id, ack.run_id.clone(), composer)
        }
    }
}

/// Unsent words go back where the person finds them, never lost: into the
/// open chat's composer (below anything typed since, see
/// [`actions::put_back`]), or, with the chat closed, into the thread's
/// saved draft. `error` is shown with them. Their outbox copy goes.
pub(crate) fn rescue(s: &mut State, id: &str, draft: Draft, error: String) {
    let sub = draft.sub.clone();
    rescue_words(
        s,
        id,
        &draft.draft_text,
        &draft.draft_pills,
        draft.files,
        error,
    );
    outbox_done(s, id, &sub);
}

/// [`rescue`] for words that are not a [`Draft`].
pub(crate) fn rescue_words(
    s: &mut State,
    id: &str,
    text: &str,
    pills: &[(std::ops::Range<usize>, String)],
    files: Vec<StagedFile>,
    error: String,
) {
    if let Some(c) = chat_for(s, id) {
        let error = actions::restore_failed(c, text, pills, files, error);
        actions::record_draft(s, id);
        set_error(s, id, error);
        return;
    }
    if text.trim().is_empty() && files.is_empty() {
        return;
    }
    let saved = crate::drafts::restore(s, id);
    let (old_text, old_pills, mut old_files) = match saved {
        Some(d) => (
            d.text,
            d.tokens
                .into_iter()
                .map(|t| (t.range, t.id))
                .collect::<Vec<_>>(),
            d.files,
        ),
        None => (String::new(), Vec::new(), Vec::new()),
    };
    let (joined, joined_pills) = actions::join_drafts(&old_text, &old_pills, text, pills);
    old_files.extend(files);
    crate::drafts::record(s, id, as_saved(&joined, &joined_pills, &old_files));
    tracing::debug!(%error, "unsent words kept in the closed chat's draft");
}

/// The message, files too, waits in the thread's queue for the turn to end.
fn queue(s: &mut State, id: &str, voice: Option<crate::voice::Submission>) {
    if let Some(why) = send_blocked(s, id) {
        refuse_voice(s, voice, why);
        if !why.is_empty() {
            set_error(s, id, why);
        }
        return;
    }
    let Some(c) = chat_ref(s, id) else { return };
    // Checked before the words leave the composer: nothing to restore.
    if c.orchestration.busy {
        refuse_voice(s, voice, "The queue is busy; try again.");
        return set_error(s, id, "The queue is busy with another change; try again.");
    }
    let running = c.running_run();
    let Some(draft) = take(s, id) else {
        refuse_voice(s, voice, "There was nothing to send.");
        return;
    };
    queue_add(s, id, draft);
    if let Some(sub) = voice {
        let composer = crate::voice::Composer::Chat(id.to_string());
        crate::voice::submission_queued(s, sub, id, running, composer);
    }
}

/// Add `draft` to the queue; a refused add keeps its words (and files).
fn queue_add(s: &mut State, id: &str, draft: Draft) {
    let Some(c) = chat_for(s, id) else { return };
    let o = &mut c.orchestration;
    o.busy = true;
    o.note = None;
    let instance = super::orchestration::instance_of_state(o);
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    let action = QueueAction::Add {
        text: draft.text.clone(),
        attachments: draft.files.iter().map(StagedFile::to_wire).collect(),
    };
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.mutate_queue(&thread_id, &action).await },
        move |s, result, tx| {
            // The person's queued message rises into the queue.
            tx.animated();
            settle_queue_add(
                s,
                &asked,
                instance,
                result.map_err(|e| e.to_string()),
                draft,
            );
        },
    );
}

/// A queue add answered.
pub(crate) fn settle_queue_add(
    s: &mut State,
    asked: &str,
    instance: jobs::Instance,
    result: Result<QueueDoc, String>,
    draft: Draft,
) {
    let current = super::orchestration::is_current(s, asked, instance);
    if current && let Some(c) = chat_for(s, asked) {
        c.orchestration.busy = false;
    }
    match result {
        Ok(queue) => {
            outbox_done(s, asked, &draft.sub);
            if current && let Some(c) = chat_for(s, asked) {
                c.orchestration.queue = queue;
            }
        }
        Err(e) => rescue(s, asked, draft, format!("Could not queue the message: {e}")),
    }
    super::orchestration::ensure_poll(s, asked);
}

// ── the queue panel's actions ───────────────────────────────────────────

/// A queued message out of the queue and into the conversation (t3code's
/// Send now): the backend's atomic `send_now` steers it into the running
/// turn, or starts it on an idle thread without releasing a held queue.
/// The words never leave the queue here, so nothing can be lost; an item
/// the queue already took answers `not_queued`.
pub(crate) fn send_now(s: &mut State, id: &str, item_id: &str, tx: &mut Tx) {
    let Some(c) = chat_ref(s, id) else { return };
    let o = &c.orchestration;
    if o.busy || o.queue.compacting {
        return;
    }
    if !o
        .queue
        .items
        .iter()
        .any(|i| i.id == item_id && !i.is_sending())
    {
        return;
    }
    let action = QueueAction::SendNow(item_id.to_string());
    queue_call(s, id, action, "Could not send the queued message", tx);
}

/// One queue call whose words stay on the backend either way; the answer
/// replaces the queue, and the timeline reloads.
fn queue_call(s: &mut State, id: &str, action: QueueAction, failure: &'static str, tx: &mut Tx) {
    let Some(c) = chat_for(s, id) else { return };
    let o = &mut c.orchestration;
    o.busy = true;
    o.note = None;
    let instance = super::orchestration::instance_of_state(o);
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    super::refresh_rows(s, id, tx);
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.mutate_queue(&thread_id, &action).await },
        move |s, result, tx| {
            if !super::orchestration::is_current(s, &asked, instance) {
                return;
            }
            // The person's send-now: the row leaves the queue with motion.
            tx.animated();
            if let Some(c) = chat_for(s, &asked) {
                c.orchestration.busy = false;
                match result {
                    Ok(queue) => c.orchestration.queue = queue,
                    Err(e) => crate::chat::report_error(s, &asked, format!("{failure}: {e}")),
                }
            }
            super::load_timeline(s, &asked);
            super::orchestration::refresh(s, &asked);
        },
    );
}

/// ⇧⌘↵: send the first queued message now. `false` when there is none.
pub(crate) fn send_first_queued(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    let first = chat_ref(s, id).and_then(|c| {
        c.orchestration
            .queue
            .items
            .iter()
            .find(|i| !i.is_sending())
            .map(|i| i.id.clone())
    });
    match first {
        Some(item) => {
            send_now(s, id, &item, tx);
            true
        }
        None => false,
    }
}

/// Load a queued message into the composer to edit it. What the composer
/// held waits aside (and stays the thread's saved draft) until the edit is
/// saved or cancelled. Moving to another queued message while this one has
/// unsaved changes is refused, so the changes are never dropped.
pub(crate) fn edit_queued(s: &mut State, id: &str, item_id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(item) = c
        .orchestration
        .queue
        .items
        .iter()
        .find(|i| i.id == item_id && !i.is_sending())
    else {
        return;
    };
    let text = item.text.clone();
    if let Some(editing) = &c.orchestration.editing {
        if editing.item_id == item_id {
            return;
        }
        if editing.saving || editing.changed(&c.composer.text) {
            return set_error(
                s,
                id,
                "Save or cancel the queued message you're editing first.",
            );
        }
    }
    let stash = match c.orchestration.editing.take() {
        // Moving to another item keeps the first stash.
        Some(editing) => editing.stash,
        None => Stash {
            text: c.composer.text.clone(),
            pills: inline_files::pills(&c.composer),
            files: std::mem::take(&mut c.staged),
        },
    };
    c.orchestration.editing = Some(Editing {
        item_id: item_id.to_string(),
        original: text.clone(),
        stash,
        saving: false,
    });
    c.error = None;
    c.composer.replace_text(text);
    super::composer::on_change(s, id);
    super::focus_composer_of(s, id);
}

/// ⌥↑: edit the last queued message. `false` when there is none.
pub(crate) fn edit_last_queued(s: &mut State, id: &str) -> bool {
    let last = chat_ref(s, id).and_then(|c| {
        c.orchestration
            .queue
            .items
            .iter()
            .rev()
            .find(|i| !i.is_sending())
            .map(|i| i.id.clone())
    });
    match last {
        Some(item) => {
            edit_queued(s, id, &item);
            true
        }
        None => false,
    }
}

/// Cancel: leave the edit, dropping its unsaved changes (the queued message
/// keeps its words); the composer gets back what it held before. Not while
/// a save is out.
pub(crate) fn cancel_edit(s: &mut State, id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    if c.orchestration.editing.as_ref().is_some_and(|e| e.saving) {
        return;
    }
    let Some(editing) = c.orchestration.editing.take() else {
        return;
    };
    let Stash { text, pills, files } = editing.stash;
    c.composer.clear();
    c.staged.clear();
    actions::put_back(c, &text, &pills, files);
    crate::drafts::remove(s, &edit_key(id));
    super::composer::on_change(s, id);
}

/// Esc while editing: leaves an unchanged edit; with changes it keeps them
/// and says how to drop them (only Cancel discards words). `true` when
/// handled.
pub(crate) fn escape_edit(s: &mut State, id: &str) -> bool {
    let Some(c) = chat_ref(s, id) else {
        return false;
    };
    let Some(editing) = &c.orchestration.editing else {
        return false;
    };
    if editing.saving || editing.changed(&c.composer.text) {
        set_error(
            s,
            id,
            "Press Enter to save the queued message, or Cancel beside it to drop your changes.",
        );
    } else {
        cancel_edit(s, id);
    }
    true
}

/// Submit while editing a queued message: save it (Enter), or, with the
/// alternate resolving to steer (⌘↵ while a turn runs), save it and send it
/// into the running turn (`send_now`). The edit stays in the composer until
/// the gateway answers: a refused save keeps it there with the error, and
/// a second submit meanwhile does nothing. Files go with the edit.
pub(crate) fn submit_edit(s: &mut State, id: &str, alternate: bool, tx: &mut Tx) {
    let mode = mode_of(s, id, alternate);
    let Some(c) = chat_ref(s, id) else { return };
    let Some(editing) = c.orchestration.editing.as_ref() else {
        return;
    };
    if editing.saving || c.orchestration.busy {
        return;
    }
    let item_id = editing.item_id.clone();
    let (text, files) = inline_files::outgoing(&c.composer, &c.staged);
    if text.trim().is_empty() && files.is_empty() {
        return;
    }
    let still_queued = c
        .orchestration
        .queue
        .items
        .iter()
        .any(|i| i.id == item_id && !i.is_sending());
    if !still_queued {
        return edit_target_gone(s, id);
    }
    let steer_after = alternate && mode == Mode::Steer && !c.orchestration.queue.compacting;
    let Some(c) = chat_for(s, id) else { return };
    if let Some(editing) = c.orchestration.editing.as_mut() {
        editing.saving = true;
    }
    c.error = None;
    let o = &mut c.orchestration;
    o.busy = true;
    let instance = super::orchestration::instance_of_state(o);
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    let edit = QueueAction::Edit {
        item_id: item_id.clone(),
        text,
        attachments: files.iter().map(StagedFile::to_wire).collect(),
    };
    super::refresh_rows(s, id, tx);
    jobs::run(
        s,
        "orchestration",
        move |c| async move {
            let saved = c.mutate_queue(&thread_id, &edit).await?;
            if !steer_after {
                return Ok((saved, None));
            }
            let sent = c
                .mutate_queue(&thread_id, &QueueAction::SendNow(item_id))
                .await;
            Ok((saved, Some(sent.map_err(|e| e.to_string()))))
        },
        move |s, result: Result<EditAnswer, zlo_client::ClientError>, tx| {
            settle_edit(s, &asked, instance, result.map_err(|e| e.to_string()), tx);
        },
    );
}

/// A saved queue, and the send that followed (save and steer).
type EditAnswer = (QueueDoc, Option<Result<QueueDoc, String>>);

/// A queued edit's save (and send) answered. Only now does the composer
/// leave the edit: saved, it gets back what was set aside; refused, the edit
/// stays in it with the error.
pub(crate) fn settle_edit(
    s: &mut State,
    asked: &str,
    instance: jobs::Instance,
    result: Result<EditAnswer, String>,
    tx: &mut Tx,
) {
    if !super::orchestration::is_current(s, asked, instance) {
        // The chat closed meanwhile: its draft and the edit's own draft
        // were recorded; a saved edit's copy is no longer needed.
        if result.is_ok() {
            crate::drafts::remove(s, &edit_key(asked));
        }
        return;
    }
    let Some(c) = chat_for(s, asked) else { return };
    c.orchestration.busy = false;
    if let Some(editing) = c.orchestration.editing.as_mut() {
        editing.saving = false;
    }
    match result {
        Err(e) => {
            set_error(s, asked, format!("Could not save the queued message: {e}"));
        }
        Ok((saved, sent)) => {
            c.orchestration.queue = saved;
            if let Some(Ok(queue)) = &sent {
                c.orchestration.queue = queue.clone();
            }
            c.composer.clear();
            c.staged.clear();
            restore_stash(s, asked);
            crate::drafts::remove(s, &edit_key(asked));
            if let Some(Err(e)) = sent {
                set_error(
                    s,
                    asked,
                    format!("Saved, but it couldn't be sent now: {e}. It stays queued."),
                );
            }
            super::load_timeline(s, asked);
        }
    }
    super::composer::on_change(s, asked);
    super::refresh_rows(s, asked, tx);
}

/// The message being edited left the queue (the turn took it) before the
/// edit was saved: a changed edit stays in the composer as an ordinary
/// draft, with what was set aside below it; an unchanged one went as it
/// was, so the composer just gets back what it held.
pub(crate) fn edit_target_gone(s: &mut State, id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(editing) = c.orchestration.editing.as_ref() else {
        return;
    };
    if editing.saving {
        return;
    }
    let changed = editing.changed(&c.composer.text);
    if !changed {
        c.composer.clear();
    }
    restore_stash(s, id);
    crate::drafts::remove(s, &edit_key(id));
    if changed {
        set_error(
            s,
            id,
            "That queued message was sent before you saved; your edited text is still here.",
        );
    }
}

/// Put the stashed composer back after a finished edit, below anything in
/// the composer now.
fn restore_stash(s: &mut State, id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    if let Some(editing) = c.orchestration.editing.take() {
        let Stash { text, pills, files } = editing.stash;
        actions::put_back(c, &text, &pills, files);
    }
    actions::record_draft(s, id);
}

/// What the thread's saved draft holds while a queued message is edited:
/// the set-aside composer. The edit itself (when changed) is saved apart
/// under its own key, so a thread switch, a quit or the item draining keeps
/// both.
pub(crate) fn draft_while_editing(
    s: &mut State,
    id: &str,
) -> Option<(String, Pills, Vec<StagedFile>)> {
    let c = chat_ref(s, id)?;
    let editing = c.orchestration.editing.as_ref()?;
    let stash = &editing.stash;
    let base = (stash.text.clone(), stash.pills.clone(), stash.files.clone());
    let edit = editing.changed(&c.composer.text).then(|| {
        as_saved(
            &c.composer.text,
            &inline_files::pills(&c.composer),
            &c.staged,
        )
    });
    match edit {
        Some(edit) => crate::drafts::record(s, &edit_key(id), edit),
        None => crate::drafts::remove(s, &edit_key(id)),
    }
    Some(base)
}

/// A queued message in the composer, and what the composer held before.
pub(crate) struct Editing {
    pub item_id: String,
    /// The queued words as loaded, to tell unsaved changes.
    original: String,
    stash: Stash,
    /// The save is out: no second submit, no cancel.
    pub saving: bool,
}

impl Editing {
    fn changed(&self, now: &str) -> bool {
        now.trim() != self.original.trim()
    }
}

#[derive(Default)]
struct Stash {
    text: String,
    pills: Pills,
    files: Vec<StagedFile>,
}

// ── the send button ─────────────────────────────────────────────────────

/// Send, and while a turn runs Stop beside it. With an empty draft while a
/// turn runs only Stop shows. The send button names what a click does now
/// (Steer message / Queue message, flipping while ⌘ is held); with the
/// queue held after Stop and nothing typed it is Resume thread. For 500 ms
/// after Stop the button in its place ignores clicks.
///
/// Both sit in one keyed `send-stop` row whatever shows, so typing while a
/// turn runs brings Send in beside Stop (and Stop back on its own) as the
/// same two buttons popping in and out, never a fresh row.
pub(crate) fn render_send(
    s: &State,
    chat: &ChatState,
    running: bool,
    reduced: bool,
    k: &Kit,
) -> El {
    let o = &chat.orchestration;
    let ready = !chat.staged.is_empty() || !chat.composer.text.trim().is_empty();
    let editing = o.editing.is_some();
    let can_steer = dispatch::can_steer(chat);
    let held_cmd = s.workspace.inbox.cmd_held;
    let mode = dispatch::resolve(running, held_cmd, s.prefs.follow_ups, can_steer);
    let resume = !running && !ready && !editing && o.queue.held && !o.queue.items.is_empty();
    let look = dispatch::send_look(mode, s.prefs.follow_ups, can_steer, editing, resume);
    let mut group = row().key("send-stop").gap(6.).center_y();
    if !running || ready || editing {
        let id = chat.thread_id.clone();
        group = group.child(
            round_button_tinted(
                s,
                "send",
                look.icon,
                ready || resume,
                look.label,
                Some((&chat.send_tint, reduced)),
                k,
            )
            .help(look.help)
            .transition(super::composer::round_pop())
            .on_press_tx(move |s: &mut State, tx| {
                let alternate = tx.mods().cmd;
                press_send(s, &id, resume, alternate, tx)
            }),
        );
    }
    if running {
        let stop_id = chat.thread_id.clone();
        group = group.child(
            round_button_tinted(
                s,
                "stop",
                &icons::SQUARE,
                !ready,
                "Stop",
                Some((&chat.stop_tint, reduced)),
                k,
            )
            .help("Stop")
            .transition(super::composer::round_pop())
            .on_press_tx(move |s: &mut State, tx| press_stop(s, &stop_id, tx)),
        );
    }
    group
}

/// Stop: the turn ends, and the button that takes its place waits out the
/// click guard.
pub(crate) fn press_stop(s: &mut State, id: &str, tx: &mut Tx) {
    if let Some(c) = chat_for(s, id) {
        c.orchestration.stopped_at = Some(Instant::now());
    }
    tx.wake_after(STOP_CLICK_GUARD);
    actions::stop(s, id, tx);
}

/// A click on the send button: Resume thread, or a send (`alternate` with
/// ⌘). Ignored within [`STOP_CLICK_GUARD`] of a Stop.
pub(crate) fn press_send(s: &mut State, id: &str, resume: bool, alternate: bool, tx: &mut Tx) {
    if chat_ref(s, id).is_some_and(|c| c.orchestration.stop_guard()) {
        return;
    }
    if resume {
        super::orchestration::queue_action(s, id, QueueAction::Release);
    } else {
        actions::send_as(s, id, alternate, tx);
    }
}

#[cfg(test)]
#[path = "steer_tests.rs"]
mod tests;
