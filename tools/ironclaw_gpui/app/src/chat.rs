//! One open thread: transcript, live stream, composer, run controls.

use std::rc::Rc;

use ironclaw_gpui_client::contract::ProductGateKind;
use ironclaw_gpui_client::transcript::{GateState, Row, ToolStatus, Transcript};
use ironclaw_gpui_client::{
    Backend, ClientError, EventStream, GateDecision, StreamMessage, StreamStatus,
};

use crate::jobs::flatten;
use crate::ui::*;

const TIMELINE_LIMIT: u32 = 200;
const CONTENT_MAX_WIDTH: f32 = 780.;

pub struct ChatView {
    backend: Backend,
    channel: Option<String>,
    thread_id: String,
    transcript: Transcript,
    rows: Rc<Vec<Row>>,
    scroller: Entity<MessageScrollerState>,
    composer: Entity<TextareaState>,
    status: StreamStatus,
    error: Option<SharedString>,
    _stream: Option<EventStream>,
    _pump: Task<()>,
}

/// Emitted on every timeline load so the sidebar picks up server-set titles.
pub struct ThreadUpdated;

impl EventEmitter<ThreadUpdated> for ChatView {}

impl ChatView {
    pub fn new(
        backend: Backend,
        channel: Option<String>,
        thread_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("Message IronClaw")
                .submit_on_enter(true)
        });
        let subscription = cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.send(window, cx);
            }
        });
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));

        let (stream, mut rx) = backend.subscribe(&thread_id);
        let pump = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.recv().await {
                // Drain whatever else is queued so a burst renders once.
                let mut batch = vec![first];
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }
                if this
                    .update(cx, |this, cx| this.on_stream(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        composer.update(cx, |state, cx| state.focus(window, cx));

        let mut this = Self {
            backend,
            channel,
            thread_id,
            transcript: Transcript::default(),
            rows: Rc::new(Vec::new()),
            scroller,
            composer,
            status: StreamStatus::Connecting,
            error: None,
            _stream: Some(stream),
            _pump: pump,
        };
        cx.on_release(move |_, _| drop(subscription)).detach();
        this.load_timeline(cx);
        this
    }

    fn load_timeline(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.timeline(&thread_id, TIMELINE_LIMIT).await });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| match result {
                Ok(page) => {
                    cx.emit(ThreadUpdated);
                    this.transcript.set_timeline(page);
                    this.refresh_rows(cx);
                }
                Err(e) => this.set_error(e.to_string(), cx),
            });
        });
        self.keep(task);
    }

    fn on_stream(&mut self, batch: Vec<StreamMessage>, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut reload = false;
        for message in batch {
            match message {
                StreamMessage::Frame(frame) => {
                    let applied = self.transcript.apply(&frame.event);
                    changed |= applied.changed;
                    reload |= applied.reload_timeline;
                }
                StreamMessage::Status(status) => {
                    self.status = status;
                    changed = true;
                }
                StreamMessage::Undecodable { event, error } => {
                    tracing::debug!(%event, %error, "skipped an undecodable stream frame");
                }
                StreamMessage::Closed { reason } => {
                    self.set_error(format!("Live updates stopped: {reason}"), cx);
                }
            }
        }
        if reload {
            self.load_timeline(cx);
        }
        if changed {
            self.refresh_rows(cx);
        }
    }

    /// Recompute rows and tell the virtual list exactly what moved, so only
    /// changed rows are re-measured while text streams.
    fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        let new_rows = self.transcript.rows();
        let old = self.rows.clone();
        let prefix = old
            .iter()
            .zip(new_rows.iter())
            .take_while(|(a, b)| a.key() == b.key())
            .count();
        let changed: Vec<usize> = (0..prefix).filter(|&i| old[i] != new_rows[i]).collect();
        let (old_len, new_len) = (old.len(), new_rows.len());
        self.scroller.update(cx, |state, cx| {
            if prefix < old_len || prefix < new_len {
                state.splice(prefix..old_len, new_len - prefix, cx);
            }
            for i in changed {
                state.remeasure_items(i..i + 1, cx);
            }
        });
        self.rows = Rc::new(new_rows);
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(channel) = self.channel.clone() else {
            self.set_error("This gateway has no chat channel configured.", cx);
            return;
        };
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        self.error = None;
        let pending = self.transcript.push_pending_user(&text);
        self.refresh_rows(cx);
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));

        let thread_id = self.thread_id.clone();
        let body = text.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.send_message(&channel, &thread_id, &body).await });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(ack) => {
                    this.transcript.ack_pending_user(pending);
                    if let Some(notice) = ack.notice.filter(|n| !n.trim().is_empty()) {
                        this.error = Some(notice.into());
                    }
                    this.load_timeline(cx);
                }
                Err(e) => {
                    this.transcript.drop_pending_user(pending);
                    this.composer
                        .update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.transcript.active_run().map(str::to_string) else {
            return;
        };
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.cancel_run(&thread_id, &run).await });
        self.run_and_report(job, cx);
    }

    fn retry(&mut self, run: String, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.retry_run(&thread_id, &run).await });
        self.run_and_report(job, cx);
    }

    fn resolve_gate(
        &mut self,
        run: String,
        gate_ref: String,
        decision: GateDecision,
        cx: &mut Context<Self>,
    ) {
        self.transcript.mark_gate_resolving(&gate_ref);
        self.refresh_rows(cx);
        let thread_id = self.thread_id.clone();
        let gate = gate_ref.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.resolve_gate(&thread_id, &run, &gate, decision).await });
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

    /// Fire a mutation; the stream reports its effect, so only errors matter.
    fn run_and_report(
        &mut self,
        job: tokio::task::JoinHandle<Result<serde_json::Value, ClientError>>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn(async move |this, cx| {
            if let Err(e) = flatten(job.await) {
                let _ = this.update(cx, |this, cx| this.set_error(e.to_string(), cx));
            }
        });
        self.keep(task);
    }

    fn set_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    /// Request tasks hold only a weak handle, so they can outlive the view
    /// safely. Dropping a GPUI task would cancel an in-flight request.
    fn keep(&mut self, task: Task<()>) {
        task.detach();
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let weak = cx.weak_entity();
        let running = self.transcript.active_run().is_some();
        let stopping = self.transcript.is_stopping();
        // Fill the silence between send and the first streamed token.
        let working = stopping
            || running
                && !matches!(
                    self.rows.last(),
                    Some(Row::Assistant {
                        streaming: true,
                        ..
                    })
                );

        let list = MessageScroller::new("transcript", self.scroller.clone(), move |ix, _, cx| {
            let row = &rows[ix];
            h_flex().w_full().justify_center().px_4().py_1p5().child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .child(render_row(row, &weak, cx)),
            )
        });

        let banner = match self.status {
            StreamStatus::Reconnecting { delay_secs, .. } => {
                Some(format!("Connection lost. Reconnecting in {delay_secs}s"))
            }
            _ => None,
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .when_some(banner, |this, text| {
                this.child(
                    div()
                        .px_4()
                        .py_1()
                        .text_xs()
                        .bg(cx.theme().warning)
                        .text_color(cx.theme().warning_foreground)
                        .child(text),
                )
            })
            .child(div().flex_1().min_h_0().child(list.size_full()))
            .when(working, |this| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        h_flex()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Spinner::new().xsmall())
                            .child(if stopping {
                                "Stopping…"
                            } else {
                                "Working…"
                            }),
                    ),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        div()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error),
                    ),
                )
            })
            .child(
                h_flex().justify_center().px_4().pb_4().pt_2().child(
                    h_flex()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .gap_2()
                        .items_end()
                        .child(div().flex_1().child(Textarea::new(&self.composer)))
                        .child(if running {
                            Button::new("stop")
                                .icon(IconName::Square)
                                .tooltip("Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                        } else {
                            Button::new("send")
                                .icon(IconName::ArrowUp)
                                .primary()
                                .tooltip("Send (Enter)")
                                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                        }),
                ),
            )
    }
}

fn render_row(row: &Row, weak: &WeakEntity<ChatView>, cx: &App) -> AnyElement {
    match row {
        Row::User { text, pending, .. } => user_bubble(text.clone(), *pending).into_any_element(),
        Row::Assistant {
            key,
            text,
            streaming,
        } => markdown(SharedString::from(key.clone()), text.clone(), *streaming).into_any_element(),
        Row::System { text, .. } => div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text.clone())
            .into_any_element(),
        Row::Thinking { text, .. } => {
            activity_line(IconName::Bot, "Thinking", Some(text.clone().into()), cx)
                .into_any_element()
        }
        Row::Narration { text, .. } => {
            activity_line(IconName::Ellipsis, text.clone(), None, cx).into_any_element()
        }
        Row::Tool {
            title,
            detail,
            status,
            ..
        } => {
            let icon = match status {
                ToolStatus::Running => IconName::LoaderCircle,
                ToolStatus::Completed => IconName::CircleCheck,
                ToolStatus::Failed => IconName::CircleX,
            };
            activity_line(icon, title.clone(), detail.clone().map(Into::into), cx)
                .into_any_element()
        }
        Row::Skills { names, .. } => activity_line(
            IconName::BookOpen,
            format!("Using skills: {}", names.join(", ")),
            None,
            cx,
        )
        .into_any_element(),
        Row::WorkSummary { text, .. } => {
            activity_line(IconName::FileText, text.clone(), None, cx).into_any_element()
        }
        Row::Gate {
            key,
            run_id,
            gate_ref,
            kind,
            headline,
            body,
            allow_always,
            state,
        } => render_gate(
            key,
            run_id,
            gate_ref,
            *kind,
            headline,
            body,
            *allow_always,
            *state,
            weak,
            cx,
        ),
        Row::RunNotice {
            key,
            run_id,
            text,
            failed,
            retryable,
        } => card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .items_start()
                    .when(*failed, |this| this.text_color(cx.theme().danger))
                    .child(
                        Icon::new(if *failed {
                            IconName::TriangleAlert
                        } else {
                            IconName::Info
                        })
                        .flex_none(),
                    )
                    .child(div().flex_1().min_w_0().child(text.clone())),
            )
            .when(*retryable, |this| {
                let weak = weak.clone();
                let run = run_id.clone();
                this.child(
                    h_flex().child(
                        Button::new(SharedString::from(format!("{key}-retry")))
                            .label("Retry")
                            .small()
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| this.retry(run.clone(), cx));
                            }),
                    ),
                )
            })
            .into_any_element(),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_gate(
    key: &str,
    run_id: &str,
    gate_ref: &str,
    kind: ProductGateKind,
    headline: &str,
    body: &Option<String>,
    allow_always: bool,
    state: GateState,
    weak: &WeakEntity<ChatView>,
    cx: &App,
) -> AnyElement {
    let decide = |label: &'static str, suffix: &str, decision: GateDecision| {
        let weak = weak.clone();
        let (run, gate) = (run_id.to_string(), gate_ref.to_string());
        Button::new(SharedString::from(format!("{key}-{suffix}")))
            .label(label)
            .small()
            .on_click(move |_, _, cx| {
                let (run, gate) = (run.clone(), gate.clone());
                let _ = weak.update(cx, |this, cx| this.resolve_gate(run, gate, decision, cx));
            })
    };

    let actions = match (state, kind) {
        (GateState::Closed, _) => None,
        (GateState::Resolving, _) => Some(
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().xsmall())
                .child("Waiting for IronClaw to confirm")
                .into_any_element(),
        ),
        // Sign-in flows run in the browser; the gate clears when they finish.
        (GateState::Open, ProductGateKind::Auth) => Some(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Finish this sign-in from the browser (sidebar, Open in browser).")
                .into_any_element(),
        ),
        (GateState::Open, _) => Some(
            h_flex()
                .gap_2()
                .child(
                    decide(
                        "Approve",
                        "approve",
                        GateDecision::Approve { always: false },
                    )
                    .primary(),
                )
                .when(allow_always, |this| {
                    this.child(decide(
                        "Always allow",
                        "always",
                        GateDecision::Approve { always: true },
                    ))
                })
                .child(decide("Decline", "decline", GateDecision::Decline).ghost())
                .into_any_element(),
        ),
    };

    card(cx)
        .when(state == GateState::Closed, |this| this.opacity(0.6))
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .font_medium()
                .items_start()
                .child(Icon::new(IconName::TriangleAlert).flex_none())
                .child(div().flex_1().min_w_0().child(headline.to_string())),
        )
        .when_some(body.clone(), |this, body| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(body),
            )
        })
        .children(actions)
        .into_any_element()
}
