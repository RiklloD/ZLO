//! Follow-ups while a turn runs, as t3code does them. Enter does Settings →
//! Follow-ups (Steer by default): the message joins the running turn and
//! shows inside it with a "Steer" marker. ⌘↵ or ⌘-click on the send button
//! does the other (queue) for one message. Nothing restarts the turn, and
//! the backend declines an approval the steer replaces. Steering waits only
//! for compaction; then the message queues.
//!
//! What a send became comes from the ack's typed `delivery`
//! ([`Landed`]), never from its notice text or a guess at the running run.
//!
//! The queue panel's Send now, Edit and the shortcuts (⇧⌘↵ sends the first
//! queued message, ⌥↑ edits the last) are here too. The gateway's queue
//! route has no promote action, so Send now removes the item and sends its
//! words as a steer (or a new turn, idle) in one step.

use rune::{El, Tx, row};
use rune_kit::Kit;
use zlo_client::attachments::StagedFile;
use zlo_client::orchestration::QueueAction;
use zlo_client::wire::{Landed, SubmitAck};

use super::dispatch::{self, Mode};
use super::{
    ChatState, actions, chat_for, chat_ref, composer::round_button, inline_files, set_error,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

/// A message on its way out, with what the composer held, so a failed send
/// can put it back.
pub(crate) struct Draft {
    text: String,
    files: Vec<StagedFile>,
    draft_text: String,
    draft_pills: Vec<(std::ops::Range<usize>, String)>,
    /// The words are a queued item's (Send now): a failed send puts them
    /// back in the queue rather than the composer.
    from_queue: bool,
}

impl Draft {
    /// A message not typed in the composer (a queued one sent now).
    fn words(text: String) -> Self {
        Self {
            draft_text: text.clone(),
            text,
            files: Vec::new(),
            draft_pills: Vec::new(),
            from_queue: true,
        }
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
        dispatch::can_steer(&c.rows),
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

/// Take the composer's message, emptying it. `None` when there is nothing.
fn take(s: &mut State, id: &str) -> Option<Draft> {
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
        from_queue: false,
    };
    c.composer.clear();
    c.error = None;
    c.staged.clear();
    actions::record_draft(s, id);
    Some(draft)
}

/// The message joins the running turn, shown as the person's row inside
/// it. `voice` is talk mode's claim on it.
fn steer(s: &mut State, id: &str, voice: Option<crate::voice::Submission>, tx: &mut Tx) {
    match take(s, id) {
        Some(draft) => deliver(s, id, draft, voice, tx),
        None => {
            if let Some(sub) = voice {
                crate::voice::submission_refused(s, sub, "There was nothing to send.");
            }
        }
    }
}

/// Send `draft` through the chat channel. While a turn runs its row shows
/// as a steer, waiting to be read; the ack says where it really went.
fn deliver(
    s: &mut State,
    id: &str,
    draft: Draft,
    voice: Option<crate::voice::Submission>,
    tx: &mut Tx,
) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(channel) = c.channel.clone() else {
        if let Some(sub) = voice {
            crate::voice::submission_refused(
                s,
                sub,
                "This gateway has no chat channel configured.",
            );
        }
        rescue(s, id, draft, "This gateway has no chat channel configured.".into());
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
            note_steer_id(s, &thread_id, pending, result.as_ref().ok());
            let landed = result
                .as_ref()
                .map(SubmitAck::landed)
                .map_err(|e| e.to_string());
            apply_landed(s, &thread_id, pending, landed, draft, tx);
        },
    );
}

/// The ack's steer id keys the pending row to its stored copy.
fn note_steer_id(s: &mut State, id: &str, pending: u64, ack: Option<&SubmitAck>) {
    if let (Some(c), Some(steer)) = (chat_for(s, id), ack.and_then(|a| a.steer_id.clone())) {
        c.transcript.set_pending_steer_id(pending, Some(steer));
    }
}

/// The gateway answered a send: settle its row by where it went.
fn apply_landed(
    s: &mut State,
    thread_id: &str,
    pending: u64,
    landed: Result<Landed, String>,
    draft: Draft,
    tx: &mut Tx,
) {
    let Some(c) = chat_for(s, thread_id) else {
        // The chat closed meanwhile: a send that failed still keeps its
        // words, in the thread's saved draft.
        match landed {
            Ok(Landed::Refused) => rescue(s, thread_id, draft, "The running turn didn't take this message.".into()),
            Err(e) => rescue(s, thread_id, draft, e),
            Ok(_) => {}
        }
        return;
    };
    let failure = match landed {
        // The turn refused it: never show it as sent. The words go back in
        // the composer so Stop or ⌘↵ can act on them.
        Ok(Landed::Refused) => Some((
            "The running turn didn't take this message. Press Stop to end it, or ⌘↵ to queue it for after the turn.".to_string(),
            true,
        )),
        Err(e) => Some((e, false)),
        Ok(landed) => {
            match landed {
                // It waits in the queue panel until the turn ends.
                Landed::Queued => c.transcript.drop_pending_user(pending),
                Landed::Steered { .. } => {
                    c.transcript.ack_pending_user(pending);
                    c.transcript.set_pending_steer(pending, Some(true));
                }
                // Too late to steer: it started the next turn.
                Landed::Started | Landed::Refused => {
                    c.transcript.ack_pending_user(pending);
                    c.transcript.set_pending_steer(pending, None);
                }
            }
            super::refresh_rows(s, thread_id, tx);
            super::load_timeline(s, thread_id);
            super::refresh_extras(s, thread_id);
            None
        }
    };
    let Some((error, reload)) = failure else {
        return;
    };
    if let Some(c) = chat_for(s, thread_id) {
        c.transcript.drop_pending_user(pending);
    }
    rescue(s, thread_id, draft, error);
    super::refresh_rows(s, thread_id, tx);
    if reload {
        super::load_timeline(s, thread_id);
    }
}

/// Tell talk mode what the gateway made of its message: refused, queued
/// behind `running`, or taken (a steer joins `running`).
pub(crate) fn report_voice(
    s: &mut State,
    id: &str,
    sub: crate::voice::Submission,
    ack: &SubmitAck,
    running: Option<String>,
) {
    let Some(c) = chat_ref(s, id) else { return };
    let composer = crate::voice::Composer::Chat(id.to_string());
    let now_running = c.running_run();
    match ack.landed() {
        Landed::Refused => crate::voice::submission_refused(
            s,
            sub,
            "The running turn didn't take your message; it's back in the composer.",
        ),
        Landed::Queued => {
            crate::voice::submission_queued(s, sub, id, running.or(now_running), composer)
        }
        Landed::Steered { .. } | Landed::Started => {
            crate::voice::submission_accepted(s, sub, id, ack.run_id.clone().or(running), composer)
        }
    }
}

/// The message waits in the thread's queue for the turn to end.
fn queue(s: &mut State, id: &str, voice: Option<crate::voice::Submission>) {
    let Some(c) = chat_ref(s, id) else { return };
    if !c.staged.is_empty() {
        // The queue route stores words only.
        let why = "Files can't wait in the queue yet. Steer them into this turn instead.";
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, why);
        }
        return set_error(s, id, why);
    }
    let running = c.running_run();
    let Some(draft) = take(s, id) else {
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, "There was nothing to send.");
        }
        return;
    };
    if !super::orchestration::queue_follow_up(s, id, draft.text.clone()) {
        // Another queue change is in flight: the words stay in the composer.
        if let Some(c) = chat_for(s, id) {
            actions::put_back(c, &draft.draft_text, &draft.draft_pills, Vec::new());
        }
        actions::record_draft(s, id);
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, "The queue is busy; try again.");
        }
        return;
    }
    if let Some(sub) = voice {
        let composer = crate::voice::Composer::Chat(id.to_string());
        crate::voice::submission_queued(s, sub, id, running, composer);
    }
}

// ── the queue panel's actions ───────────────────────────────────────────

/// A queued message out of the queue and into the conversation: a steer of
/// the running turn, or a new turn when none runs (t3code's Send now).
pub(crate) fn send_now(s: &mut State, id: &str, item_id: &str, tx: &mut Tx) {
    let Some(c) = chat_ref(s, id) else { return };
    let o = &c.orchestration;
    if o.busy {
        return;
    }
    let Some(item) = o.queue.items.iter().find(|i| i.id == item_id) else {
        return;
    };
    let text = item.text.clone();
    take_from_queue(s, id, item_id.to_string(), Draft::words(text), tx);
}

/// Remove `item_id` from the queue, then send `draft`, in one job, so the
/// pair finishes even if the chat closes meanwhile. Removing first means
/// the queue can never run it too. No words are lost on any failure: a
/// failed remove leaves a queued item queued (and puts edited words back in
/// the composer); a send that fails after the remove puts a queued item's
/// words back in the queue, and anything else back in the composer.
fn take_from_queue(s: &mut State, id: &str, item_id: String, draft: Draft, tx: &mut Tx) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(channel) = c.channel.clone() else {
        rescue(s, id, draft, "This gateway has no chat channel configured.".into());
        return;
    };
    let o = &mut c.orchestration;
    o.busy = true;
    let instance = super::orchestration::instance_of_state(o);
    let running = c.running_run();
    let names: Vec<String> = draft.files.iter().map(|f| f.filename.clone()).collect();
    let pending = c.transcript.push_pending_user(&draft.text, &names);
    if running.is_some() {
        c.transcript.set_pending_steer(pending, Some(true));
    }
    super::refresh_rows(s, id, tx);
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    let body = draft.text.clone();
    let wire: Vec<_> = draft.files.iter().map(StagedFile::to_wire).collect();
    let roots = actions::mention_roots(s, id, &draft.text);
    let requeue = draft.from_queue && draft.files.is_empty();
    jobs::run(
        s,
        "orchestration",
        move |c| async move {
            let queue = match c.mutate_queue(&thread_id, &QueueAction::Remove(item_id)).await {
                Ok(queue) => queue,
                Err(e) => return Ok(Taken::NotRemoved(e.to_string())),
            };
            match c.send_message_with_roots(&channel, &thread_id, &body, wire, roots).await {
                Ok(ack) => Ok(Taken::Sent(queue, ack)),
                Err(e) if requeue => {
                    let back = c.mutate_queue(&thread_id, &QueueAction::Add(body)).await;
                    Ok(Taken::Requeued(e.to_string(), back.map_err(|e| e.to_string())))
                }
                Err(e) => Ok(Taken::SendFailed(e.to_string())),
            }
        },
        move |s, result: Result<Taken, zlo_client::ClientError>, tx| {
            let taken = result.unwrap_or_else(|e| Taken::NotRemoved(e.to_string()));
            settle_taken(s, &asked, instance, pending, taken, draft, tx);
        },
    );
}

/// A remove-then-send finished: settle the row, and keep the words
/// wherever the outcome leaves them unsent.
fn settle_taken(
    s: &mut State,
    asked: &str,
    instance: jobs::Instance,
    pending: u64,
    taken: Taken,
    draft: Draft,
    tx: &mut Tx,
) {
    let asked = asked.to_string();
    let current = super::orchestration::is_current(s, &asked, instance);
    if current && let Some(c) = chat_for(s, &asked) {
        c.orchestration.busy = false;
    }
    match taken {
            Taken::Sent(queue, ack) => {
                if current && let Some(c) = chat_for(s, &asked) {
                    c.orchestration.queue = queue;
                }
                note_steer_id(s, &asked, pending, Some(&ack));
                apply_landed(s, &asked, pending, Ok(ack.landed()), draft, tx);
            }
            Taken::NotRemoved(e) => {
                drop_pending(s, &asked, pending, tx);
                let why = format!("Could not send the queued message: {e}");
                if draft.from_queue {
                    // Still queued: nothing left the queue.
                    crate::chat::report_error(s, &asked, why);
                } else {
                    rescue(s, &asked, draft, why);
                }
            }
            Taken::Requeued(e, Ok(queue)) => {
                drop_pending(s, &asked, pending, tx);
                if current && let Some(c) = chat_for(s, &asked) {
                    c.orchestration.queue = queue;
                }
                crate::chat::report_error(
                    s,
                    &asked,
                    format!("Could not send the queued message: {e}. It is back at the end of the queue."),
                );
            }
            Taken::Requeued(e, Err(_)) | Taken::SendFailed(e) => {
                drop_pending(s, &asked, pending, tx);
                rescue(s, &asked, draft, e);
            }
        }
    super::orchestration::ensure_poll(s, &asked);
}

/// How a remove-then-send went.
enum Taken {
    NotRemoved(String),
    Sent(zlo_client::orchestration::QueueDoc, SubmitAck),
    /// The send failed; the words went back in the queue (or failed to).
    Requeued(String, Result<zlo_client::orchestration::QueueDoc, String>),
    SendFailed(String),
}

fn drop_pending(s: &mut State, id: &str, pending: u64, tx: &mut Tx) {
    if let Some(c) = chat_for(s, id) {
        c.transcript.drop_pending_user(pending);
        super::refresh_rows(s, id, tx);
    }
}

/// Unsent words go back where the person finds them, never lost: into the
/// open chat's composer (below anything typed since, see
/// [`actions::put_back`]), or, with the chat closed, into the thread's
/// saved draft. `error` is shown with them.
pub(crate) fn rescue(s: &mut State, id: &str, draft: Draft, error: String) {
    rescue_words(s, id, &draft.draft_text, &draft.draft_pills, draft.files, error);
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
            d.tokens.into_iter().map(|t| (t.range, t.id)).collect::<Vec<_>>(),
            d.files,
        ),
        None => (String::new(), Vec::new(), Vec::new()),
    };
    let (joined, joined_pills) = actions::join_drafts(&old_text, &old_pills, text, pills);
    old_files.extend(files);
    let tokens = joined_pills
        .into_iter()
        .filter_map(|(range, id)| {
            let shown = joined.get(range.clone())?.to_string();
            Some(crate::drafts::DraftToken {
                range,
                id,
                label: shown.clone(),
                text: shown,
            })
        })
        .collect();
    crate::drafts::record(
        s,
        id,
        crate::drafts::Draft {
            text: joined,
            tokens,
            files: old_files,
            profile: None,
        },
    );
    tracing::debug!(%error, "unsent words kept in the closed chat's draft");
}

/// ⇧⌘↵: send the first queued message now. `false` when there is none.
pub(crate) fn send_first_queued(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    let first =
        chat_ref(s, id).and_then(|c| c.orchestration.queue.items.first().map(|i| i.id.clone()));
    match first {
        Some(item) => {
            send_now(s, id, &item, tx);
            true
        }
        None => false,
    }
}

/// Load a queued message into the composer to edit it. What the composer
/// held waits aside and comes back when the edit is saved or cancelled.
/// Moving to another queued message while this one has unsaved changes is
/// refused, so the changes are never dropped.
pub(crate) fn edit_queued(s: &mut State, id: &str, item_id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(item) = c.orchestration.queue.items.iter().find(|i| i.id == item_id) else {
        return;
    };
    let text = item.text.clone();
    if let Some(editing) = &c.orchestration.editing {
        if editing.item_id == item_id {
            return;
        }
        if editing.changed(&c.composer.text) {
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
    });
    c.error = None;
    c.composer.replace_text(text);
    super::composer::on_change(s, id);
    super::focus_composer_of(s, id);
}

/// ⌥↑: edit the last queued message. `false` when there is none.
pub(crate) fn edit_last_queued(s: &mut State, id: &str) -> bool {
    let last =
        chat_ref(s, id).and_then(|c| c.orchestration.queue.items.last().map(|i| i.id.clone()));
    match last {
        Some(item) => {
            edit_queued(s, id, &item);
            true
        }
        None => false,
    }
}

/// Cancel: leave the edit, dropping its unsaved changes (the queued message
/// keeps its words); the composer gets back what it held before.
pub(crate) fn cancel_edit(s: &mut State, id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(editing) = c.orchestration.editing.take() else {
        return;
    };
    let Stash { text, pills, files } = editing.stash;
    c.composer.clear();
    c.staged.clear();
    actions::put_back(c, &text, &pills, files);
    super::composer::on_change(s, id);
}

/// Esc while editing: leaves an unchanged edit; with changes it keeps them
/// and says how to drop them (only Cancel discards words). `true` when
/// handled.
pub(crate) fn escape_edit(s: &mut State, id: &str) -> bool {
    let Some(c) = chat_ref(s, id) else { return false };
    let Some(editing) = &c.orchestration.editing else {
        return false;
    };
    if editing.changed(&c.composer.text) {
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
/// alternate resolving to steer (⌘↵ while a turn runs), save it into the
/// running turn as a steer. An empty edit changes nothing.
pub(crate) fn submit_edit(s: &mut State, id: &str, alternate: bool, tx: &mut Tx) {
    let mode = mode_of(s, id, alternate);
    let Some(c) = chat_ref(s, id) else { return };
    let Some(item_id) = c.orchestration.editing.as_ref().map(|e| e.item_id.clone()) else {
        return;
    };
    if c.orchestration.busy {
        return;
    }
    let (text, files) = inline_files::outgoing(&c.composer, &c.staged);
    if text.trim().is_empty() && files.is_empty() {
        return;
    }
    let still_queued = c.orchestration.queue.items.iter().any(|i| i.id == item_id);
    if !still_queued {
        // The edit stays in the composer; what it held before comes back
        // below it.
        restore_stash(s, id);
        set_error(
            s,
            id,
            "That message already left the queue; your edited text is still here.",
        );
        return;
    }
    if alternate && mode == Mode::Steer {
        let draft = take(s, id);
        restore_stash(s, id);
        if let Some(draft) = draft {
            take_from_queue(s, id, item_id, draft, tx);
        }
        super::refresh_rows(s, id, tx);
        return;
    }
    if !files.is_empty() {
        return set_error(s, id, "Files can't be added to a queued message yet.");
    }
    let Some(draft) = take(s, id) else { return };
    restore_stash(s, id);
    save_edit(s, id, item_id, draft);
}

/// Store an edited queued message. A failed save puts the edited words
/// back (the queued message keeps its old ones).
fn save_edit(s: &mut State, id: &str, item_id: String, draft: Draft) {
    let Some(c) = chat_for(s, id) else { return };
    let o = &mut c.orchestration;
    o.busy = true;
    o.note = None;
    let instance = super::orchestration::instance_of_state(o);
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    let action = QueueAction::Edit {
        item_id,
        text: draft.text.clone(),
    };
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.mutate_queue(&thread_id, &action).await },
        move |s, result, _| {
            settle_edit(s, &asked, instance, result.map_err(|e| e.to_string()), draft)
        },
    );
}

/// An edit's save finished. A failed one puts the edited words back.
fn settle_edit(
    s: &mut State,
    asked: &str,
    instance: jobs::Instance,
    result: Result<zlo_client::orchestration::QueueDoc, String>,
    draft: Draft,
) {
    let current = super::orchestration::is_current(s, asked, instance);
    if current && let Some(c) = chat_for(s, asked) {
        c.orchestration.busy = false;
    }
    match result {
        Ok(queue) => {
            if current && let Some(c) = chat_for(s, asked) {
                c.orchestration.queue = queue;
            }
        }
        Err(e) => rescue(
            s,
            asked,
            draft,
            format!("Could not save the queued message: {e}"),
        ),
    }
    super::orchestration::ensure_poll(s, asked);
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
/// the set-aside composer, with the edit below it when it has unsaved
/// changes. Leaving the chat mid-edit (a thread switch) then keeps both.
pub(crate) fn draft_while_editing(
    c: &ChatState,
) -> Option<(String, Vec<(std::ops::Range<usize>, String)>, Vec<StagedFile>)> {
    let editing = c.orchestration.editing.as_ref()?;
    let stash = &editing.stash;
    let mut files = stash.files.clone();
    if !editing.changed(&c.composer.text) {
        return Some((stash.text.clone(), stash.pills.clone(), files));
    }
    let (text, pills) = actions::join_drafts(
        &stash.text,
        &stash.pills,
        &c.composer.text,
        &inline_files::pills(&c.composer),
    );
    files.extend(c.staged.iter().cloned());
    Some((text, pills, files))
}

/// A queued message in the composer, and what the composer held before.
pub(crate) struct Editing {
    pub item_id: String,
    /// The queued words as loaded, to tell unsaved changes.
    original: String,
    stash: Stash,
}

impl Editing {
    fn changed(&self, now: &str) -> bool {
        now.trim() != self.original.trim()
    }
}

#[derive(Default)]
struct Stash {
    text: String,
    pills: Vec<(std::ops::Range<usize>, String)>,
    files: Vec<StagedFile>,
}

// ── the send button ─────────────────────────────────────────────────────

/// Send, and while a turn runs Stop beside it. With an empty draft while a
/// turn runs only Stop shows. The send button names what a click does now
/// (Steer message / Queue message, flipping while ⌘ is held); with the
/// queue held after Stop and nothing typed it is Resume thread.
pub(crate) fn render_send(s: &State, chat: &ChatState, running: bool, k: &Kit) -> El {
    let o = &chat.orchestration;
    let ready = !chat.staged.is_empty() || !chat.composer.text.trim().is_empty();
    let editing = o.editing.is_some();
    let can_steer = dispatch::can_steer(&chat.rows);
    let held_cmd = s.workspace.inbox.cmd_held;
    let mode = dispatch::resolve(running, held_cmd, s.prefs.follow_ups, can_steer);
    let resume = !running && !ready && !editing && o.queue.held && !o.queue.items.is_empty();
    let look = dispatch::send_look(mode, s.prefs.follow_ups, can_steer, editing, resume);
    let id = chat.thread_id.clone();
    let send = round_button(s, "send", look.icon, ready || resume, look.label, k)
        .help(look.help)
        .on_press_tx(move |s: &mut State, tx| {
            if resume {
                super::orchestration::queue_action(s, &id, QueueAction::Release);
            } else {
                actions::send_as(s, &id, tx.mods().cmd, tx);
            }
        });
    if !running {
        return send;
    }
    let stop_id = chat.thread_id.clone();
    let stop = round_button(s, "stop", &icons::SQUARE, !ready, "Stop", k)
        .help("Stop")
        .on_press_tx(move |s: &mut State, tx| actions::stop(s, &stop_id, tx));
    if !ready && !editing {
        return stop;
    }
    row().gap(6.).child(send).child(stop)
}

#[cfg(test)]
mod tests {
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

    fn pending_steers(s: &State) -> Vec<(String, Option<bool>)> {
        chat(s)
            .transcript
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::User {
                    text,
                    steer,
                    pending: true,
                    ..
                } => Some((text, steer.map(|m| m.waiting))),
                _ => None,
            })
            .collect()
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
            pending_steers(&s),
            vec![("also check clippy".into(), Some(true))],
            "shown inside the turn as a steer, waiting to be read"
        );
        assert!(!chat(&s).orchestration.busy, "nothing was queued");
    }

    #[test]
    fn the_alternate_queues_and_the_queue_setting_flips_both() {
        let mut s = test_state();
        running(&mut s);
        let mut tx = Tx::default();
        actions::send_as(&mut s, ID, true, &mut tx);
        assert!(chat(&s).orchestration.busy, "⌘↵ queues it");
        assert!(
            pending_steers(&s).is_empty(),
            "a queued message is not in the turn"
        );
        assert!(chat(&s).composer.text.is_empty());

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
        assert_eq!(pending_steers(&s).len(), 1, "⌘↵ steers under Queue");
    }

    #[test]
    fn files_steer_but_cannot_wait_in_the_queue_yet() {
        let mut s = test_state();
        running(&mut s);
        let file = StagedFile::from_bytes("a.txt".into(), "text/plain".into(), b"hi".to_vec());
        if let Some(c) = chat_for(&mut s, ID) {
            c.staged.push(file.clone());
        }
        let mut tx = Tx::default();
        actions::send_as(&mut s, ID, true, &mut tx);
        assert!(
            chat(&s)
                .error
                .as_deref()
                .is_some_and(|e| e.contains("Files can't wait"))
        );
        assert_eq!(
            chat(&s).composer.text,
            "also check clippy",
            "nothing was lost"
        );
        assert_eq!(chat(&s).staged.len(), 1);
        // Steered, the files go with the words.
        actions::send_as(&mut s, ID, false, &mut tx);
        assert!(chat(&s).staged.is_empty());
        assert_eq!(pending_steers(&s).len(), 1);
    }

    #[test]
    fn editing_a_queued_message_stashes_the_draft_and_saves_in_place() {
        let mut s = test_state();
        running(&mut s);
        if let Some(c) = chat_for(&mut s, ID) {
            c.orchestration.queue = queue_of(&["first", "second"]);
        }
        assert!(edit_last_queued(&mut s, ID));
        assert_eq!(chat(&s).composer.text, "second");
        let editing = chat(&s)
            .orchestration
            .editing
            .as_ref()
            .map(|e| e.item_id.clone());
        assert_eq!(editing.as_deref(), Some("q1"));
        cancel_edit(&mut s, ID);
        assert_eq!(
            chat(&s).composer.text,
            "also check clippy",
            "the draft came back"
        );
        assert!(chat(&s).orchestration.editing.is_none());

        edit_queued(&mut s, ID, "q0");
        if let Some(c) = chat_for(&mut s, ID) {
            c.composer.replace_text("first, reworded");
        }
        let mut tx = Tx::default();
        actions::send_as(&mut s, ID, false, &mut tx);
        assert!(chat(&s).orchestration.busy, "Enter saves the edit");
        assert!(chat(&s).orchestration.editing.is_none());
        assert_eq!(chat(&s).composer.text, "also check clippy");
        assert!(pending_steers(&s).is_empty(), "a save sends nothing");
    }

    #[test]
    fn the_alternate_while_editing_saves_into_the_turn_as_a_steer() {
        let mut s = test_state();
        running(&mut s);
        if let Some(c) = chat_for(&mut s, ID) {
            c.orchestration.queue = queue_of(&["first"]);
        }
        edit_queued(&mut s, ID, "q0");
        let mut tx = Tx::default();
        actions::send_as(&mut s, ID, true, &mut tx);
        // The item is removed first; its words steer once that lands.
        assert!(chat(&s).orchestration.busy);
        assert!(chat(&s).orchestration.editing.is_none());
        assert_eq!(chat(&s).composer.text, "also check clippy");
    }

    #[test]
    fn send_now_and_the_shortcut_take_the_first_queued_message() {
        let mut s = test_state();
        running(&mut s);
        assert!(!send_first_queued(&mut s, ID, &mut Tx::default()), "nothing queued");
        if let Some(c) = chat_for(&mut s, ID) {
            c.orchestration.queue = queue_of(&["first", "second"]);
        }
        assert!(send_first_queued(&mut s, ID, &mut Tx::default()));
        assert!(chat(&s).orchestration.busy);
        assert_eq!(
            chat(&s).composer.text,
            "also check clippy",
            "the draft stays"
        );
    }

    /// A steer in flight, as `deliver` leaves it.
    fn in_flight(s: &mut State) -> (u64, Draft) {
        let draft = take(s, ID).expect("a draft");
        let c = chat_for(s, ID).expect("chat");
        let pending = c.transcript.push_pending_user(&draft.text, &[]);
        c.transcript.set_pending_steer(pending, Some(true));
        (pending, draft)
    }

    #[test]
    fn the_typed_delivery_settles_the_row() {
        let mut tx = Tx::default();
        let mut s = test_state();
        running(&mut s);
        let (pending, draft) = in_flight(&mut s);
        apply_landed(
            &mut s,
            ID,
            pending,
            Ok(Landed::Steered { waiting: false }),
            draft,
            &mut tx,
        );
        let rows = chat(&s).transcript.rows();
        assert!(
            rows.iter().any(|r| matches!(r,
            Row::User { steer: Some(m), pending: false, .. } if m.waiting)),
            "kept in the turn, waiting to be read until the timeline says"
        );

        let mut s = test_state();
        running(&mut s);
        let (pending, draft) = in_flight(&mut s);
        apply_landed(&mut s, ID, pending, Ok(Landed::Queued), draft, &mut tx);
        assert!(
            !chat(&s)
                .transcript
                .rows()
                .iter()
                .any(|r| matches!(r, Row::User { key, .. } if key.starts_with("pending:"))),
            "a queued message shows in the queue panel, not the turn"
        );

        let mut s = test_state();
        running(&mut s);
        let (pending, draft) = in_flight(&mut s);
        apply_landed(&mut s, ID, pending, Ok(Landed::Started), draft, &mut tx);
        assert!(
            chat(&s).transcript.rows().iter().any(|r| matches!(r,
            Row::User { key, steer: None, .. } if key.starts_with("pending:"))),
            "too late to steer: it is the next turn's prompt"
        );

        let mut s = test_state();
        running(&mut s);
        let (pending, draft) = in_flight(&mut s);
        apply_landed(&mut s, ID, pending, Ok(Landed::Refused), draft, &mut tx);
        assert_eq!(
            chat(&s).composer.text,
            "also check clippy",
            "refused: back in the composer"
        );
        assert!(
            chat(&s)
                .error
                .as_deref()
                .is_some_and(|e| e.contains("⌘↵ to queue"))
        );
    }

    #[test]
    fn a_compacting_turn_queues_even_on_steer() {
        let mut s = test_state();
        running(&mut s);
        if let Some(c) = chat_for(&mut s, ID) {
            let mut rows = (*c.rows).clone();
            rows.push(Row::User {
                key: "msg:c".into(),
                text: "/compact".into(),
                pending: false,
                attachments: Vec::new(),
                steer: None,
            });
            c.rows = std::rc::Rc::new(rows);
        }
        assert_eq!(mode_of(&s, ID, false), Mode::Queue);
        assert_eq!(mode_of(&s, ID, true), Mode::Queue);
    }
}
