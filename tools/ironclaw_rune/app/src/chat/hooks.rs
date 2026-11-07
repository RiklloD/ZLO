//! Hooks other areas call on a chat (split out of `chat.rs`).
//!
//! The chat extras (area H) work on any chat, the open one or a side chat
//! in its right pane, so their hooks take the chat's thread id and find it
//! again with [`chat_for`]; a chat that closed meanwhile is left alone. The
//! side pane (S) works on the open chat and passes its id. The voice (F) and
//! workspace (B) hooks route by composer or act on the open chat.

use ironclaw_gpui_client::orchestration::MergedFork;
use ironclaw_gpui_client::wire::TimelineMessage;
use rune::{TextEdit, Tx};

use super::{actions, approval_mode, chat_for, chat_ref, composer, load_timeline, refresh_rows, set_error, snapshot};
use crate::state::State;

/// The open chat's thread id.
fn open_id(s: &State) -> Option<String> {
    s.chat.as_ref().map(|c| c.thread_id.clone())
}

/// Run `f` with a `Tx` for the chat showing `id`, on the next turn of the
/// UI loop (for callers that have none). It finds the chat again by id, so
/// a chat that closed meanwhile is left alone.
pub fn with_tx(s: &State, id: &str, f: impl FnOnce(&mut State, &str, &mut Tx) + Send + 'static) {
    let id = id.to_string();
    s.rt.handle.update(move |s, tx| {
        if chat_ref(s, &id).is_some() {
            f(s, &id, tx);
        }
    });
}

/// The composer field voice dictates into, if that composer exists now:
/// `Chat(id)` is the open chat's (or its side chat's) composer, `Draft`
/// the new-thread screen's. The quick composer's field lives in its own
/// window's state, out of reach here (voice types there with
/// `workspace::quick::insert_text`).
pub fn voice_composer<'a>(s: &'a State, composer: &crate::voice::Composer) -> Option<&'a TextEdit> {
    use crate::voice::Composer;
    match composer {
        Composer::Chat(id) => chat_ref(s, id).map(|c| &c.composer),
        Composer::Draft => s.workspace.draft.as_ref().map(|d| &d.input),
        Composer::Quick => None,
    }
}

/// [`voice_composer`], mutable: voice inserts committed words at the caret
/// and shows the still-changing tail as the field's preedit.
pub fn voice_composer_mut<'a>(s: &'a mut State, composer: &crate::voice::Composer) -> Option<&'a mut TextEdit> {
    use crate::voice::Composer;
    match composer {
        Composer::Chat(id) => chat_for(s, id).map(|c| &mut c.composer),
        Composer::Draft => crate::workspace::composer_edit(s, composer),
        Composer::Quick => None,
    }
}

/// Send what `composer` holds, exactly as pressing Enter in it does. Each
/// send handler takes talk mode's claim (`voice::take_submission`) and
/// reports what became of it.
pub fn voice_press_enter(s: &mut State, composer: &crate::voice::Composer, tx: &mut Tx) {
    use crate::voice::Composer;
    match composer {
        Composer::Chat(id) => {
            let id = id.clone();
            actions::send(s, &id, tx);
        }
        Composer::Draft => crate::workspace::send_draft(s),
        Composer::Quick => {}
    }
}

/// A SnapShot finished (area F): the workspace decides where it lands
/// (`workspace::stage_snapshot`), and for the open thread calls this, which
/// stages it in the open chat's composer. An `Err` becomes its error line.
pub fn stage_snapshot(s: &mut State, snap: Result<crate::snapshot::Snapshot, String>) {
    let Some(id) = open_id(s) else {
        if let Err(e) = snap {
            tracing::warn!(error = %e, "SnapShot failed with no chat open");
        }
        return;
    };
    snapshot::stage_snapshot(s, &id, snap);
}

/// Show `message` in the open chat's error line (GPUI `ChatView::set_error`).
/// The extras report into their own chat with `chat::report_error`.
pub fn open_error(s: &mut State, message: impl Into<String>) {
    match open_id(s) {
        Some(id) => set_error(s, &id, message),
        None => tracing::warn!(error = %message.into(), "chat error with no chat open"),
    }
}

/// Re-read the open chat's timeline.
pub fn reload_timeline(s: &mut State) {
    if let Some(id) = open_id(s) {
        load_timeline(s, &id);
    }
}

/// The run streaming in `id`'s chat (IronClaw's loop), if any.
pub fn active_run(s: &State, id: &str) -> Option<String> {
    let c = chat_ref(s, id)?;
    c.transcript
        .active_run()
        .or(c.fixture_run.as_deref())
        .map(str::to_string)
}

/// `id`'s chat is a side chat drawn in its parent's pane.
pub fn embedded(s: &State, id: &str) -> bool {
    chat_ref(s, id).is_some_and(|c| c.embedded)
}

/// `id`'s composer text.
pub fn composer_text(s: &State, id: &str) -> String {
    chat_ref(s, id).map(|c| c.composer.text.clone()).unwrap_or_default()
}

/// Put `text` into `id`'s composer (caret at its end) and note the draft.
pub fn set_composer_text(s: &mut State, id: &str, text: String) {
    let Some(c) = chat_for(s, id) else { return };
    c.composer.replace_text(text);
    composer::on_change(s, id);
}

/// A fork's source conversation, shown above its own messages.
pub fn set_fork_prefix(s: &mut State, id: &str, prefix: Vec<TimelineMessage>) {
    let Some(c) = chat_for(s, id) else { return };
    c.transcript.set_prefix(prefix);
    with_tx(s, id, refresh_rows);
}

/// Forks merged back into this thread, shown where they arrived.
pub fn set_merges(s: &mut State, id: &str, merges: Vec<MergedFork>) {
    let Some(c) = chat_for(s, id) else { return };
    c.transcript.set_merges(merges);
    with_tx(s, id, refresh_rows);
}

/// Stop `id`'s turn (computer use's Stop does this too).
pub fn stop_run(s: &mut State, id: &str) {
    with_tx(s, id, actions::stop);
}

/// The global auto-approve switch changed (Settings, Profiles): the
/// approval pills of the open chat and its side chat re-read it (GPUI
/// `approval_mode::changed`).
pub fn approval_mode_changed(s: &mut State) {
    approval_mode::changed(s);
}

/// Focus the open chat's composer (the window came back, a picker closed,
/// the thread was opened again).
pub fn focus_composer(s: &mut State) {
    if let Some(id) = open_id(s) {
        focus_composer_of(s, &id);
    }
}

/// Focus `id`'s composer, the open chat's or a side chat's (Esc closed
/// that chat's picker).
pub fn focus_composer_of(s: &mut State, id: &str) {
    if s.page != crate::state::Page::Chat {
        return;
    }
    if let Some(c) = chat_for(s, id) {
        c.composer.focused = true;
    }
}
