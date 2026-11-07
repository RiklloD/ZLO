//! Routines: scheduled automations with their next run, last result and
//! Run now / Pause / Resume, beside the chat (GPUI `chat/side/routines.rs`).
//! MonoCode's Habits list over `GET /automations` and `GET /schedules`
//! (agent- or page-made schedules and webhooks). Automations are global, so
//! every one is listed here; they are created and edited on the Automations
//! page.

use std::collections::HashSet;

use chrono::Utc;
use ironclaw_gpui_client::AutomationAction;
use ironclaw_gpui_client::product_wire::{
    RebornAutomationInfo, RebornAutomationRecentRunStatus, RebornAutomationSource,
    RebornAutomationState,
};
use ironclaw_gpui_client::schedules::{LastOutcome, ScheduleInfo};
use rune::{El, col, icon, row};
use rune_kit::Kit;

use super::ui::{self, SM, XS};
use super::{bridge, side, side_mut};
use crate::icons;
use crate::jobs;
use crate::state::State;

#[derive(Default)]
pub(super) struct RoutinesState {
    list: Option<Result<Vec<RebornAutomationInfo>, String>>,
    scheduler_off: bool,
    busy: HashSet<String>,
    schedules: Vec<ScheduleInfo>,
}

/// "in 5 min", "in 3 h", or a local date for later.
fn until(at: chrono::DateTime<Utc>) -> String {
    let secs = (at - Utc::now()).num_seconds();
    match secs {
        s if s <= 0 => "due now".into(),
        s if s < 3600 => format!("in {} min", (s / 60).max(1)),
        s if s < 86_400 => format!("in {} h", s / 3600),
        _ => jobs::when(at),
    }
}

fn schedule(source: &RebornAutomationSource) -> String {
    match source {
        RebornAutomationSource::Schedule { cron, timezone } => format!("{cron} ({timezone})"),
        RebornAutomationSource::Once { at, .. } => format!("Once at {at}"),
    }
}

/// Both lists. Routines are global, so a late answer applies to any thread.
pub(super) fn load(s: &mut State) {
    jobs::run(
        s,
        "routines",
        |c| async move { c.list_schedules().await },
        |s, result, _| match result {
            Ok(list) => {
                if let Some(x) = side_mut(s) {
                    x.routines.schedules = list;
                }
            }
            // silent-ok: a gateway without schedules lists routines only.
            Err(e) if e.is_not_found() => {
                if let Some(x) = side_mut(s) {
                    x.routines.schedules.clear();
                }
            }
            Err(e) => bridge::set_error(s, e.to_string()),
        },
    );
    jobs::run(
        s,
        "routines",
        |c| async move { c.list_automations().await },
        |s, result, _| {
            let Some(x) = side_mut(s) else { return };
            match result {
                Ok(list) => {
                    x.routines.scheduler_off = !list.scheduler_enabled;
                    x.routines.list = Some(Ok(list.automations));
                }
                Err(e) => x.routines.list = Some(Err(e.to_string())),
            }
        },
    );
}

fn schedule_action(s: &mut State, id: String, enabled: Option<bool>) {
    let key = format!("schedule:{id}");
    let Some(x) = side_mut(s) else { return };
    if !x.routines.busy.insert(key.clone()) {
        return;
    }
    jobs::run(
        s,
        "routines",
        move |c| async move {
            match enabled {
                Some(on) => c.set_schedule_enabled(&id, on).await.map(|_| ()),
                None => c.run_schedule(&id).await.map(|_| ()),
            }
        },
        move |s, result, _| {
            if let Some(x) = side_mut(s) {
                x.routines.busy.remove(&key);
            }
            if let Err(e) = result {
                bridge::set_error(s, e.to_string());
            }
            load(s);
        },
    );
}

fn routine_action(s: &mut State, id: String, action: AutomationAction) {
    let Some(x) = side_mut(s) else { return };
    if !x.routines.busy.insert(id.clone()) {
        return;
    }
    let key = id.clone();
    jobs::run(
        s,
        "routines",
        move |c| async move { c.automation_action(&id, action).await },
        move |s, result, _| {
            if let Some(x) = side_mut(s) {
                x.routines.busy.remove(&key);
            }
            if let Err(e) = result {
                bridge::set_error(s, e.to_string());
            }
            load(s);
        },
    );
}

fn schedule_rows(s: &State, k: &Kit, state: &RoutinesState) -> Vec<El> {
    let p = &s.palette;
    state
        .schedules
        .iter()
        .map(|sch| {
            let id = sch.id.clone();
            let busy = state.busy.contains(&format!("schedule:{id}"));
            let next = sch
                .next_run_at
                .filter(|_| sch.enabled)
                .and_then(chrono::DateTime::<Utc>::from_timestamp_millis)
                .map(until);
            let status = match (sch.enabled, next) {
                (false, _) => "Paused".to_string(),
                (true, Some(n)) => format!("Next {n}"),
                (true, None) if sch.is_webhook() => "On each request".to_string(),
                (true, None) => "Scheduled".to_string(),
            };
            let button = |key: &str,
                          label: &'static str,
                          glyph: &'static rune::Icon,
                          enabled: Option<bool>| {
                let id = id.clone();
                ui::ibtn(glyph, label)
                    .disabled(busy)
                    .on_press(move |s: &mut State| schedule_action(s, id.clone(), enabled))
                    .build(k)
                    .key(format!("schedule-{key}-{}", sch.id))
            };
            let last = sch
                .last_run_at
                .and_then(chrono::DateTime::<Utc>::from_timestamp_millis);
            let thread = sch
                .last_run_thread_id
                .clone()
                .unwrap_or_else(|| sch.thread_id.clone());
            let failed = matches!(sch.last_outcome(), LastOutcome::Failed(_));
            let mut card = col()
                .key(format!("schedule-{id}"))
                .mx(8.)
                .px(8.)
                .py(8.)
                .gap(4.)
                .radius(8.)
                .hover(p.hover)
                .child(
                    row()
                        .gap(8.)
                        .items_center()
                        .child(
                            icon(
                                if sch.is_webhook() {
                                    &icons::GLOBE
                                } else {
                                    &icons::CALENDAR
                                },
                                12.,
                            )
                            .color(if sch.enabled {
                                p.text_muted
                            } else {
                                p.text_faint
                            }),
                        )
                        .child(ui::line(sch.title.clone(), SM, p.text).grow(1.).min_w(0.))
                        .child(button("run", "Run now", &icons::PLAY, None))
                        .child(if sch.enabled {
                            button("pause", "Pause", &icons::PAUSE, Some(false))
                        } else {
                            button("resume", "Resume", &icons::PLAY, Some(true))
                        }),
                )
                .child(ui::line(format!("{status} · {}", sch.describe), XS, p.text_faint).pl(22.));
            if let Some(last) = last {
                card = card.child(
                    row()
                        .key(format!("schedule-last-{id}"))
                        .pl(22.)
                        .gap(4.)
                        .items_center()
                        // Only queuing is known here; the run's thread has its result.
                        .child(ui::dot(if failed { p.danger } else { p.text_faint }, 6.))
                        .child(ui::line(
                            format!(
                                "{} {}",
                                if failed {
                                    "Last attempt failed"
                                } else {
                                    "Last queued"
                                },
                                jobs::ago(last)
                            ),
                            XS,
                            p.text_muted,
                        ))
                        .child(icon(&icons::ARROW_RIGHT, 12.).color(p.text_muted))
                        .cursor(rune::Cursor::Pointer)
                        .on_press(move |s: &mut State| {
                            crate::workspace::open_thread(s, thread.clone())
                        }),
                );
            }
            card
        })
        .collect()
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    let state = &x.routines;
    let list = match &state.list {
        None => return ui::loading(k),
        Some(Err(e)) => return ui::error_line(s, e.clone()),
        Some(Ok(list)) => list,
    };
    let p = &s.palette;
    let schedule_rows = schedule_rows(s, k, state);
    let mut out = col().w_full().child(
        ui::heading(
            s,
            format!("Routines · {}", list.len() + schedule_rows.len()),
        )
        .child(ui::grow())
        .child(
            ui::ibtn(&icons::REFRESH_CW, "Refresh")
                .label("Refresh routines")
                .on_press(load)
                .build(k)
                .key("routines-refresh"),
        ),
    );
    if state.scheduler_off {
        out = out.child(ui::error_line(
            s,
            "Scheduling is off on this gateway, so routines will not fire on their own.",
        ));
    }
    out = out.children(schedule_rows);
    if list.is_empty() && state.schedules.is_empty() {
        let empty = ui::empty(
            s,
            "No routines yet. Ask IronClaw to do something on a schedule (\"every morning at 8, check…\") and it shows up here.",
        );
        return ui::scroll("routines-list", out.child(empty));
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
        let action = |label: &'static str, glyph: &'static rune::Icon, act: AutomationAction| {
            let id = id.clone();
            ui::ibtn(glyph, label)
                .disabled(busy)
                .on_press(move |s: &mut State| routine_action(s, id.clone(), act))
                .build(k)
                .key(format!("routine-{label}-{}", info.automation_id))
        };
        let run_thread = last
            .as_ref()
            .and_then(|r| r.thread_id.as_ref())
            .map(|t| t.to_string());
        let mut head = row()
            .gap(8.)
            .items_center()
            .child(icon(&icons::CALENDAR, 12.).color(if paused {
                p.text_faint
            } else {
                p.text_muted
            }))
            .child(ui::line(info.name.clone(), SM, p.text).grow(1.).min_w(0.));
        if !finished {
            head = head
                .child(action("Run now", &icons::PLAY, AutomationAction::RunNow))
                .child(if paused {
                    action("Resume", &icons::PLAY, AutomationAction::Resume)
                } else {
                    action("Pause", &icons::PAUSE, AutomationAction::Pause)
                });
        }
        let mut card = col()
            .key(format!("routine-{id}"))
            .mx(8.)
            .px(8.)
            .py(8.)
            .gap(4.)
            .radius(8.)
            .hover(p.hover)
            .child(head)
            .child(
                ui::line(
                    format!("{status} · {}", schedule(&info.source)),
                    XS,
                    p.text_faint,
                )
                .pl(22.),
            );
        if let Some(last) = last {
            let when = jobs::ago(last.submitted_at);
            let label = match last.status {
                RebornAutomationRecentRunStatus::Error => "Last run failed",
                RebornAutomationRecentRunStatus::Running => "Running since",
                _ => "Last run",
            };
            let mut line = row()
                .key(format!("routine-last-{id}"))
                .pl(22.)
                .gap(4.)
                .items_center()
                .child(ui::dot(last_color, 6.))
                .child(ui::line(format!("{label} {when}"), XS, p.text_muted));
            if let Some(thread) = run_thread {
                line = line
                    .cursor(rune::Cursor::Pointer)
                    .child(icon(&icons::ARROW_RIGHT, 12.).color(p.text_muted))
                    .on_press(move |s: &mut State| {
                        crate::workspace::open_thread(s, thread.clone())
                    });
            }
            card = card.child(line);
        }
        out = out.child(card);
    }
    ui::scroll("routines-list", out)
}

/// Fixture lists for scenes.
pub(super) fn fixture(
    x: &mut super::SideState,
    schedules: Vec<ScheduleInfo>,
    list: Vec<RebornAutomationInfo>,
) {
    x.routines.schedules = schedules;
    x.routines.list = Some(Ok(list));
}
