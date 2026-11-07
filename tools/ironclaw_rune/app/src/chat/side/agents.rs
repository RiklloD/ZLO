//! Agents: the subagents this conversation delegated to and its forks (GPUI
//! `chat/side/agents.rs`). MonoCode's Sessions tab over the orchestrator
//! routes the chat already polls (`/tasks`, `/lineage`, area H), so this tab
//! adds no requests of its own.

use chrono::Utc;
use ironclaw_gpui_client::orchestration::{TaskStatus, TaskView};
use rune::{Color, El, col, icon, row};
use rune_kit::Kit;

use super::bridge;
use super::ui::{self, SM, XS};
use crate::design::Palette;
use crate::icons;
use crate::state::State;

fn status_color(status: TaskStatus, p: &Palette) -> Color {
    match status {
        TaskStatus::Completed => p.success,
        TaskStatus::Failed | TaskStatus::Interrupted => p.danger,
        TaskStatus::Cancelled | TaskStatus::Unknown => p.text_faint,
        _ => p.accent,
    }
}

/// "3 min" from Unix milliseconds to now (or to the finish).
fn elapsed(task: &TaskView) -> Option<String> {
    let start = task.started_at.unwrap_or(task.created_at);
    if start <= 0 {
        return None;
    }
    let end = task.finished_at.unwrap_or_else(|| Utc::now().timestamp_millis());
    let secs = ((end - start) / 1000).max(0);
    Some(match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    })
}

/// The status dot: a soft pulse while the task is live (GPUI `kit::status_dot`).
fn status_dot(color: Color, live: bool) -> El {
    let d = ui::dot(color, 8.);
    if live { d.looping(rune::LoopSpec::opacity(1.2, 0.0, 0.45, 1.0)) } else { d }
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let mut tasks: Vec<TaskView> = bridge::tasks(s).to_vec();
    tasks.sort_by_key(|t| (!t.status.is_live(), -t.created_at));
    let mut list = col().w_full().child(
        ui::heading(s, format!("Subagents · {}", tasks.len()))
            .child(ui::grow())
            .child(
                ui::btn("Fork")
                    .icon(&icons::GIT_FORK)
                    .help("Fork this conversation")
                    .on_press(bridge::fork)
                    .build(k)
                    .key("agents-fork"),
            ),
    );
    if tasks.is_empty() {
        list = list.child(ui::empty(
            s,
            "No subagents yet. When IronClaw hands work to another agent, it shows up here with its status and result.",
        ));
    }
    for task in tasks {
        let live = task.status.is_live();
        let color = status_color(task.status, p);
        let id = task.task_id.clone();
        let note = task
            .error
            .clone()
            .or(task.summary.clone())
            .filter(|s| !s.trim().is_empty());
        let meta = [
            Some(task.status.label().to_string()),
            Some(task.engine_label()).filter(|s| !s.is_empty()),
            elapsed(&task),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let title = if task.title.is_empty() { "Untitled task".to_string() } else { task.title.clone() };
        let mut head = row()
            .gap(8.)
            .items_center()
            .child(status_dot(color, live))
            .child(ui::line(title, SM, p.text).grow(1.).min_w(0.));
        if live && !task.cancel_requested {
            let id = id.clone();
            head = head.child(
                ui::ibtn(&icons::SQUARE, "Cancel")
                    .label("Cancel this subagent")
                    .on_press(move |s: &mut State| bridge::cancel_task(s, id.clone()))
                    .build(k)
                    .key(format!("agent-cancel-{}", task.task_id)),
            );
        }
        if let Some(child) = task.child_thread_id.clone() {
            head = head.child(
                ui::ibtn(&icons::ARROW_RIGHT, "Open its conversation")
                    .on_press(move |s: &mut State| crate::workspace::open_thread(s, child.clone()))
                    .build(k)
                    .key(format!("agent-open-{}", task.task_id)),
            );
        }
        let mut card = col()
            .key(format!("agent-{id}"))
            .mx(8.)
            .px(8.)
            .py(8.)
            .gap(4.)
            .radius(8.)
            .hover(p.hover)
            .child(head)
            .child(ui::line(meta, XS, p.text_faint).pl(16.));
        if let Some(note) = note {
            let ink = if task.status == TaskStatus::Failed { p.danger } else { p.text_muted };
            card = card.child(ui::para(note, XS, ink).max_lines(3).pl(16.));
        }
        list = list.child(card);
    }

    let mut related: Vec<(&'static str, String)> = Vec::new();
    if let Some(lineage) = bridge::lineage(s) {
        if let Some(from) = &lineage.forked_from {
            related.push(("Forked from", from.source_thread.clone()));
        }
        for fork in &lineage.forks {
            if let Some(target) = &fork.target_thread {
                related.push(("Fork", target.clone()));
            }
        }
    }
    if !related.is_empty() {
        list = list.child(ui::heading(s, "Forks"));
        for (ix, (label, thread)) in related.into_iter().enumerate() {
            let title = bridge::title(s, &thread).unwrap_or_else(|| "Conversation".into());
            list = list.child(
                ui::hover_row(s, format!("agent-fork-{ix}"))
                    .mx(8.)
                    .child(icon(&icons::GIT_FORK, 12.).color(p.text_faint))
                    .child(ui::line(label, XS, p.text_faint))
                    .child(ui::line(title, SM, p.text).grow(1.).min_w(0.))
                    .on_press(move |s: &mut State| crate::workspace::open_thread(s, thread.clone())),
            );
        }
    }
    ui::scroll("agents-list", list)
}
