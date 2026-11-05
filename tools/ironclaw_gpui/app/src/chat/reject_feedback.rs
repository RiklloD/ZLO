//! "No, do X instead": rejecting an approval with a reason. The card's
//! "Reject with feedback…" opens a one-line field under it; Enter sends the
//! decline with the text, Esc closes the field and the card keeps asking.
//! Plain Decline stays one click on the card. The backend bounds the text and
//! hands it to the agent as the rejected call's result, so the agent adapts
//! instead of stopping; it never turns the decline into an approval.

use ironclaw_gpui_client::GateDecision;

use super::ChatView;
use crate::jobs::flatten;
use crate::ui::*;

/// The open feedback field: which gate it answers and the text so far.
pub(super) struct RejectEditor {
    pub(super) gate_ref: String,
    run: String,
    pub(super) input: Entity<InputState>,
    _sub: Subscription,
}

impl ChatView {
    /// Open the feedback field under `gate_ref`'s card (one at a time).
    pub(super) fn start_reject(
        &mut self,
        run: String,
        gate_ref: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("No — do this instead… (Enter to send, Esc to cancel)")
        });
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit_reject(cx);
            }
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.reject = Some(RejectEditor {
            gate_ref,
            run,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    pub(super) fn cancel_reject(&mut self, cx: &mut Context<Self>) {
        if self.reject.take().is_some() {
            cx.notify();
        }
    }

    /// Send the decline with the field's text; an empty field is a plain
    /// decline. A failure gives the card its buttons back and says why.
    fn submit_reject(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.reject.take() else {
            return;
        };
        let feedback = editor.input.read(cx).value().trim().to_string();
        let RejectEditor { gate_ref, run, .. } = editor;
        if feedback.is_empty() {
            self.resolve_gate(run, gate_ref, GateDecision::Decline, cx);
            return;
        }
        self.transcript
            .mark_gate_resolving(&gate_ref, GateDecision::Decline);
        self.refresh_rows(cx);
        let thread_id = self.thread_id.clone();
        let gate = gate_ref.clone();
        let job = self.backend.spawn(move |c| async move {
            c.resolve_gate_with_feedback(&thread_id, &run, &gate, &feedback)
                .await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.transcript.clear_gate_resolving(&gate_ref);
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    /// What the transcript list needs to draw the open field: the gate it
    /// belongs to and its input (cheap handles, read while the list draws).
    pub(super) fn reject_snapshot(&self) -> Option<(String, Entity<InputState>)> {
        self.reject
            .as_ref()
            .map(|editor| (editor.gate_ref.clone(), editor.input.clone()))
    }
}

/// The card's "Reject with feedback…" button.
pub(super) fn reject_button(
    key: &str,
    run_id: &str,
    gate_ref: &str,
    weak: &WeakEntity<ChatView>,
) -> Button {
    let weak = weak.clone();
    let (run, gate) = (run_id.to_string(), gate_ref.to_string());
    Button::new(SharedString::from(format!("{key}-reject")))
        .label("Reject with feedback…")
        .small()
        .ghost()
        .on_click(move |_, window, cx| {
            let (run, gate) = (run.clone(), gate.clone());
            let _ = weak.update(cx, |this, cx| this.start_reject(run, gate, window, cx));
        })
}

/// The field under a card, when it is open for `gate_ref`.
pub(super) fn render_reject_editor(
    gate_ref: &str,
    open: Option<&(String, Entity<InputState>)>,
    weak: &WeakEntity<ChatView>,
) -> Option<AnyElement> {
    let (_, input) = open.filter(|(open_ref, _)| open_ref == gate_ref)?;
    let cancel = weak.clone();
    let esc = weak.clone();
    Some(
        h_flex()
            .gap_2()
            .pt_1()
            .items_center()
            // Capture, so Esc closes the field before the input sees it.
            .capture_key_down(move |event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    let _ = esc.update(cx, |this, cx| this.cancel_reject(cx));
                    cx.stop_propagation();
                }
            })
            .child(div().flex_1().child(Input::new(input).small()))
            .child(
                Button::new(SharedString::from(format!("{gate_ref}-reject-cancel")))
                    .label("Cancel")
                    .ghost()
                    .xsmall()
                    .on_click(move |_, _, cx| {
                        let _ = cancel.update(cx, |this, cx| this.cancel_reject(cx));
                    }),
            )
            .into_any_element(),
    )
}
