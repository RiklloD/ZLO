//! "No, do X instead": rejecting an approval with a reason (GPUI
//! `chat/reject_feedback.rs`). The card's "Reject with feedback…" opens a
//! one-line field under it; Enter sends the decline with the text, Esc
//! closes the field and the card keeps asking. Plain Decline stays one click
//! on the card. The backend bounds the text and hands it to the agent as the
//! rejected call's result, so the agent adapts instead of stopping; it never
//! turns the decline into an approval.

use zlo_client::GateDecision;
use rune::{El, KeyEvent, TextEdit, Tx, el, row};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, TextInput};

use super::{ChatState, actions, chat_for, orphan_edit, set_error};
use crate::jobs;
use crate::state::State;

/// The open feedback field: which gate it answers and the text so far.
pub(crate) struct RejectEditor {
    pub(crate) gate_ref: String,
    run: String,
    pub(crate) input: TextEdit,
}

/// Open the feedback field under `gate_ref`'s card (one at a time).
pub(crate) fn start_reject(s: &mut State, thread: &str, run: String, gate_ref: String) {
    let Some(c) = chat_for(s, thread) else { return };
    let mut input = TextEdit::new("");
    input.focused = true;
    c.reject = Some(RejectEditor {
        gate_ref,
        run,
        input,
    });
}

fn cancel_reject(s: &mut State, thread: &str) {
    if let Some(c) = chat_for(s, thread) {
        c.reject = None;
    }
}

/// Send the decline with the field's text; an empty field is a plain
/// decline. A failure gives the card its buttons back and says why.
fn submit_reject(s: &mut State, thread: &str, tx: &mut Tx) {
    let Some(c) = chat_for(s, thread) else { return };
    let Some(editor) = c.reject.take() else {
        return;
    };
    let feedback = editor.input.text.trim().to_string();
    let RejectEditor { gate_ref, run, .. } = editor;
    if feedback.is_empty() {
        actions::resolve_gate(s, thread, run, gate_ref, GateDecision::Decline, tx);
        return;
    }
    c.transcript
        .mark_gate_resolving(&gate_ref, GateDecision::Decline);
    super::refresh_rows(s, thread, tx);
    let thread_id = thread.to_string();
    let req = thread_id.clone();
    let gate = gate_ref.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move {
            c.resolve_gate_with_feedback(&req, &run, &gate, &feedback)
                .await
        },
        move |s, result, tx| {
            if let Err(e) = result
                && let Some(c) = chat_for(s, &thread_id)
            {
                c.transcript.clear_gate_resolving(&gate_ref);
                set_error(s, &thread_id, e.to_string());
                super::refresh_rows(s, &thread_id, tx);
            }
        },
    );
}

fn reject_edit<'a>(s: &'a mut State, thread: &str) -> &'a mut TextEdit {
    match chat_for(s, thread).and_then(|c| c.reject.as_mut()) {
        Some(editor) => &mut editor.input,
        None => orphan_edit(),
    }
}

/// The card's "Reject with feedback…" button.
pub(crate) fn reject_button(
    chat: &ChatState,
    key: &str,
    run_id: &str,
    gate_ref: &str,
    k: &Kit,
) -> El {
    let (run, gate, thread) = (
        run_id.to_string(),
        gate_ref.to_string(),
        chat.thread_id.clone(),
    );
    Button::new("Reject with feedback…")
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Regular)
        .on_press(move |s: &mut State| start_reject(s, &thread, run.clone(), gate.clone()))
        .build(k)
        .key(format!("{key}-reject"))
}

/// The field under a card, when it is open for `gate_ref`.
pub(crate) fn render_reject_editor(chat: &ChatState, gate_ref: &str, k: &Kit) -> Option<El> {
    let editor = chat.reject.as_ref().filter(|e| e.gate_ref == gate_ref)?;
    let (edit_thread, key_thread, submit_thread, cancel_thread) = (
        chat.thread_id.clone(),
        chat.thread_id.clone(),
        chat.thread_id.clone(),
        chat.thread_id.clone(),
    );
    Some(
        row()
            .key(format!("{gate_ref}-reject-editor"))
            .gap(8.)
            .pt(4.)
            .center_y()
            .transition(super::flow::rise(4.))
            .child(
                el().grow(1.).min_w(0.).child(
                    TextInput::new(&editor.input)
                        .placeholder("No — do this instead… (Enter to send, Esc to cancel)")
                        .width(None)
                        .on_edit(move |s: &mut State| reject_edit(s, &edit_thread))
                        // Esc closes the field before the input sees it.
                        .on_key(move |s: &mut State, ev: &KeyEvent, _tx: &mut Tx| {
                            if ev.key == rune::Key::Escape {
                                cancel_reject(s, &key_thread);
                                return true;
                            }
                            false
                        })
                        .on_submit(move |s: &mut State, tx: &mut Tx| {
                            submit_reject(s, &submit_thread, tx)
                        })
                        .build(k),
                ),
            )
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Regular)
                    .on_press(move |s: &mut State| cancel_reject(s, &cancel_thread))
                    .build(k)
                    .key(format!("{gate_ref}-reject-cancel")),
            ),
    )
}
