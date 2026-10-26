//! Steering the running turn from the composer. While a turn runs, Enter
//! sends the message into it: the backend folds it in at the turn's next
//! step, and an engine that cannot steer (Antigravity) queues it instead.
//! Nothing restarts the turn. An approval still waiting is declined, since
//! the new words replace it. Stop sits beside Send; ⌥Enter (or ⌥-click on
//! Send) queues the message to run after the turn.

use ironclaw_gpui_client::attachments::StagedFile;
use ironclaw_gpui_client::{ClientError, GateDecision};

use super::{ChatView, round_button};
use crate::jobs::flatten;
use crate::ui::*;

/// Ack notices that mean "waits in the queue" rather than "steered".
const QUEUED_NOTICE: &str = "Queued";

impl ChatView {
    /// Enter while a turn runs: the message joins that turn, shown as the
    /// person's row inside it rather than as the start of a new turn.
    pub(super) fn steer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() && self.staged.is_empty() {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            self.set_error("This gateway has no chat channel configured.", cx);
            return;
        };
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        self.error = None;
        let files = std::mem::take(&mut self.staged);
        let names: Vec<String> = files.iter().map(|f| f.filename.clone()).collect();
        let pending = self.transcript.push_pending_user(&text, &names);
        self.refresh_rows(cx);

        let gate = self.open_approval();
        let thread_id = self.thread_id.clone();
        let body = text.clone();
        let wire = files.iter().map(StagedFile::to_wire).collect();
        let job = self
            .backend
            .spawn(move |c| async move { c.send_message(&channel, &thread_id, &body, wire).await });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(ack) => {
                    let queued = ack
                        .notice
                        .as_deref()
                        .is_some_and(|n| n.starts_with(QUEUED_NOTICE));
                    if queued {
                        // It shows in the queue panel until the turn ends.
                        this.transcript.drop_pending_user(pending);
                        this.error = ack.notice.map(Into::into);
                    } else {
                        this.transcript.ack_pending_user(pending);
                        if let Some((run, gate_ref, _)) = gate {
                            this.decline_for_steer(run, gate_ref, cx);
                        }
                    }
                    this.refresh_rows(cx);
                    this.load_timeline(cx);
                    this.refresh_orchestration(cx);
                }
                Err(e) => {
                    this.transcript.drop_pending_user(pending);
                    this.staged = files;
                    this.composer
                        .update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    /// Decline the approval the steer replaces. A CLI engine has usually
    /// declined it already (with the person's words as the reason), so a
    /// gate that is gone (404/409) is not an error here. Any other failure
    /// gives the card its buttons back and says why. The native loop gets
    /// no reason text: it reads the person's words as their next message.
    fn decline_for_steer(&mut self, run: String, gate_ref: String, cx: &mut Context<Self>) {
        self.transcript
            .mark_gate_resolving(&gate_ref, GateDecision::Decline);
        let thread_id = self.thread_id.clone();
        let gate = gate_ref.clone();
        let job = self.backend.spawn(move |c| async move {
            c.resolve_gate(&thread_id, &run, &gate, GateDecision::Decline)
                .await
        });
        let task = cx.spawn(async move |this, cx| {
            let Err(e) = flatten(job.await) else {
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
            let _ = this.update(cx, |this, cx| {
                this.transcript.clear_gate_resolving(&gate_ref);
                this.set_error(e.to_string(), cx);
                this.refresh_rows(cx);
            });
        });
        self.keep(task);
    }

    /// ⌥Enter: the message waits in the thread's queue and runs once the
    /// current turn ends. With no turn running it is an ordinary send.
    pub(super) fn send_after_turn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running_run().is_none() {
            return self.send(window, cx);
        }
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.staged.is_empty() {
            self.set_error(
                "Files can't wait in the queue. Press Enter to send them into this turn.",
                cx,
            );
            return;
        }
        if self.queue_follow_up(text, window, cx) {
            self.composer
                .update(cx, |state, cx| state.clean(window, cx));
        }
    }

    /// ⌥Enter in the composer. `true` when it was handled.
    pub(super) fn steer_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keys = &event.keystroke;
        if keys.key != "enter" || !keys.modifiers.alt || keys.modifiers.platform {
            return false;
        }
        self.send_after_turn(window, cx);
        true
    }

    /// Send, and while a turn runs a small Stop beside it. The bright one is
    /// what Enter would do: Stop with an empty composer, Send once there is
    /// something to send.
    pub(super) fn render_send(&self, running: bool, cx: &mut Context<Self>) -> AnyElement {
        let ready = !self.staged.is_empty() || !self.composer.read(cx).value().trim().is_empty();
        if !running {
            // Nothing to send: a neutral disc, not a faded primary (T3 Code).
            return round_button("send", IconName::ArrowUp, ready, "Send", cx)
                .tooltip(|w, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Send (Enter)").build(w, cx)
                })
                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                .into_any_element();
        }
        let stop = round_button("stop", IconName::Square, !ready, "Stop", cx)
            .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Stop").build(w, cx))
            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)));
        let send = round_button(
            "steer-send",
            IconName::ArrowUp,
            ready,
            "Send into this turn",
            cx,
        )
        .tooltip(|w, cx| {
            gpui_kit::component::tooltip::Tooltip::new(
                "Send into this turn (Enter) · after it (⌥Enter)",
            )
            .build(w, cx)
        })
        .on_click(
            cx.listener(|this, event: &gpui_kit::ClickEvent, window, cx| {
                if event.modifiers().alt {
                    this.send_after_turn(window, cx);
                } else {
                    this.send(window, cx);
                }
            }),
        );
        h_flex()
            .gap(px(6.))
            .child(stop)
            .child(send)
            .into_any_element()
    }
}
