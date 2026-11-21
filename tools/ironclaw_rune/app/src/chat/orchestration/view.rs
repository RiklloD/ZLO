//! Orchestration's views.

use chrono::Utc;
use rune::{El, Icon, col, el, row};
use rune_kit::{ButtonVariant, Kit};
use zlo_client::orchestration::{
    HoldReason, QueueAction, QueueItem, QueueItemKind, TaskStatus, TaskView,
};

use super::{
    OrchestrationState, cancel_task, countdown, delegation, duration, merge_back, queue_action,
};
use crate::chat::ChatState;
use crate::chat::dispatch;
use crate::design::kit::compact::{
    ICON_XS, SM, XS, ghost_button, ghost_icon, ghost_icon_tx, ico, spinner, txt, txt_medium,
    variant_button,
};
use crate::design::{self, Palette, motion};
use crate::icons;
use crate::state::State;

/// Zeron's reading column; Appearance → Conversation width moves it.
fn content_width(s: &State) -> f32 {
    s.prefs.conversation_width.clamp(480., 1200.)
}

/// What stays on the composer: notices that change what a send does (the
/// strip), the queued messages (t3code's QueuedRunsControl), and the goal
/// editor while open. `plan` is
/// the chat's plan strip, drawn first. Work, goals and queued turns live at
/// the transcript's end ([`tail`]), as in T3 Code's timeline pass.
pub fn composer(s: &State, chat: &ChatState, k: &Kit, plan: Option<El>) -> Option<El> {
    let goal = chat
        .goals
        .editing()
        .then(|| crate::chat::goals::view(s, chat, k))
        .flatten();
    let strip = strip(s, chat, k);
    let queue = queue_panel(s, chat, k);
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
    // The block wipes open above the composer; each part inside it wipes
    // in on its own once the block is up (a queued message, the goal
    // editor, a note). Both play only on commits the person caused.
    Some(
        row()
            .key("orchestration")
            .w_full()
            .px(16.)
            .pt(8.)
            .justify_center()
            .transition(motion::reveal())
            .child(inner),
    )
}

/// The transcript's last row, in T3's order: live subagents (work that may
/// need you), then the goal.
pub fn tail(s: &State, chat: &ChatState, k: &Kit) -> Option<El> {
    let o = &chat.orchestration;
    let id = chat.thread_id.as_str();
    let frame = frame(&s.palette, id, o, k);
    let goal = (!chat.goals.editing())
        .then(|| crate::chat::goals::view(s, chat, k))
        .flatten();
    if frame.is_none() && goal.is_none() {
        return None;
    }
    Some(
        col()
            .key("orchestration-tail")
            .w_full()
            .gap(8.)
            .children(frame)
            .children(goal),
    )
}

/// The queued messages above the composer, t3code's QueuedRunsControl:
/// dashed unsent bubbles on the sender's side, "Queued message i of n",
/// and per row Send now, Edit and Remove. A queue longer than three shows
/// its first two and "Show all n queued".
fn queue_panel(s: &State, chat: &ChatState, k: &Kit) -> Option<El> {
    let o = &chat.orchestration;
    let n = o.queue.items.len();
    if n == 0 {
        return None;
    }
    let thread = chat.thread_id.as_str();
    let running = chat.running_run().is_some();
    let (shown, toggle) = dispatch::queue_rows(n, o.queue_expanded);
    let mut list = col()
        .key("queued")
        .w_full()
        .gap(8.)
        .role("list")
        .label(format!("{n} queued"))
        .transition(motion::reveal());
    for (ix, item) in o.queue.items.iter().enumerate().take(shown) {
        list = list.child(queued_row(s, thread, o, item, ix, n, running, k));
    }
    if let Some(label) = toggle {
        let chat_id = thread.to_string();
        list = list.child(row().w_full().justify_end().child(ghost_button(
            k,
            "queue-more",
            &label,
            None,
            false,
            move |s| {
                if let Some(c) = crate::chat::chat_for(s, &chat_id) {
                    c.orchestration.queue_expanded = !c.orchestration.queue_expanded;
                }
            },
        )));
    }
    Some(list)
}

/// What a queued row says under its bubble.
pub(crate) fn queued_caption(item: &QueueItem, ix: usize, n: usize, held: bool) -> String {
    let mut line = format!("Queued message {} of {n}", ix + 1);
    if item.is_sending() {
        line.push_str(" · sending");
        return line;
    }
    match item.kind {
        QueueItemKind::RecoveredSteer => line.push_str(" · a steer the agent didn't read"),
        QueueItemKind::DeferredSteer => line.push_str(" · steers in once the turn is ready"),
        QueueItemKind::Message | QueueItemKind::Other => {}
    }
    if held {
        line.push_str(" · on hold");
    }
    line
}

#[allow(clippy::too_many_arguments)]
fn queued_row(
    s: &State,
    thread: &str,
    o: &OrchestrationState,
    item: &QueueItem,
    ix: usize,
    n: usize,
    running: bool,
    k: &Kit,
) -> El {
    let p = &s.palette;
    let busy = o.busy;
    let editing = o.editing.as_ref().is_some_and(|e| e.item_id == item.id);
    let bubble_max = content_width(s) * 0.8;
    let mut actions = row().gap(1.).center_y();
    if item.is_sending() {
        // A delivery holds it: in progress, nothing to act on.
        actions = actions.child(spinner(k, ICON_XS, p.text_muted));
    } else if editing {
        let chat = thread.to_string();
        actions = actions
            .child(txt("Editing in the composer", XS, p.text_muted))
            .child(ghost_button(
                k,
                format!("queued-cancel-{}", item.id),
                "Cancel",
                None,
                false,
                move |s| crate::chat::steer::cancel_edit(s, &chat),
            ));
    } else {
        let mut hover = row().gap(1.).center_y().group_opacity(0., 1.);
        let send_tip = match (running, ix) {
            (false, _) => "Start a turn with this message".to_string(),
            (true, 0) => "Send into the current turn (⇧⌘↩)".to_string(),
            (true, _) => "Send into the current turn".to_string(),
        };
        let edit_tip = if ix + 1 == n {
            "Edit in the composer (⌥↑)"
        } else {
            "Edit in the composer"
        };
        let (send_id, edit_id, remove_id) = (item.id.clone(), item.id.clone(), item.id.clone());
        let (c1, c2, c3) = (thread.to_string(), thread.to_string(), thread.to_string());
        // Compaction takes no steer: the item waits for it.
        if !busy && !o.queue.compacting {
            hover = hover.child(ghost_icon_tx(
                k,
                format!("queued-send-{}", item.id),
                &icons::ARROW_UP,
                &send_tip,
                move |s, tx| crate::chat::steer::send_now(s, &c1, &send_id, tx),
            ));
        }
        if !busy {
            hover = hover
                .child(ghost_icon(
                    k,
                    format!("queued-edit-{}", item.id),
                    &icons::PENCIL,
                    edit_tip,
                    move |s| crate::chat::steer::edit_queued(s, &c2, &edit_id),
                ))
                .child(ghost_icon(
                    k,
                    format!("queued-remove-{}", item.id),
                    &icons::CLOSE,
                    "Remove from queue",
                    move |s| queue_action(s, &c3, QueueAction::Remove(remove_id.clone())),
                ));
        }
        actions = actions.child(hover);
    }
    let caption = queued_caption(item, ix, n, o.queue.held);
    let text = if item.text.trim().is_empty() {
        "Queued message".to_string()
    } else {
        item.text.clone()
    };
    col()
        .key(format!("queued-row-{}", item.id))
        // Queued, removed or sent: the row rises in and wipes away.
        .transition(motion::reveal().combined(rune::Transition::offset(0., 6.)))
        .hover_group()
        .w_full()
        .items_end()
        .gap(4.)
        .role("listitem")
        .label(format!("Queued message {} of {n}, not sent", ix + 1))
        // In a full-width row, as the person's own bubbles: the row
        // shrinks it to a narrow pane (a side chat's).
        .child(
            row().w_full().justify_end().child(
                el().shrink()
                    .max_w(bubble_max)
                    .px(16.)
                    .py(10.)
                    .radius(design::COMPOSER_RADIUS)
                    .border(
                        1.,
                        if editing {
                            p.text_muted
                        } else {
                            p.border_strong
                        },
                    )
                    .border_dash(4., 3.)
                    .child(txt(text, SM, p.text_muted).wrap().max_lines(4).shrink()),
            ),
        )
        .child(
            row()
                .gap(4.)
                .center_y()
                .child(actions)
                .child(txt(caption, XS, p.text_faint)),
        )
}

/// A clickable muted line in the strip: its ink brightens under the
/// pointer and it sinks a little while pressed.
fn link_chip(p: &Palette, key: &str, i: &'static Icon, label: String, open: String) -> El {
    row()
        .key(key)
        .role("button")
        .label(label.clone())
        .gap(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .hover_motion(motion::HOVER, motion::HOVER)
        .child(ico(i, ICON_XS, p.text_muted).group_color(p.text))
        .child(txt(label, XS, p.text_muted).group_color(p.text))
        .on_press(move |s: &mut State| crate::workspace::open_thread(s, open.clone()))
        .press_scale(motion::PRESS)
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
    let title_of =
        |id: &str| crate::workspace::title_of(s, id).unwrap_or_else(|| "a conversation".into());

    if let Some(task) = delegation(&chat.thread_id) {
        let parent = task.parent_thread_id.clone();
        chips.push(link_chip(
            p,
            "strip-parent",
            &icons::BOT,
            format!("Delegated from {}", title_of(&parent)),
            parent,
        ));
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
            chips.push(link_chip(
                p,
                "strip-fork",
                &icons::COPY,
                format!("Forked from {}", title_of(&source)),
                source,
            ));
        }
        let merging = id.clone();
        let merge = variant_button(
            k,
            "merge-back",
            "Merge back",
            Some(&icons::UNDO_2),
            ButtonVariant::Ghost,
            busy,
            move |s| merge_back(s, &merging),
        )
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
            Some(HoldReason::Interrupted) => (
                &icons::TRIANGLE_ALERT,
                "Interrupted when the app quit",
                "Continue",
            ),
            Some(HoldReason::Stopped | HoldReason::Cancelled) => {
                (&icons::PAUSE, "Paused after Stop", "Resume queue")
            }
            _ => (&icons::PAUSE, "Queue on hold", "Release"),
        };
        chips.push(text_chip(p, "strip-held".into(), i, label));
        let interrupted = o.queue.hold_reason == Some(HoldReason::Interrupted);
        let variant = if interrupted {
            ButtonVariant::Submit
        } else {
            ButtonVariant::Ghost
        };
        let releasing = id.clone();
        chips.push(
            variant_button(
                k,
                "queue-release",
                action,
                Some(&icons::PLAY),
                variant,
                busy,
                move |s| queue_action(s, &releasing, QueueAction::Release),
            )
            .help(if interrupted {
                "Finish the cut turn, then run the queued follow-ups"
            } else {
                "Let queued follow-ups run again"
            }),
        );
    }

    if let Some(note) = o.note.clone() {
        chips.push(
            text_chip(p, "strip-note".into(), &icons::CIRCLE_CHECK, note)
                .transition(motion::fade()),
        );
    }

    (!chips.is_empty()).then(|| {
        row()
            .key("orchestration-strip")
            .transition(motion::reveal())
            .flex_wrap()
            .center_y()
            .gaps(4., 12.)
            .children(chips)
    })
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
        .child(
            el().px(8.)
                .pt(4.)
                .child(txt("Working in the background", XS, p.text_muted)),
        );
    for task in live {
        let status = match (task.status, task.limit_reset_at) {
            (TaskStatus::Waiting, Some(at)) => {
                format!("Usage limit, resumes {}", countdown(at, now))
            }
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
        let tip = if cancelling {
            "Cancelling"
        } else {
            "Cancel this subagent and everything it delegated"
        };
        // Keyed apart, so Cancel gives way to the spinner with a fade.
        let cancel = if cancelling {
            spinner(k, ICON_XS, p.text_muted)
                .help(tip)
                .key(format!("cancelling-{}", task.task_id))
                .transition(motion::fade())
        } else {
            ghost_icon(
                k,
                format!("cancel-{}", task.task_id),
                &icons::CLOSE,
                tip,
                move |s| cancel_task(s, &chat, task_id.clone()),
            )
            .label("Cancel subagent")
            .transition(motion::fade())
        };
        let mut r = row()
            .key(format!("subagent-{}", task.task_id))
            .gap(8.)
            .px(8.)
            .py(4.)
            .center_y()
            .radius(design::ROW_RADIUS)
            .transition(motion::reveal())
            .child(icon)
            .child(
                txt_medium(task.title.clone(), SM, p.text)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            )
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
                .hover_motion(motion::HOVER, motion::HOVER)
                .cursor(rune::Cursor::Pointer)
                .role("button")
                .on_press(move |s: &mut State| crate::workspace::open_thread(s, child.clone()))
                .press_scale(motion::PRESS_CARD);
        }
        rows = rows.child(r);
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str) -> QueueItem {
        serde_json::from_value(serde_json::json!({"id": "q", "text": "hi", "kind": kind}))
            .expect("item decodes")
    }

    #[test]
    fn queued_rows_say_where_they_stand_and_what_kind_they_are() {
        assert_eq!(
            queued_caption(&item("message"), 0, 3, false),
            "Queued message 1 of 3"
        );
        assert_eq!(
            queued_caption(&item("recovered_steer"), 1, 3, true),
            "Queued message 2 of 3 · a steer the agent didn't read · on hold"
        );
        assert_eq!(
            queued_caption(&item("deferred_steer"), 0, 1, false),
            "Queued message 1 of 1 · steers in once the turn is ready"
        );
        assert_eq!(item("something_new").kind, QueueItemKind::Other);
        let sending: QueueItem = serde_json::from_value(
            serde_json::json!({"id": "q", "text": "hi", "status": "sending"}),
        )
        .expect("item decodes");
        assert_eq!(
            queued_caption(&sending, 0, 2, true),
            "Queued message 1 of 2 · sending",
            "in progress, not on hold"
        );
    }
}
