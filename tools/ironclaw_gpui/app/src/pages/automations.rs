//! Automations: schedules (`t3_schedule` — clock or webhook, made here or
//! by an agent) with a create/edit form, and IronClaw's routines (cron and
//! one-time triggers made in a chat). Each row shows its next run and last
//! result; clicking it opens the thread of its last run.

mod form;
mod schedule_rows;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use ironclaw_gpui_client::product_wire::{
    RebornAutomationHoldReason, RebornAutomationInfo, RebornAutomationRecentRunStatus,
    RebornAutomationRunStatus, RebornAutomationSource, RebornAutomationState,
};
use ironclaw_gpui_client::schedules::{ScheduleInfo, ScheduleSecret};
use ironclaw_gpui_client::{AutomationAction, Backend};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

pub struct AutomationsView {
    backend: Backend,
    items: Vec<RebornAutomationInfo>,
    scheduler_enabled: bool,
    loaded: bool,
    error: Option<SharedString>,
    busy: HashSet<String>,
    confirm_delete: Option<String>,
    /// The automation whose name is being edited, with its input.
    renaming: Option<(String, Entity<InputState>)>,
    _rename_enter: Option<Subscription>,
    schedules: Vec<ScheduleInfo>,
    /// The open create/edit form.
    form: Option<form::Form>,
    confirm_delete_schedule: Option<String>,
    confirm_rotate: Option<String>,
    /// Webhook URLs and secrets the user revealed, by schedule id. Kept in
    /// memory only while shown; a refresh does not keep them.
    revealed: HashMap<String, ScheduleSecret>,
    show_curl: HashSet<String>,
}

#[derive(Clone, Copy)]
enum ScheduleAction {
    RunNow,
    Pause,
    Resume,
    Delete,
    RotateSecret,
}

/// Schedule ids share `busy` with routine ids.
fn schedule_key(id: &str) -> String {
    format!("schedule:{id}")
}

impl EventEmitter<PageEvent> for AutomationsView {}

impl AutomationsView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            items: Vec::new(),
            scheduler_enabled: true,
            loaded: false,
            error: None,
            busy: HashSet::new(),
            confirm_delete: None,
            renaming: None,
            _rename_enter: None,
            schedules: Vec::new(),
            form: None,
            confirm_delete_schedule: None,
            confirm_rotate: None,
            revealed: HashMap::new(),
            show_curl: HashSet::new(),
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.revealed.clear();
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.list_schedules().await },
            |this, result, cx| {
                match result {
                    Ok(list) => this.schedules = list,
                    // A gateway without the orchestrator lists routines only.
                    Err(e) if e.is_not_found() => this.schedules.clear(),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.list_automations().await },
            |this, result, cx| {
                match result {
                    Ok(page) => {
                        this.items = page.automations;
                        this.scheduler_enabled = page.scheduler_enabled;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn act(&mut self, id: String, action: AutomationAction, cx: &mut Context<Self>) {
        if !self.busy.insert(id.clone()) {
            return;
        }
        self.confirm_delete = None;
        cx.notify();
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.automation_action(&job_id, action).await },
            move |this, result, cx| {
                this.busy.remove(&id);
                match result {
                    // Show the gateway's read-back, then re-list for run history.
                    Ok(resp) => {
                        if let Some(updated) = resp.automation
                            && let Some(row) = this.items.iter_mut().find(|a| a.automation_id == id)
                        {
                            *row = updated;
                        }
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn schedule_act(&mut self, id: String, action: ScheduleAction, cx: &mut Context<Self>) {
        let key = schedule_key(&id);
        if !self.busy.insert(key.clone()) {
            return;
        }
        self.confirm_delete_schedule = None;
        self.confirm_rotate = None;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                match action {
                    ScheduleAction::RunNow => c.run_schedule(&id).await.map(|_| ()),
                    ScheduleAction::Pause => c.set_schedule_enabled(&id, false).await.map(|_| ()),
                    ScheduleAction::Resume => c.set_schedule_enabled(&id, true).await.map(|_| ()),
                    ScheduleAction::Delete => c.delete_schedule(&id).await,
                    ScheduleAction::RotateSecret => c.rotate_schedule_secret(&id).await.map(|_| ()),
                }
            },
            move |this, result, cx| {
                this.busy.remove(&key);
                match result {
                    Ok(()) => {
                        this.error = None;
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn start_rename(
        &mut self,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        input.update(cx, |s, cx| s.focus(window, cx));
        self._rename_enter = Some(cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save_rename(cx);
            }
        }));
        self.renaming = Some((id, input));
        cx.notify();
    }

    fn save_rename(&mut self, cx: &mut Context<Self>) {
        let Some((id, input)) = self.renaming.take() else {
            return;
        };
        self._rename_enter = None;
        let name = input.read(cx).value().trim().to_string();
        let unchanged = self
            .items
            .iter()
            .any(|a| a.automation_id == id && a.name == name);
        if name.is_empty() || unchanged {
            cx.notify();
            return;
        }
        self.busy.insert(id.clone());
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.rename_automation(&job_id, &name).await },
            move |this, result, cx| {
                this.busy.remove(&id);
                match result {
                    Ok(resp) => {
                        if let Some(updated) = resp.automation
                            && let Some(row) = this.items.iter_mut().find(|a| a.automation_id == id)
                        {
                            *row = updated;
                        }
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_row(&self, ix: usize, a: &RebornAutomationInfo, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id = a.automation_id.clone();
        let busy = self.busy.contains(&id);
        let confirming = self.confirm_delete.as_deref() == Some(id.as_str());
        let paused = matches!(a.state, RebornAutomationState::Paused);
        let finished = matches!(a.state, RebornAutomationState::Completed);

        let mut facts: Vec<String> = vec![schedule_text(&a.source)];
        if let Some(next) = a.next_run_at.filter(|_| !paused) {
            facts.push(format!("Next {}", jobs::when(next)));
        }
        if let Some(last) = a.last_run_at {
            let status = match a.last_status {
                Some(RebornAutomationRunStatus::Ok) => " (ok)",
                Some(RebornAutomationRunStatus::Error) => " (failed)",
                None => "",
            };
            facts.push(format!("Last {}{status}", jobs::ago(last)));
        }

        let hold = a.active_hold.as_ref().map(|h| match h.reason {
            RebornAutomationHoldReason::Approval => "Waiting for your approval in its conversation",
            RebornAutomationHoldReason::Auth => "Waiting for a sign-in",
            RebornAutomationHoldReason::InProgress => "Previous run still in progress",
            RebornAutomationHoldReason::Other => "Held",
        });

        let runs = a.recent_runs.iter().enumerate().map(|(rix, run)| {
            let (icon, color) = match run.status {
                RebornAutomationRecentRunStatus::Ok => (IconName::CircleCheck, theme.success),
                RebornAutomationRecentRunStatus::Error => (IconName::CircleX, theme.danger),
                RebornAutomationRecentRunStatus::Running => (IconName::LoaderCircle, muted),
                RebornAutomationRecentRunStatus::Unknown => (IconName::Dash, muted),
            };
            let thread = run.thread_id.as_ref().map(|t| t.as_str().to_string());
            h_flex()
                .id(("run", ix * 100 + rix))
                .role(Role::Button)
                .aria_label(format!("Run {}", jobs::ago(run.submitted_at)))
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded(theme.radius)
                .text_xs()
                .text_color(muted)
                .child(div().text_color(color).child(Icon::new(icon).xsmall()))
                .child(jobs::ago(run.submitted_at))
                .when_some(thread, |this, thread| {
                    this.cursor_pointer()
                        .map(|d| crate::design::kit::smooth_bg(d, format!("run-{ix}-{rix}"), cx))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(PageEvent::OpenThread(thread.clone()))
                        }))
                })
        });

        let actions = h_flex()
            .gap_1()
            .child(
                Button::new(("run-now", ix))
                    .icon(IconName::Play)
                    .ghost()
                    .small()
                    .tooltip("Run now")
                    .accessibility_label("Run now")
                    .disabled(busy)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.act(id.clone(), AutomationAction::RunNow, cx)
                    })),
            )
            .when(!finished, |this| {
                let id = id.clone();
                // Resume is spelled out so it never reads as a second "Run now".
                let button = if paused {
                    Button::new(("pause-resume", ix))
                        .label("Resume")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.act(id.clone(), AutomationAction::Resume, cx)
                        }))
                } else {
                    Button::new(("pause-resume", ix))
                        .icon(IconName::Pause)
                        .ghost()
                        .small()
                        .tooltip("Pause schedule")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.act(id.clone(), AutomationAction::Pause, cx)
                        }))
                };
                this.child(button.disabled(busy))
            })
            .child(if confirming {
                let id = id.clone();
                Button::new(("confirm-delete", ix))
                    .label("Delete?")
                    .danger()
                    .small()
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act(id.clone(), AutomationAction::Delete, cx)
                    }))
            } else {
                let id = id.clone();
                Button::new(("delete", ix))
                    .icon(IconName::Delete)
                    .ghost()
                    .small()
                    .tooltip("Delete automation")
                    .accessibility_label("Delete automation")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_delete = Some(id.clone());
                        cx.notify();
                    }))
            });

        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(match self.renaming.as_ref().filter(|(rid, _)| *rid == id) {
                        Some((_, input)) => h_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(input).small()))
                            .child(
                                Button::new(("rename-save", ix))
                                    .label("Save")
                                    .primary()
                                    .xsmall()
                                    .on_click(cx.listener(|this, _, _, cx| this.save_rename(cx))),
                            )
                            .child(
                                Button::new(("rename-cancel", ix))
                                    .label("Cancel")
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.renaming = None;
                                        this._rename_enter = None;
                                        cx.notify();
                                    })),
                            )
                            .into_any_element(),
                        None => {
                            let (rid, name) = (id.clone(), a.name.clone());
                            div()
                                .id(("rename", ix))
                                .role(Role::Button)
                                .aria_label(format!("Rename {}", a.name))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_semibold()
                                .cursor_text()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_rename(rid.clone(), name.clone(), window, cx)
                                }))
                                .child(a.name.clone())
                                .into_any_element()
                        }
                    })
                    .child(state_tag(a.state))
                    .child(actions),
            )
            .child({
                let last_run = a
                    .recent_runs
                    .iter()
                    .find_map(|r| r.thread_id.as_ref().map(|t| t.as_str().to_string()));
                div()
                    .id(("facts", ix))
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .when_some(last_run, |d, thread| {
                        d.cursor_pointer()
                            .role(Role::Button)
                            .aria_label("Open the last run")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(PageEvent::OpenThread(thread.clone()))
                            }))
                    })
                    .child(facts.join("  ·  "))
            })
            .when_some(hold, |this, hold| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(Icon::new(IconName::TriangleAlert).xsmall())
                        .child(hold),
                )
            })
            .when(!a.recent_runs.is_empty(), |this| {
                this.child(h_flex().flex_wrap().gap_1().children(runs))
            })
            .into_any_element()
    }
}

fn state_tag(state: RebornAutomationState) -> Tag {
    match state {
        RebornAutomationState::Active | RebornAutomationState::Scheduled => {
            Tag::success().small().child("Active")
        }
        RebornAutomationState::Paused => Tag::warning().small().child("Paused"),
        RebornAutomationState::Completed => Tag::secondary().small().child("Done"),
        RebornAutomationState::Disabled => Tag::secondary().small().child("Disabled"),
        RebornAutomationState::Inactive => Tag::secondary().small().child("Inactive"),
        RebornAutomationState::Unknown => Tag::secondary().small().child("Unknown"),
    }
}

fn schedule_text(source: &RebornAutomationSource) -> String {
    match source {
        RebornAutomationSource::Schedule { cron, timezone } => {
            format!("Schedule {cron} ({timezone})")
        }
        RebornAutomationSource::Once { at, timezone } => match at.parse::<DateTime<Utc>>() {
            Ok(at) => format!("Once, {}", jobs::when(at)),
            Err(_) => format!("Once, {at} ({timezone})"),
        },
    }
}

impl Render for AutomationsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = h_flex()
            .gap_1()
            .child(
                Button::new("new-automation")
                    .icon(IconName::Plus)
                    .label("New automation")
                    .small()
                    .disabled(self.form.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(None, window, cx))),
            )
            .child(
                refresh_button("refresh-automations")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let rows: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, a)| self.render_row(ix, a, cx))
            .collect();
        let schedules: Vec<AnyElement> = self
            .schedules
            .iter()
            .enumerate()
            .map(|(ix, s)| self.render_schedule(ix, s, cx))
            .collect();
        let form = self.form.as_ref().map(|f| self.render_form(f, cx));
        let nothing = self.loaded && rows.is_empty() && schedules.is_empty();
        let section = |title: &'static str, cx: &App| {
            div()
                .px(px(8.))
                .pt_4()
                .pb_2()
                .text_sm()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child(title)
        };

        let body = v_flex()
            .child(header("Automations", actions))
            .when_some(self.error.as_ref(), |this, e| this.child(error_line(e, cx)))
            .when(!self.scheduler_enabled, |this| {
                this.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child("Scheduling is off on this gateway, so schedules will not fire. Run now still works."),
                )
            })
            .when_some(form, |this, form| this.child(div().pb_3().child(form)))
            .when(nothing, |this| {
                this.child(empty_state(
                    "No automations yet. Make one with New automation, or ask IronClaw in a conversation, for example: \"Every weekday at 9:00, summarize my inbox.\"",
                    cx,
                ))
            })
            .when(!schedules.is_empty(), |this| {
                this.child(section("Schedules and webhooks", cx))
                    .child(v_flex().gap_3().children(schedules))
            })
            .when(!rows.is_empty(), |this| {
                this.child(section("Routines (made in a chat)", cx))
                    .child(v_flex().gap_3().children(rows))
            });
        page_body("automations-page", body)
    }
}
