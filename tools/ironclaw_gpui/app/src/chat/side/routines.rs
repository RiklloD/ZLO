//! Routines: scheduled automations with their next run, last result and
//! Run now / Pause / Resume, beside the chat. MonoCode's Habits list over
//! `GET /automations`. Automations are global (the gateway does not record
//! which conversation made one), so every routine is listed here.

use std::collections::HashSet;

use chrono::Utc;
use ironclaw_gpui_client::AutomationAction;
use ironclaw_gpui_client::product_wire::{
    RebornAutomationInfo, RebornAutomationRecentRunStatus, RebornAutomationSource,
    RebornAutomationState,
};

use super::{ChatView, empty, error_line, heading, loading, scroll};
use crate::design::{Palette, kit};
use crate::jobs::run;
use crate::ui::*;

#[derive(Default)]
pub(super) struct State {
    list: Option<Result<Vec<RebornAutomationInfo>, String>>,
    scheduler_off: bool,
    busy: HashSet<String>,
}

/// "in 5 min", "in 3 h", or a local date for later.
fn until(at: chrono::DateTime<Utc>) -> String {
    let secs = (at - Utc::now()).num_seconds();
    match secs {
        s if s <= 0 => "due now".into(),
        s if s < 3600 => format!("in {} min", (s / 60).max(1)),
        s if s < 86_400 => format!("in {} h", s / 3600),
        _ => crate::jobs::when(at),
    }
}

fn schedule(source: &RebornAutomationSource) -> String {
    match source {
        RebornAutomationSource::Schedule { cron, timezone } => format!("{cron} ({timezone})"),
        RebornAutomationSource::Once { at, .. } => format!("Once at {at}"),
    }
}

impl ChatView {
    pub(super) fn load_routines(&mut self, cx: &mut Context<Self>) {
        run(
            &self.backend,
            cx,
            |c| async move { c.list_automations().await },
            |this, result, cx| {
                let state = &mut this.side.routines;
                match result {
                    Ok(list) => {
                        state.scheduler_off = !list.scheduler_enabled;
                        state.list = Some(Ok(list.automations));
                    }
                    Err(e) => state.list = Some(Err(e.to_string())),
                }
                cx.notify();
            },
        );
    }

    fn routine_action(&mut self, id: String, action: AutomationAction, cx: &mut Context<Self>) {
        if !self.side.routines.busy.insert(id.clone()) {
            return;
        }
        cx.notify();
        let key = id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.automation_action(&id, action).await },
            move |this, result, cx| {
                this.side.routines.busy.remove(&key);
                if let Err(e) = result {
                    this.set_error(e.to_string(), cx);
                }
                this.load_routines(cx);
            },
        );
    }

    pub(super) fn render_routines(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let state = &self.side.routines;
        let list = match &state.list {
            None => return loading(),
            Some(Err(e)) => return error_line(e.clone(), cx),
            Some(Ok(list)) => list.clone(),
        };
        let mut col = scroll("routines-list").child(
            heading(format!("Routines · {}", list.len()), cx)
                .child(div().flex_1())
                .child(
                    Button::new("routines-refresh")
                        .icon(IconName::RefreshCw)
                        .ghost()
                        .xsmall()
                        .tooltip("Refresh")
                        .accessibility_label("Refresh routines")
                        .on_click(cx.listener(|this, _, _, cx| this.load_routines(cx))),
                ),
        );
        if state.scheduler_off {
            col = col.child(error_line(
                "Scheduling is off on this gateway, so routines will not fire on their own.",
                cx,
            ));
        }
        if list.is_empty() {
            return col
                .child(empty(
                    "No routines yet. Ask IronClaw to do something on a schedule (\"every morning at 8, check…\") and it shows up here.",
                    cx,
                ))
                .into_any_element();
        }
        for info in list {
            let id = info.automation_id.clone();
            let busy = state.busy.contains(&id);
            let paused = matches!(
                info.state,
                RebornAutomationState::Paused | RebornAutomationState::Disabled
            );
            let finished = info.state == RebornAutomationState::Completed;
            let next = info.next_run_at.filter(|_| !paused && !finished).map(until);
            let last = info.recent_runs.first().cloned();
            let last_color = match last.as_ref().map(|r| r.status) {
                Some(RebornAutomationRecentRunStatus::Error) => p.danger,
                Some(RebornAutomationRecentRunStatus::Ok) => p.success,
                Some(RebornAutomationRecentRunStatus::Running) => p.accent,
                _ => p.text_faint,
            };
            let status = if paused {
                "Paused".to_string()
            } else if finished {
                "Done".to_string()
            } else {
                next.map(|n| format!("Next {n}"))
                    .unwrap_or_else(|| "Scheduled".into())
            };
            let action = |label: &'static str, icon: IconName, act: AutomationAction| {
                let id = id.clone();
                Button::new(SharedString::from(format!(
                    "routine-{label}-{}",
                    info.automation_id
                )))
                .icon(icon)
                .ghost()
                .xsmall()
                .tooltip(label)
                .accessibility_label(label)
                .disabled(busy)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.routine_action(id.clone(), act, cx)),
                )
            };
            let run_thread = last
                .as_ref()
                .and_then(|r| r.thread_id.as_ref())
                .map(|t| t.to_string());
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
                            .child(
                                Icon::new(IconName::Calendar)
                                    .xsmall()
                                    .text_color(if paused { p.text_faint } else { p.text_muted }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .child(info.name.clone()),
                            )
                            .when(!finished, |d| {
                                d.child(action("Run now", IconName::Play, AutomationAction::RunNow))
                                    .child(if paused {
                                        action("Resume", IconName::Play, AutomationAction::Resume)
                                    } else {
                                        action("Pause", IconName::Pause, AutomationAction::Pause)
                                    })
                            }),
                    )
                    .child(
                        div()
                            .pl(px(22.))
                            .text_xs()
                            .text_color(p.text_faint)
                            .truncate()
                            .child(format!("{status} · {}", schedule(&info.source))),
                    )
                    .when_some(last, |d, last| {
                        let when = crate::jobs::ago(last.submitted_at);
                        let label = match last.status {
                            RebornAutomationRecentRunStatus::Error => "Last run failed",
                            RebornAutomationRecentRunStatus::Running => "Running since",
                            _ => "Last run",
                        };
                        d.child(
                            h_flex()
                                .id(SharedString::from(format!("routine-last-{id}")))
                                .pl(px(22.))
                                .gap_1()
                                .items_center()
                                .text_xs()
                                .text_color(p.text_muted)
                                .child(div().size(px(6.)).rounded_full().bg(last_color))
                                .child(format!("{label} {when}"))
                                .when_some(run_thread, |d, thread| {
                                    d.cursor_pointer()
                                        .hover(|s| s.text_color(p.text))
                                        .child(Icon::new(IconName::ArrowRight).xsmall())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.open_thread(thread.clone(), cx)
                                        }))
                                }),
                        )
                    }),
            );
        }
        col.into_any_element()
    }
}
