//! Orchestration around the open thread: the subagent frame above the
//! composer, the context strip (delegated from, forked from with Merge back,
//! usage-limit countdown, paused after Stop, interrupted with Continue,
//! queued follow-ups), the queue panel and the Fork action.
//!
//! Refresh is event-driven: on open, when the thread's stream reports a run
//! end or a timeline reload, and after every mutation here. While a turn
//! runs or a child task is live, a slow poll re-reads tasks and queue, and
//! only while the app window is in front. An idle thread schedules nothing.

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::orchestration::{
    ForkOptions, HoldReason, Lineage, QueueAction, QueueDoc, TaskStatus, TaskView,
};

use super::{ChatView, OpenThread, content_width};
use crate::jobs::run;
use crate::ui::*;

/// Poll period while something is moving. Also the granularity of elapsed
/// and countdown labels, which only repaint when data arrives.
const POLL: Duration = Duration::from_secs(4);

#[derive(Default)]
pub(super) struct Orchestration {
    tasks: Vec<TaskView>,
    queue: QueueDoc,
    lineage: Lineage,
    lineage_loaded: bool,
    queue_open: bool,
    queue_input: Option<(Entity<InputState>, Subscription)>,
    /// A mutation is in flight; its buttons wait for it.
    busy: bool,
    /// Merge-back outcome, shown in the strip until the next action.
    note: Option<SharedString>,
    loading: bool,
    /// Another refresh was asked for while one was in flight.
    stale: bool,
    polling: bool,
    /// The thread's goal (strip above the composer).
    pub(super) goal: super::goals::Goal,
}

/// Child thread → the task that created it, for threads delegated from a
/// thread opened in this session. The routes give a parent its children but
/// not a child its parent, so this is how a child knows "delegated from".
#[derive(Default)]
struct Delegations(HashMap<String, TaskView>);

impl Global for Delegations {}

struct Snapshot {
    tasks: Result<Vec<TaskView>, ClientError>,
    queue: Result<QueueDoc, ClientError>,
    lineage: Option<Result<Lineage, ClientError>>,
}

impl ChatView {
    /// Re-read tasks and queue (and lineage until loaded). Coalesces: a call
    /// during a refresh only marks it stale.
    pub(super) fn refresh_orchestration(&mut self, cx: &mut Context<Self>) {
        self.load_goal(cx);
        let o = &mut self.orch;
        if o.loading {
            o.stale = true;
            return;
        }
        o.loading = true;
        let want_lineage = !o.lineage_loaded;
        let thread_id = self.thread_id.clone();
        let job = self.backend.spawn(move |c| async move {
            let (tasks, queue) =
                tokio::join!(c.thread_tasks(&thread_id), c.thread_queue(&thread_id));
            let lineage = match want_lineage {
                true => Some(c.thread_lineage(&thread_id).await),
                false => None,
            };
            Snapshot {
                tasks: tasks.map(|t| t.tasks),
                queue,
                lineage,
            }
        });
        cx.spawn(async move |this, cx| {
            if let Ok(snapshot) = job.await {
                let _ = this.update(cx, |this, cx| this.apply_snapshot(snapshot, cx));
            }
        })
        .detach();
    }

    fn apply_snapshot(&mut self, s: Snapshot, cx: &mut Context<Self>) {
        let mut reload_timeline = false;
        let o = &mut self.orch;
        o.loading = false;
        // silent-ok: a thread the orchestrator has not seen yet (no message
        // sent) answers 404, as does an older gateway; both mean "nothing".
        match s.tasks {
            Ok(tasks) => o.tasks = tasks,
            Err(e) if e.is_not_found() => o.tasks.clear(),
            Err(e) => tracing::debug!(error = %e, "thread tasks refresh failed"),
        }
        match s.queue {
            Ok(queue) => {
                // An external turn shows up only here: reload its transcript
                // while it runs and once more when it ends.
                if queue.external_run().is_some() || o.queue.external_run().is_some() {
                    reload_timeline = true;
                }
                o.queue = queue;
            }
            Err(e) if e.is_not_found() => o.queue = QueueDoc::default(),
            Err(e) => tracing::debug!(error = %e, "thread queue refresh failed"),
        }
        match s.lineage {
            Some(Ok(lineage)) => {
                let forked = lineage.forked_from.is_some();
                o.lineage = lineage;
                o.lineage_loaded = true;
                if forked {
                    self.load_fork_prefix(cx);
                }
            }
            Some(Err(e)) => tracing::debug!(error = %e, "thread lineage refresh failed"),
            None => {}
        }
        let delegations = cx.default_global::<Delegations>();
        for task in &self.orch.tasks {
            if let Some(child) = &task.child_thread_id {
                delegations.0.insert(child.clone(), task.clone());
            }
        }
        let mut related: Vec<String> = Vec::new();
        if let Some(parent) = self.delegation(cx).map(|t| t.parent_thread_id) {
            related.push(parent);
        }
        if let Some(from) = &self.orch.lineage.forked_from {
            related.push(from.source_thread.clone());
        }
        crate::titles::ensure(related, &self.backend, cx);
        if reload_timeline {
            self.load_timeline(cx);
        }
        if std::mem::take(&mut self.orch.stale) {
            self.refresh_orchestration(cx);
        }
        self.ensure_poll(cx);
        cx.notify();
    }

    /// A fork shows its source conversation above its own messages.
    fn load_fork_prefix(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.fork_prefix(&thread_id).await },
            |this, result, cx| match result {
                Ok(prefix) => {
                    this.transcript.set_prefix(prefix);
                    this.refresh_rows(cx);
                }
                Err(e) => tracing::debug!(error = %e, "fork source load failed"),
            },
        );
    }

    fn delegation(&self, cx: &App) -> Option<TaskView> {
        cx.try_global::<Delegations>()?
            .0
            .get(&self.thread_id)
            .cloned()
    }

    fn needs_poll(&self) -> bool {
        self.transcript.active_run().is_some()
            || self.orch.queue.external_run().is_some()
            || self.orch.tasks.iter().any(|t| t.status.is_live())
    }

    /// The running turn, streamed (IronClaw's loop) or polled (CLI runtime).
    pub(super) fn running_run(&self) -> Option<String> {
        self.transcript
            .active_run()
            .or(self.orch.queue.external_run())
            .map(str::to_string)
    }

    /// Start the slow poll if something is moving. It stops by itself.
    pub(super) fn ensure_poll(&mut self, cx: &mut Context<Self>) {
        if self.orch.polling || !self.needs_poll() {
            return;
        }
        self.orch.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let keep = this.update(cx, |this, cx| {
                    if !this.needs_poll() {
                        this.orch.polling = false;
                        return false;
                    }
                    // In the background nothing is watching; the next stream
                    // event or a later tick catches up.
                    if cx.active_window().is_some() {
                        this.refresh_orchestration(cx);
                    }
                    true
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn queue_action(&mut self, action: QueueAction, cx: &mut Context<Self>) {
        if self.orch.busy {
            return;
        }
        self.orch.busy = true;
        self.orch.note = None;
        cx.notify();
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.mutate_queue(&thread_id, &action).await },
            |this, result, cx| {
                this.orch.busy = false;
                match result {
                    Ok(queue) => this.orch.queue = queue,
                    Err(e) => this.set_error(e.to_string(), cx),
                }
                this.ensure_poll(cx);
                cx.notify();
            },
        );
    }

    fn cancel_task(&mut self, task_id: String, cx: &mut Context<Self>) {
        run(
            &self.backend,
            cx,
            move |c| async move { c.cancel_task(&task_id).await },
            |this, result, cx| match result {
                Ok(view) => {
                    if let Some(slot) = this
                        .orch
                        .tasks
                        .iter_mut()
                        .find(|t| t.task_id == view.task_id)
                    {
                        *slot = view;
                    }
                    cx.notify();
                }
                Err(e) => this.set_error(e.to_string(), cx),
            },
        );
    }

    /// Fork the thread into a new one. `run`: keep only the history up to
    /// and including run `at_run` (1-based); `None` keeps everything.
    pub(super) fn fork(&mut self, at_run: Option<u32>, cx: &mut Context<Self>) {
        if self.orch.busy {
            return;
        }
        self.orch.busy = true;
        cx.notify();
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move {
                let options = ForkOptions {
                    run: at_run,
                    ..ForkOptions::default()
                };
                c.fork_thread(&thread_id, &options).await
            },
            |this, result, cx| {
                this.orch.busy = false;
                match result {
                    Ok(forked) => cx.emit(OpenThread(forked.fork_thread_id)),
                    Err(e) => this.set_error(e.to_string(), cx),
                }
                cx.notify();
            },
        );
    }

    fn merge_back(&mut self, cx: &mut Context<Self>) {
        if self.orch.busy {
            return;
        }
        self.orch.busy = true;
        self.orch.note = None;
        cx.notify();
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.merge_back(&thread_id, None).await },
            |this, result, cx| {
                this.orch.busy = false;
                match result {
                    Ok(merged) => {
                        let target = merged
                            .target_thread_id
                            .as_deref()
                            .and_then(|id| crate::titles::get(cx, id))
                            .unwrap_or_else(|| "the original thread".into());
                        this.orch.note = Some(
                            merged
                                .note
                                .filter(|n| !n.trim().is_empty())
                                .unwrap_or_else(|| {
                                    format!("Merged into {target}. It arrives there as context once that thread is idle.")
                                })
                                .into(),
                        );
                        this.orch.lineage_loaded = false;
                        this.refresh_orchestration(cx);
                    }
                    Err(e) => this.set_error(e.to_string(), cx),
                }
                cx.notify();
            },
        );
    }

    fn toggle_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.orch.queue_open = !self.orch.queue_open;
        if self.orch.queue_open && self.orch.queue_input.is_none() {
            let input = cx
                .new(|cx| InputState::new(window, cx).placeholder("Add a follow-up to the queue"));
            let sub = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_queued(window, cx);
                }
            });
            self.orch.queue_input = Some((input, sub));
        }
        cx.notify();
    }

    fn add_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = &self.orch.queue_input else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() || self.orch.busy {
            return;
        }
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.queue_action(QueueAction::Add(text), cx);
    }

    fn open_thread(&mut self, thread_id: String, cx: &mut Context<Self>) {
        cx.emit(OpenThread(thread_id));
    }

    /// Strip, subagent frame and queue panel, stacked above the composer.
    pub(super) fn render_orchestration(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let goal = self.render_goal(cx);
        let strip = self.render_strip(cx);
        let frame = self.render_frame(cx);
        let queue = self.orch.queue_open.then(|| self.render_queue(cx));
        if goal.is_none() && strip.is_none() && frame.is_none() && queue.is_none() {
            return None;
        }
        Some(
            h_flex()
                .justify_center()
                .px_4()
                .pt_2()
                .child(
                    v_flex()
                        .w_full()
                        .max_w(px(content_width(cx)))
                        .gap_2()
                        .children(goal)
                        .children(queue)
                        .children(frame)
                        .children(strip),
                )
                .into_any_element(),
        )
    }

    fn render_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let o = &self.orch;
        let now = Utc::now().timestamp_millis();
        let busy = o.busy;
        let mut chips: Vec<AnyElement> = Vec::new();

        if let Some(task) = self.delegation(cx) {
            let parent = task.parent_thread_id.clone();
            let title = crate::titles::get(cx, &parent).unwrap_or_else(|| "a conversation".into());
            chips.push(
                link_chip(
                    "strip-parent",
                    IconName::Bot,
                    format!("Delegated from {title}"),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_thread(parent.clone(), cx)))
                .into_any_element(),
            );
            if task.status == TaskStatus::Waiting
                && let Some(at) = task.limit_reset_at
            {
                chips.push(text_chip(
                    IconName::Pause,
                    format!("Usage limit, resumes {}", countdown(at, now)),
                    cx,
                ));
            }
        }

        if let Some(from) = &o.lineage.forked_from {
            let source = from.source_thread.clone();
            let title = crate::titles::get(cx, &source).unwrap_or_else(|| "a conversation".into());
            chips.push(
                link_chip(
                    "strip-fork",
                    IconName::Copy,
                    format!("Forked from {title}"),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_thread(source.clone(), cx)))
                .into_any_element(),
            );
            chips.push(
                Button::new("merge-back")
                    .icon(IconName::Undo2)
                    .label("Merge back")
                    .xsmall()
                    .ghost()
                    .loading(busy)
                    .tooltip("Send what happened here back to the original thread")
                    .on_click(cx.listener(|this, _, _, cx| this.merge_back(cx)))
                    .into_any_element(),
            );
        }

        // The earliest limit reset among waiting children.
        if let Some(at) = o
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Waiting)
            .filter_map(|t| t.limit_reset_at)
            .min()
        {
            chips.push(text_chip(
                IconName::Pause,
                format!("Subagent usage limit, resumes {}", countdown(at, now)),
                cx,
            ));
        }

        if o.queue.held {
            let (icon, text, action) = match o.queue.hold_reason {
                Some(HoldReason::Interrupted) => (
                    IconName::TriangleAlert,
                    "Interrupted when the app quit",
                    "Continue",
                ),
                Some(HoldReason::Stopped | HoldReason::Cancelled) => {
                    (IconName::Pause, "Paused after Stop", "Resume queue")
                }
                _ => (IconName::Pause, "Queue on hold", "Release"),
            };
            chips.push(text_chip(icon, text, cx));
            let interrupted = o.queue.hold_reason == Some(HoldReason::Interrupted);
            chips.push(
                Button::new("queue-release")
                    .icon(IconName::Play)
                    .label(action)
                    .xsmall()
                    .when(interrupted, |b| b.primary())
                    .when(!interrupted, |b| b.ghost())
                    .loading(busy)
                    .tooltip(if interrupted {
                        "Finish the cut turn, then run the queued follow-ups"
                    } else {
                        "Let queued follow-ups run again"
                    })
                    .on_click(
                        cx.listener(|this, _, _, cx| this.queue_action(QueueAction::Release, cx)),
                    )
                    .into_any_element(),
            );
        }

        let queued = o.queue.items.len();
        if queued > 0 || o.queue_open {
            let label = match queued {
                0 => "Queue".to_string(),
                1 => "1 queued follow-up".to_string(),
                n => format!("{n} queued follow-ups"),
            };
            chips.push(
                Button::new("queue-toggle")
                    .icon(IconName::Inbox)
                    .label(label)
                    .xsmall()
                    .ghost()
                    .selected(o.queue_open)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_queue(window, cx)))
                    .into_any_element(),
            );
        }

        if let Some(note) = o.note.clone() {
            chips.push(text_chip(IconName::CircleCheck, note, cx));
        }

        (!chips.is_empty()).then(|| {
            h_flex()
                .flex_wrap()
                .items_center()
                .gap_x_3()
                .gap_y_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .children(chips)
                .into_any_element()
        })
    }

    /// Live delegated children: title, engine, status, elapsed, open, cancel.
    fn render_frame(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let live: Vec<&TaskView> = self
            .orch
            .tasks
            .iter()
            .filter(|t| t.status.is_live())
            .collect();
        if live.is_empty() {
            return None;
        }
        let now = Utc::now().timestamp_millis();
        let theme = cx.theme();
        let rows = live.into_iter().enumerate().map(|(ix, task)| {
            let status = match (task.status, task.limit_reset_at) {
                (TaskStatus::Waiting, Some(at)) => {
                    format!("Usage limit, resumes {}", countdown(at, now))
                }
                (TaskStatus::WaitingChildren, _) if task.pending_children > 0 => {
                    format!("Waiting for {} subagent(s)", task.pending_children)
                }
                (status, _) => status.label().to_string(),
            };
            let elapsed = task.started_at.map(|s| duration((now - s).max(0) / 1000));
            let child = task.child_thread_id.clone();
            let task_id = task.task_id.clone();
            let cancelling = task.cancel_requested;
            h_flex()
                .id(("subagent", ix))
                .gap_2()
                .px_2()
                .py_1()
                .items_center()
                .rounded(theme.radius)
                .text_sm()
                .when(child.is_some(), |row| {
                    crate::design::kit::smooth_bg(
                        row.cursor_pointer(),
                        format!("subagent-{ix}"),
                        cx,
                    )
                })
                .when_some(child, |row, child| {
                    row.on_click(
                        cx.listener(move |this, _, _, cx| this.open_thread(child.clone(), cx)),
                    )
                })
                .child(match task.status {
                    TaskStatus::Running | TaskStatus::Starting => {
                        Spinner::new().xsmall().into_any_element()
                    }
                    _ => Icon::new(IconName::Pause).xsmall().into_any_element(),
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_medium()
                        .child(task.title.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(task.engine_label()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(match elapsed {
                            Some(e) => format!("{status} · {e}"),
                            None => status,
                        }),
                )
                .child(
                    Button::new(SharedString::from(format!("cancel-{}", task.task_id)))
                        .icon(IconName::Close)
                        .xsmall()
                        .ghost()
                        .loading(cancelling)
                        .disabled(cancelling)
                        .tooltip(if cancelling {
                            "Cancelling"
                        } else {
                            "Cancel this subagent and everything it delegated"
                        })
                        .accessibility_label("Cancel subagent")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.cancel_task(task_id.clone(), cx)
                        })),
                )
        });
        Some(
            v_flex()
                .p_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary)
                .child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Subagents"),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_queue(&self, cx: &mut Context<Self>) -> AnyElement {
        let o = &self.orch;
        let theme = cx.theme();
        let busy = o.busy;
        let first = o.queue.items.first().map(|i| i.id.clone());
        let items = o.queue.items.iter().enumerate().map(|(ix, item)| {
            let id = item.id.clone();
            let promote = (ix > 0).then(|| (item.id.clone(), first.clone()));
            h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .items_start()
                .text_sm()
                .child(
                    div()
                        .flex_none()
                        .w(px(18.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{}.", ix + 1)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .line_clamp(2)
                        .child(item.text.clone()),
                )
                .when_some(promote, |row, (id, before)| {
                    row.child(
                        Button::new(SharedString::from(format!("promote-{id}")))
                            .icon(IconName::ChevronUp)
                            .xsmall()
                            .ghost()
                            .disabled(busy)
                            .tooltip("Run this one next")
                            .accessibility_label("Move to the front")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.queue_action(
                                    QueueAction::Reorder {
                                        item_id: id.clone(),
                                        before_id: before.clone(),
                                    },
                                    cx,
                                )
                            })),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("unqueue-{}", item.id)))
                        .icon(IconName::Delete)
                        .xsmall()
                        .ghost()
                        .disabled(busy)
                        .tooltip("Remove from the queue")
                        .accessibility_label("Remove from the queue")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.queue_action(QueueAction::Remove(id.clone()), cx)
                        })),
                )
        });
        let held = o.queue.held;
        let empty = o.queue.items.is_empty();
        v_flex()
            .gap_1()
            .p_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                h_flex()
                    .px_2()
                    .pt_1()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child(if held {
                        "Queued follow-ups (on hold)"
                    } else {
                        "Queued follow-ups run in order once the current turn ends"
                    }))
                    .child(
                        Button::new("queue-hold")
                            .icon(if held {
                                IconName::Play
                            } else {
                                IconName::Pause
                            })
                            .label(if held { "Release" } else { "Hold" })
                            .xsmall()
                            .ghost()
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let action = if held {
                                    QueueAction::Release
                                } else {
                                    QueueAction::Hold
                                };
                                this.queue_action(action, cx)
                            })),
                    )
                    .when(!empty, |row| {
                        row.child(
                            Button::new("queue-clear")
                                .label("Clear")
                                .xsmall()
                                .ghost()
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.queue_action(QueueAction::Clear, cx)
                                })),
                        )
                    }),
            )
            .when(empty, |panel| {
                panel.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Nothing queued."),
                )
            })
            .children(items)
            .when_some(
                o.queue_input.as_ref().map(|(i, _)| i.clone()),
                |panel, input| {
                    panel.child(
                        h_flex()
                            .gap_2()
                            .px_1()
                            .pb_1()
                            .child(div().flex_1().child(Input::new(&input).small()))
                            .child(
                                Button::new("queue-add")
                                    .icon(IconName::Plus)
                                    .xsmall()
                                    .disabled(busy)
                                    .tooltip("Add to the queue")
                                    .accessibility_label("Add to the queue")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_queued(window, cx)
                                    })),
                            ),
                    )
                },
            )
            .into_any_element()
    }
}

/// A clickable muted line in the strip.
fn link_chip(
    id: &'static str,
    icon: IconName,
    text: impl Into<SharedString>,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(id)
        .role(Role::Button)
        .gap_1()
        .items_center()
        .cursor_pointer()
        .hover(|s| s.text_color(cx.theme().foreground))
        .child(Icon::new(icon).xsmall())
        .child(text.into())
}

fn text_chip(icon: IconName, text: impl Into<SharedString>, _cx: &App) -> AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(Icon::new(icon).xsmall())
        .child(text.into())
        .into_any_element()
}

/// "in 12 min", "in 2 h 5 min", "any moment now".
fn countdown(at_ms: i64, now_ms: i64) -> String {
    let secs = (at_ms - now_ms) / 1000;
    if secs <= 0 {
        return "any moment now".into();
    }
    format!("in {}", duration(secs))
}

/// "45 s", "3 min", "2 h 5 min".
fn duration(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::{countdown, duration};

    #[test]
    fn labels_read_naturally() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(185), "3 min");
        assert_eq!(duration(7_500), "2 h 5 min");
        assert_eq!(countdown(1_000, 5_000), "any moment now");
        assert_eq!(countdown(725_000, 5_000), "in 12 min");
    }
}
