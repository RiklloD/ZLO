//! Orchestration's views (GPUI `render_orchestration`, `render_tail`,
//! `render_queued`, `render_strip`, `render_frame`, `render_queue`).

use chrono::Utc;
use ironclaw_gpui_client::orchestration::{HoldReason, QueueAction, TaskStatus, TaskView};
use rune::{El, Icon, TextEdit, col, el, row};
use rune_kit::{ButtonVariant, Kit, TextInput};

use super::{
    OrchestrationState, add_queued, cancel_task, countdown, delegation, duration, merge_back, queue_action,
    toggle_queue,
};
use crate::chat::extras_ui::{
    ICON_XS, SM, XS, ghost_button, ghost_icon, ico, spinner, txt, txt_medium, variant_button,
};
use crate::chat::ChatState;
use crate::design::{self, Palette};
use crate::icons;
use crate::state::State;

/// Zeron's reading column; Appearance → Conversation width moves it.
fn content_width(s: &State) -> f32 {
    s.prefs.conversation_width.clamp(480., 1200.)
}

/// What stays on the composer: notices that change what a send does (the
/// strip), the queue's controls, and the goal editor while open. `plan` is
/// area A's plan strip, drawn first. Work, goals and queued turns live at
/// the transcript's end ([`tail`]), as in T3 Code's timeline pass.
pub fn composer(s: &State, chat: &ChatState, k: &Kit, plan: Option<El>) -> Option<El> {
    let o = &chat.orchestration;
    let id = chat.thread_id.as_str();
    let goal = chat.goals.editing().then(|| crate::chat::goals::view(s, chat, k)).flatten();
    let strip = strip(s, chat, k);
    let queue = o.queue_open.then(|| queue_panel(&s.palette, id, o, k));
    if goal.is_none() && strip.is_none() && queue.is_none() && plan.is_none() {
        return None;
    }
    let inner = col()
        .w_full()
        .max_w(content_width(s))
        .gap(8.)
        .children(plan)
        .children(goal)
        .children(queue)
        .children(strip);
    Some(row().key("orchestration").w_full().px(16.).pt(8.).justify_center().child(inner))
}

/// The transcript's last row, in T3's order: live subagents (work that may
/// need you), the goal, then queued turns that are not sent yet.
pub fn tail(s: &State, chat: &ChatState, k: &Kit) -> Option<El> {
    let o = &chat.orchestration;
    let id = chat.thread_id.as_str();
    let frame = frame(&s.palette, id, o, k);
    let goal = (!chat.goals.editing()).then(|| crate::chat::goals::view(s, chat, k)).flatten();
    let queued = queued(s, id, o, k);
    if frame.is_none() && goal.is_none() && queued.is_none() {
        return None;
    }
    Some(col().key("orchestration-tail").w_full().gap(8.).children(frame).children(goal).children(queued))
}

/// Queued follow-ups as unsent messages: dashed, unfilled bubbles on the
/// sender's side, with Send next and Remove on hover.
fn queued(s: &State, thread: &str, o: &OrchestrationState, k: &Kit) -> Option<El> {
    if o.queue.items.is_empty() {
        return None;
    }
    let p = &s.palette;
    let busy = o.busy;
    let first = o.queue.items.first().map(|i| i.id.clone());
    let bubble_max = content_width(s) * 0.8;
    let mut rows = col().key("queued").w_full().gap(12.);
    for (ix, item) in o.queue.items.iter().enumerate() {
        let mut actions = row().gap(1.).group_opacity(0., 1.);
        if ix > 0 {
            let (id, before, chat) = (item.id.clone(), first.clone(), thread.to_string());
            actions = actions.child(ghost_button(k, format!("promote-{id}"), "Send next", None, busy, move |s| {
                queue_action(s, &chat, QueueAction::Reorder { item_id: id.clone(), before_id: before.clone() })
            }));
        }
        let (id, chat) = (item.id.clone(), thread.to_string());
        actions = actions.child(ghost_button(k, format!("unqueue-{id}"), "Remove", None, busy, move |s| {
            queue_action(s, &chat, QueueAction::Remove(id.clone()))
        }));
        rows = rows.child(
            col()
                .key(format!("queued-row-{}", item.id))
                .hover_group()
                .w_full()
                .items_end()
                .gap(4.)
                .role("status")
                .label(format!("Not sent yet: {}", item.text))
                // In a full-width row, as the person's own bubbles: the row
                // shrinks it to a narrow pane (a side chat's).
                .child(
                    row().w_full().justify_end().child(
                        el().shrink()
                            .max_w(bubble_max)
                            .px(16.)
                            .py(10.)
                            .radius(design::BUBBLE_RADIUS)
                            .border(1., p.border_strong)
                            .border_dash(4., 3.)
                            .child(txt(item.text.clone(), SM, p.text_muted).wrap().max_lines(4).shrink()),
                    ),
                )
                .child(
                    row()
                        .gap(4.)
                        .center_y()
                        .child(actions)
                        .child(txt(if o.queue.held { "Queued, on hold" } else { "Queued" }, XS, p.text_faint)),
                ),
        );
    }
    Some(rows)
}

/// A clickable muted line in the strip.
fn link_chip(p: &Palette, key: &str, i: &'static Icon, label: String, open: String) -> El {
    row()
        .key(key)
        .role("button")
        .label(label.clone())
        .gap(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .child(ico(i, ICON_XS, p.text_muted))
        .child(txt(label, XS, p.text_muted))
        .on_press(move |s: &mut State| crate::workspace::open_thread(s, open.clone()))
}

fn text_chip(p: &Palette, key: String, i: &'static Icon, label: impl Into<String>) -> El {
    row()
        .key(key)
        .gap(4.)
        .center_y()
        .child(ico(i, ICON_XS, p.text_muted))
        .child(txt(label, XS, p.text_muted))
}

fn strip(s: &State, chat: &ChatState, k: &Kit) -> Option<El> {
    let o = &chat.orchestration;
    let id = chat.thread_id.clone();
    let p = &s.palette;
    let now = Utc::now().timestamp_millis();
    let busy = o.busy;
    let mut chips: Vec<El> = Vec::new();
    let title_of = |id: &str| crate::workspace::thread_title(s, id).unwrap_or_else(|| "a conversation".into());

    if let Some(task) = delegation(&chat.thread_id) {
        let parent = task.parent_thread_id.clone();
        chips.push(link_chip(p, "strip-parent", &icons::BOT, format!("Delegated from {}", title_of(&parent)), parent));
        if task.status == TaskStatus::Waiting
            && let Some(at) = task.limit_reset_at
        {
            chips.push(text_chip(
                p,
                "strip-parent-limit".into(),
                &icons::PAUSE,
                format!("Usage limit, resumes {}", countdown(at, now)),
            ));
        }
    }

    if let Some(from) = &o.lineage.forked_from {
        let source = from.source_thread.clone();
        // A side chat's pane already names its parent.
        if !chat.embedded {
            chips.push(link_chip(p, "strip-fork", &icons::COPY, format!("Forked from {}", title_of(&source)), source));
        }
        let merging = id.clone();
        let merge = variant_button(k, "merge-back", "Merge back", Some(&icons::UNDO_2), ButtonVariant::Ghost, busy, move |s| {
            merge_back(s, &merging)
        })
        .help("Send what happened here back to the original thread");
        chips.push(merge);
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
            p,
            "strip-subagent-limit".into(),
            &icons::PAUSE,
            format!("Subagent usage limit, resumes {}", countdown(at, now)),
        ));
    }

    if o.queue.held {
        let (i, label, action): (&'static Icon, _, _) = match o.queue.hold_reason {
            Some(HoldReason::Interrupted) => (&icons::TRIANGLE_ALERT, "Interrupted when the app quit", "Continue"),
            Some(HoldReason::Stopped | HoldReason::Cancelled) => (&icons::PAUSE, "Paused after Stop", "Resume queue"),
            _ => (&icons::PAUSE, "Queue on hold", "Release"),
        };
        chips.push(text_chip(p, "strip-held".into(), i, label));
        let interrupted = o.queue.hold_reason == Some(HoldReason::Interrupted);
        let variant = if interrupted { ButtonVariant::Submit } else { ButtonVariant::Ghost };
        let releasing = id.clone();
        chips.push(
            variant_button(k, "queue-release", action, Some(&icons::PLAY), variant, busy, move |s| {
                queue_action(s, &releasing, QueueAction::Release)
            })
            .help(if interrupted {
                "Finish the cut turn, then run the queued follow-ups"
            } else {
                "Let queued follow-ups run again"
            }),
        );
    }

    let queued = o.queue.items.len();
    if queued > 0 || o.queue_open {
        let label = match queued {
            0 => "Queue".to_string(),
            1 => "1 queued follow-up".to_string(),
            n => format!("{n} queued follow-ups"),
        };
        let toggling = id.clone();
        let toggle = ghost_button(k, "queue-toggle", &label, Some(&icons::INBOX), false, move |s| {
            toggle_queue(s, &toggling)
        });
        // Selected while the panel is open.
        chips.push(if o.queue_open {
            el().radius(design::CONTROL_RADIUS).fill(p.active).child(toggle)
        } else {
            toggle
        });
    }

    if let Some(note) = o.note.clone() {
        chips.push(text_chip(p, "strip-note".into(), &icons::CIRCLE_CHECK, note));
    }

    (!chips.is_empty()).then(|| row().key("orchestration-strip").flex_wrap().center_y().gaps(4., 12.).children(chips))
}

/// Live delegated children: title, engine, status, elapsed, open, cancel.
fn frame(p: &Palette, thread: &str, o: &OrchestrationState, k: &Kit) -> Option<El> {
    let live: Vec<&TaskView> = o.tasks.iter().filter(|t| t.status.is_live()).collect();
    if live.is_empty() {
        return None;
    }
    let now = Utc::now().timestamp_millis();
    let mut rows = col()
        .key("subagents")
        .padding(4.)
        .radius(design::PANEL_RADIUS)
        .border(1., p.border)
        .child(el().px(8.).pt(4.).child(txt("Working in the background", XS, p.text_muted)));
    for task in live {
        let status = match (task.status, task.limit_reset_at) {
            (TaskStatus::Waiting, Some(at)) => format!("Usage limit, resumes {}", countdown(at, now)),
            (TaskStatus::WaitingChildren, _) if task.pending_children > 0 => {
                format!("Waiting for {} subagent(s)", task.pending_children)
            }
            (status, _) => status.label().to_string(),
        };
        let elapsed = task.started_at.map(|st| duration((now - st).max(0) / 1000));
        let cancelling = task.cancel_requested;
        let icon = match task.status {
            TaskStatus::Running | TaskStatus::Starting => spinner(k, ICON_XS, p.text),
            _ => ico(&icons::PAUSE, ICON_XS, p.text),
        };
        let (task_id, chat) = (task.task_id.clone(), thread.to_string());
        let tip = if cancelling { "Cancelling" } else { "Cancel this subagent and everything it delegated" };
        let cancel = if cancelling {
            spinner(k, ICON_XS, p.text_muted).help(tip)
        } else {
            ghost_icon(k, format!("cancel-{}", task.task_id), &icons::CLOSE, tip, move |s| {
                cancel_task(s, &chat, task_id.clone())
            })
            .label("Cancel subagent")
        };
        let mut r = row()
            .key(format!("subagent-{}", task.task_id))
            .gap(8.)
            .px(8.)
            .py(4.)
            .center_y()
            .radius(design::ROW_RADIUS)
            .child(icon)
            .child(txt_medium(task.title.clone(), SM, p.text).truncate().grow(1.).min_w(0.))
            .child(txt(task.engine_label(), XS, p.text_muted))
            .child(txt(
                match elapsed {
                    Some(e) => format!("{status} · {e}"),
                    None => status,
                },
                XS,
                p.text_muted,
            ))
            .child(cancel);
        if let Some(child) = task.child_thread_id.clone() {
            r = r
                .hover(p.hover)
                .cursor(rune::Cursor::Pointer)
                .role("button")
                .on_press(move |s: &mut State| crate::workspace::open_thread(s, child.clone()));
        }
        rows = rows.child(r);
    }
    Some(rows)
}

/// The queue's controls: hold or release, clear, and add a follow-up. The
/// queued turns themselves show at the transcript's end.
fn queue_panel(p: &Palette, thread: &str, o: &OrchestrationState, k: &Kit) -> El {
    let busy = o.busy;
    let held = o.queue.held;
    let empty = o.queue.items.is_empty();
    let id = thread.to_string();
    let hold_id = id.clone();
    let mut header = row()
        .px(8.)
        .pt(4.)
        .gap(8.)
        .center_y()
        .child(
            txt(
                if held {
                    "On hold: nothing queued is sent"
                } else {
                    "Follow-ups are sent in order once the current turn ends"
                },
                XS,
                p.text_muted,
            )
            .grow(1.)
            .min_w(0.),
        )
        .child(ghost_button(
            k,
            "queue-hold",
            if held { "Release" } else { "Hold" },
            Some(if held { &icons::PLAY } else { &icons::PAUSE }),
            busy,
            move |s| queue_action(s, &hold_id, if held { QueueAction::Release } else { QueueAction::Hold }),
        ));
    if !empty {
        let clear_id = id.clone();
        header = header.child(ghost_button(k, "queue-clear", "Clear", None, busy, move |s| {
            queue_action(s, &clear_id, QueueAction::Clear)
        }));
    }
    let mut panel = col()
        .key("queue-panel")
        .gap(4.)
        .padding(4.)
        .radius(design::PANEL_RADIUS)
        .border(1., p.border)
        .fill(p.surface_raised)
        .child(header);
    if empty {
        panel = panel.child(el().px(8.).child(txt("Nothing queued.", XS, p.text_muted)));
    }
    if let Some(input) = &o.queue_input {
        let (edit_id, submit_id, add_id) = (id.clone(), id.clone(), id);
        panel = panel.child(
            row()
                .gap(8.)
                .px(4.)
                .pb(4.)
                .center_y()
                .child(
                    el().grow(1.).min_w(0.).child(
                        TextInput::new(input)
                            .placeholder("Add a follow-up to the queue")
                            .width(None)
                            .on_edit(move |s: &mut State| queue_edit(s, &edit_id))
                            .on_submit(move |s: &mut State, _| add_queued(s, &submit_id))
                            .build(k)
                            .key("queue-input"),
                    ),
                )
                .child(if busy {
                    el().size(24., 24.)
                } else {
                    ghost_icon(k, "queue-add", &icons::PLUS, "Add to the queue", move |s| add_queued(s, &add_id))
                }),
        );
    }
    panel
}

fn queue_edit<'a>(s: &'a mut State, id: &str) -> &'a mut TextEdit {
    match crate::chat::chat_for(s, id).and_then(|c| c.orchestration.queue_input.as_mut()) {
        Some(edit) => edit,
        // A late edit after the panel closed lands in a scratch field.
        None => crate::chat::orphan_edit(),
    }
}
