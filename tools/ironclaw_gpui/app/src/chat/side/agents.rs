//! Agents: the subagents this conversation delegated to and its forks.
//! MonoCode's Sessions tab over the orchestrator routes the chat already
//! polls (`/tasks`, `/lineage`), so this tab adds no requests of its own.

use chrono::Utc;
use ironclaw_gpui_client::orchestration::{TaskStatus, TaskView};

use super::{ChatView, empty, heading, scroll};
use crate::assets::ExtraIcon;
use crate::design::{Palette, kit};
use crate::ui::*;

fn status_color(status: TaskStatus, p: &Palette) -> gpui_kit::Hsla {
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
    let end = task
        .finished_at
        .unwrap_or_else(|| Utc::now().timestamp_millis());
    let secs = ((end - start) / 1000).max(0);
    Some(match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    })
}

impl ChatView {
    pub(super) fn render_agents(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let mut tasks = self.orch.tasks.clone();
        tasks.sort_by_key(|t| (!t.status.is_live(), -t.created_at));
        let lineage = self.orch.lineage.clone();
        let mut col = scroll("agents-list");

        col = col.child(
            heading(format!("Subagents · {}", tasks.len()), cx)
                .child(div().flex_1())
                .child(
                    Button::new("agents-fork")
                        .label("Fork")
                        .icon(ExtraIcon::GitFork)
                        .ghost()
                        .xsmall()
                        .tooltip("Fork this conversation")
                        .on_click(cx.listener(|this, _, _, cx| this.fork(None, cx))),
                ),
        );
        if tasks.is_empty() {
            col = col.child(empty(
                "No subagents yet. When IronClaw hands work to another agent, it shows up here with its status and result.",
                cx,
            ));
        }
        for task in tasks {
            let live = task.status.is_live();
            let color = status_color(task.status, &p);
            let child = task.child_thread_id.clone();
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
            col = col.child(
                v_flex()
                    .mx_2()
                    .px_2()
                    .py_2()
                    .gap_1()
                    .rounded(px(8.))
                    .hover(|s| s.bg(kit::hover_wash(&p)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(kit::status_dot(
                                SharedString::from(format!("agent-dot-{id}")),
                                color,
                                live,
                            ))
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(
                                if task.title.is_empty() {
                                    "Untitled task".to_string()
                                } else {
                                    task.title.clone()
                                },
                            ))
                            .when(live && !task.cancel_requested, |d| {
                                let id = id.clone();
                                d.child(
                                    Button::new(SharedString::from(format!("agent-cancel-{id}")))
                                        .icon(IconName::Square)
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Cancel")
                                        .accessibility_label("Cancel this subagent")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_task(id.clone(), cx)
                                        })),
                                )
                            })
                            .when_some(child, |d, child| {
                                d.child(
                                    Button::new(SharedString::from(format!("agent-open-{id}")))
                                        .icon(IconName::ArrowRight)
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Open its conversation")
                                        .accessibility_label("Open its conversation")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.open_thread(child.clone(), cx)
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .pl(px(16.))
                            .text_xs()
                            .text_color(p.text_faint)
                            .child(meta),
                    )
                    .when_some(note, |d, note| {
                        d.child(
                            div()
                                .pl(px(16.))
                                .text_xs()
                                .text_color(if task.status == TaskStatus::Failed {
                                    p.danger
                                } else {
                                    p.text_muted
                                })
                                .whitespace_normal()
                                .line_clamp(3)
                                .child(note),
                        )
                    }),
            );
        }

        let mut related: Vec<(&'static str, String)> = Vec::new();
        if let Some(from) = &lineage.forked_from {
            related.push(("Forked from", from.source_thread.clone()));
        }
        for fork in &lineage.forks {
            if let Some(target) = &fork.target_thread {
                related.push(("Fork", target.clone()));
            }
        }
        if !related.is_empty() {
            col = col.child(heading("Forks", cx));
            for (ix, (label, thread)) in related.into_iter().enumerate() {
                let title =
                    crate::titles::get(cx, &thread).unwrap_or_else(|| "Conversation".into());
                col = col.child(
                    kit::row(format!("agent-fork-{ix}"), false, cx)
                        .mx_2()
                        .child(
                            Icon::new(ExtraIcon::GitFork)
                                .xsmall()
                                .text_color(p.text_faint),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(p.text_faint)
                                .child(label),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(title))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_thread(thread.clone(), cx)),
                        ),
                );
            }
        }
        col.into_any_element()
    }
}
