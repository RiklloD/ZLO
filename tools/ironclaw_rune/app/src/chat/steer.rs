//! Steering the running turn from the composer (GPUI `chat/steer.rs`).
//! While a turn runs, Enter sends the message into it: the backend folds it
//! in at the turn's next step, and an engine that cannot steer (Antigravity)
//! queues it instead. Nothing restarts the turn. An approval still waiting is
//! declined, since the new words replace it. Stop sits beside Send; ⌥Enter
//! (or ⌥-click on Send) queues the message to run after the turn.

use ironclaw_gpui_client::attachments::StagedFile;
use ironclaw_gpui_client::wire::SubmitAck;
use ironclaw_gpui_client::{ClientError, GateDecision};
use rune::{El, KeyEvent, Tx, row};
use rune_kit::Kit;

use super::{ChatState, actions, chat_for, chat_ref, composer::round_button, inline_files, set_error};
use crate::icons;
use crate::jobs;
use crate::state::State;

/// Ack notices that mean "waits in the queue" rather than "steered".
const QUEUED_NOTICE: &str = "Queued";
/// Ack outcome of a message the running turn refused (`rejected_busy`).
const REJECTED_OUTCOME: &str = "rejected_busy";

/// Enter while a turn runs: the message joins that turn, shown as the
/// person's row inside it rather than as the start of a new turn. `voice` is
/// talk mode's claim on this message: it learns whether the turn took it,
/// queued it behind itself, or refused it.
pub(crate) fn steer(s: &mut State, id: &str, voice: Option<crate::voice::Submission>, tx: &mut Tx) {
    let refuse = |s: &mut State, why: &str| {
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, why);
        }
    };
    let Some(c) = chat_ref(s, id) else { return };
    let (text, files) = inline_files::outgoing(&c.composer, &c.staged);
    if text.is_empty() && files.is_empty() {
        return refuse(s, "There was nothing to send.");
    }
    let Some(channel) = c.channel.clone() else {
        refuse(s, "This gateway has no chat channel configured.");
        set_error(s, id, "This gateway has no chat channel configured.");
        return;
    };
    // The turn this message steers (or would wait behind).
    let steered = c.running_run();
    let gate = c.open_approval();
    let Some(c) = chat_for(s, id) else { return };
    let draft_text = c.composer.text.clone();
    let draft_pills = inline_files::pills(&c.composer);
    c.composer.clear();
    c.error = None;
    c.staged.clear();
    let names: Vec<String> = files.iter().map(|f| f.filename.clone()).collect();
    let pending = c.transcript.push_pending_user(&text, &names);
    actions::record_draft(s, id);
    super::refresh_rows(s, id, tx);

    let thread_id = id.to_string();
    let req = thread_id.clone();
    let body = text.clone();
    let wire = files.iter().map(StagedFile::to_wire).collect();
    let roots = actions::mention_roots(s, id, &text);
    jobs::run(
        s,
        "chat",
        move |c| async move {
            c.send_message_with_roots(&channel, &req, &body, wire, roots)
                .await
        },
        move |s, result, tx| {
            match (&result, voice) {
                (Ok(ack), Some(sub)) => report_voice(s, &thread_id, sub, ack, steered),
                (Err(e), Some(sub)) => crate::voice::submission_refused(s, sub, &e.to_string()),
                (_, None) => {}
            }
            let Some(c) = chat_for(s, &thread_id) else { return };
            match result {
                // The turn refused it: never show it as sent. The words go
                // back in the composer so Stop or ⌥Enter can act on them.
                Ok(ack) if ack.outcome.as_deref() == Some(REJECTED_OUTCOME) => {
                    c.transcript.drop_pending_user(pending);
                    c.staged = files;
                    inline_files::set_content(&mut c.composer, &draft_text, &draft_pills);
                    set_error(
                        s,
                        &thread_id,
                        "The running turn didn't take this message. Press Stop to end it, or ⌥Enter to queue the message for after it.",
                    );
                    super::refresh_rows(s, &thread_id, tx);
                    super::load_timeline(s, &thread_id);
                }
                Ok(ack) => {
                    let queued = ack
                        .notice
                        .as_deref()
                        .is_some_and(|n| n.starts_with(QUEUED_NOTICE));
                    if queued {
                        // It shows in the queue panel until the turn ends.
                        c.transcript.drop_pending_user(pending);
                        c.error = ack.notice;
                    } else {
                        c.transcript.ack_pending_user(pending);
                        if let Some((run, gate_ref, _)) = gate {
                            decline_for_steer(s, &thread_id, run, gate_ref);
                        }
                    }
                    super::refresh_rows(s, &thread_id, tx);
                    super::load_timeline(s, &thread_id);
                    super::orchestration::refresh(s, &thread_id);
                }
                Err(e) => {
                    c.transcript.drop_pending_user(pending);
                    c.staged = files;
                    inline_files::set_content(&mut c.composer, &draft_text, &draft_pills);
                    set_error(s, &thread_id, e.to_string());
                    super::refresh_rows(s, &thread_id, tx);
                }
            }
        },
    );
}

/// Tell talk mode what the gateway made of its message: refused (the
/// running turn was busy), queued behind `running`, or taken, as the run the
/// ack names (a steer joins `running`).
pub(crate) fn report_voice(
    s: &mut State,
    id: &str,
    sub: crate::voice::Submission,
    ack: &SubmitAck,
    running: Option<String>,
) {
    let Some(c) = chat_ref(s, id) else { return };
    let composer = c.composer_key();
    let now_running = c.running_run();
    if ack.outcome.as_deref() == Some(REJECTED_OUTCOME) {
        crate::voice::submission_refused(
            s,
            sub,
            "The running turn didn't take your message; it's back in the composer.",
        );
    } else if ack
        .notice
        .as_deref()
        .is_some_and(|n| n.starts_with(QUEUED_NOTICE))
    {
        crate::voice::submission_queued(s, sub, id, running.or(now_running), &composer);
    } else {
        crate::voice::submission_accepted(s, sub, id, ack.run_id.clone().or(running), &composer);
    }
}

/// Decline the approval the steer replaces. A CLI engine has usually
/// declined it already (with the person's words as the reason), so a gate
/// that is gone (404/409) is not an error here. Any other failure gives the
/// card its buttons back and says why. The native loop gets no reason text:
/// it reads the person's words as their next message.
fn decline_for_steer(s: &mut State, id: &str, run: String, gate_ref: String) {
    let Some(c) = chat_for(s, id) else { return };
    c.transcript
        .mark_gate_resolving(&gate_ref, GateDecision::Decline);
    let thread_id = id.to_string();
    let req = thread_id.clone();
    let gate = gate_ref.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move {
            c.resolve_gate(&req, &run, &gate, GateDecision::Decline)
                .await
        },
        move |s, result, tx| {
            let Err(e) = result else {
                return;
            };
            if matches!(
                e,
                ClientError::Api {
                    status: 404 | 409,
                    ..
                }
            ) {
                tracing::debug!(error = %e, "steer: approval already closed");
                return;
            }
            let Some(c) = chat_for(s, &thread_id) else { return };
            c.transcript.clear_gate_resolving(&gate_ref);
            set_error(s, &thread_id, e.to_string());
            super::refresh_rows(s, &thread_id, tx);
        },
    );
}

/// ⌥Enter: the message waits in the thread's queue and runs once the
/// current turn ends. With no turn running it is an ordinary send.
pub(crate) fn send_after_turn(s: &mut State, id: &str, tx: &mut Tx) {
    let Some(c) = chat_ref(s, id) else { return };
    if c.running_run().is_none() {
        return actions::send(s, id, tx);
    }
    let text = c.composer.text.trim().to_string();
    if text.is_empty() {
        return;
    }
    if !c.staged.is_empty() {
        set_error(
            s,
            id,
            "Files can't wait in the queue. Press Enter to send them into this turn.",
        );
        return;
    }
    if super::orchestration::queue_follow_up(s, id, text)
        && let Some(c) = chat_for(s, id)
    {
        c.composer.clear();
        actions::record_draft(s, id);
    }
}

/// ⌥Enter in the composer. `true` when it was handled.
pub(crate) fn steer_key(s: &mut State, id: &str, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key != rune::Key::Enter || !ev.mods.alt || ev.mods.cmd {
        return false;
    }
    send_after_turn(s, id, tx);
    true
}

/// Send, and while a turn runs a small Stop beside it. The bright one is
/// what Enter would do: Stop with an empty composer, Send once there is
/// something to send.
pub(crate) fn render_send(s: &State, chat: &ChatState, running: bool, k: &Kit) -> El {
    let ready = !chat.staged.is_empty() || !chat.composer.text.trim().is_empty();
    let id = chat.thread_id.clone();
    if !running {
        // Nothing to send: a neutral disc, not a faded primary (T3 Code).
        return round_button(s, "send", &icons::ARROW_UP, ready, "Send", k)
            .help("Send (Enter)")
            .on_press_tx(move |s: &mut State, tx| actions::send(s, &id, tx));
    }
    let stop_id = id.clone();
    let stop = round_button(s, "stop", &icons::SQUARE, !ready, "Stop", k)
        .help("Stop")
        .on_press_tx(move |s: &mut State, tx| actions::stop(s, &stop_id, tx));
    let send = round_button(s, "steer-send", &icons::ARROW_UP, ready, "Send into this turn", k)
        .help("Send into this turn (Enter) · after it (⌥Enter)")
        .on_press_tx(move |s: &mut State, tx| {
            if tx.mods().alt {
                send_after_turn(s, &id, tx);
            } else {
                actions::send(s, &id, tx);
            }
        });
    row().gap(6.).child(stop).child(send)
}
