//! Automations: schedules (`t3_schedule` — clock or webhook, made here or
//! by an agent) with a create/edit form, and IronClaw's routines (cron and
//! one-time triggers made in a chat). Each row shows its next run and last
//! result; clicking it opens the thread of its last run. Port of GPUI
//! `pages/automations.rs`.

mod form;
mod schedule_rows;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rune::{Cursor, Cx, El, TextEdit, col, row, text};
use rune_kit::{Kit, TextInput};
use zlo_client::AutomationAction;
use zlo_client::product_wire::{
    RebornAutomationHoldReason, RebornAutomationInfo, RebornAutomationRecentRunStatus,
    RebornAutomationRunStatus, RebornAutomationSource, RebornAutomationState,
};
use zlo_client::schedules::{ScheduleInfo, ScheduleSecret};

use super::common::{
    Btn, SM, Tone, XS, actions, button, button_xs, card, empty_state, error_line, glyph, header,
    icon_button, label, notice_line, page_body, refresh_button, tag,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

pub struct AutomationsState {
    pub items: Vec<RebornAutomationInfo>,
    pub scheduler_enabled: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub busy: HashSet<String>,
    pub confirm_delete: Option<String>,
    /// The automation whose name is being edited, and its field.
    pub renaming: Option<String>,
    pub rename_edit: TextEdit,
    pub schedules: Vec<ScheduleInfo>,
    /// The open create/edit form.
    pub form: Option<form::Form>,
    pub confirm_delete_schedule: Option<String>,
    pub confirm_rotate: Option<String>,
    /// Webhook URLs and secrets the user revealed, by schedule id. Kept in
    /// memory only while shown; a refresh does not keep them.
    pub revealed: HashMap<String, ScheduleSecret>,
    pub show_curl: HashSet<String>,
    /// Takes an edit for a form field that closed in the same frame.
    spare_edit: TextEdit,
}

impl Default for AutomationsState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            scheduler_enabled: true,
            loaded: false,
            error: None,
            busy: HashSet::new(),
            confirm_delete: None,
            renaming: None,
            rename_edit: TextEdit::default(),
            schedules: Vec::new(),
            form: None,
            confirm_delete_schedule: None,
            confirm_rotate: None,
            revealed: HashMap::new(),
            show_curl: HashSet::new(),
            spare_edit: TextEdit::default(),
        }
    }
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

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    s.pages.automations.revealed.clear();
    jobs::run(
        s,
        "automations",
        |c| async move { c.list_schedules().await },
        |s, result, _| {
            let this = &mut s.pages.automations;
            match result {
                Ok(list) => this.schedules = list,
                // A gateway without the orchestrator lists routines only.
                Err(e) if e.is_not_found() => this.schedules.clear(),
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
    jobs::run(
        s,
        "automations",
        |c| async move { c.list_automations().await },
        |s, result, _| {
            let this = &mut s.pages.automations;
            match result {
                Ok(page) => {
                    this.items = page.automations;
                    this.scheduler_enabled = page.scheduler_enabled;
                }
                Err(e) => this.error = Some(e.to_string()),
            }
            this.loaded = true;
        },
    );
}

fn act(s: &mut State, id: String, action: AutomationAction) {
    let this = &mut s.pages.automations;
    if !this.busy.insert(id.clone()) {
        return;
    }
    this.confirm_delete = None;
    let job_id = id.clone();
    jobs::run(
        s,
        "automations",
        move |c| async move { c.automation_action(&job_id, action).await },
        move |s, result, _| {
            let this = &mut s.pages.automations;
            this.busy.remove(&id);
            match result {
                // Show the gateway's read-back, then re-list for run history.
                Ok(resp) => {
                    if let Some(updated) = resp.automation
                        && let Some(row) = this.items.iter_mut().find(|a| a.automation_id == id)
                    {
                        *row = updated;
                    }
                    refresh(s);
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn schedule_act(s: &mut State, id: String, action: ScheduleAction) {
    let key = schedule_key(&id);
    let this = &mut s.pages.automations;
    if !this.busy.insert(key.clone()) {
        return;
    }
    this.confirm_delete_schedule = None;
    this.confirm_rotate = None;
    jobs::run(
        s,
        "automations",
        move |c| async move {
            match action {
                ScheduleAction::RunNow => c.run_schedule(&id).await.map(|_| ()),
                ScheduleAction::Pause => c.set_schedule_enabled(&id, false).await.map(|_| ()),
                ScheduleAction::Resume => c.set_schedule_enabled(&id, true).await.map(|_| ()),
                ScheduleAction::Delete => c.delete_schedule(&id).await,
                ScheduleAction::RotateSecret => c.rotate_schedule_secret(&id).await.map(|_| ()),
            }
        },
        move |s, result, _| {
            let this = &mut s.pages.automations;
            this.busy.remove(&key);
            match result {
                Ok(()) => {
                    this.error = None;
                    refresh(s);
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn start_rename(s: &mut State, id: String, name: String) {
    let this = &mut s.pages.automations;
    this.rename_edit = TextEdit::new(name);
    this.rename_edit.focused = true;
    this.renaming = Some(id);
}

fn save_rename(s: &mut State) {
    let this = &mut s.pages.automations;
    let Some(id) = this.renaming.take() else {
        return;
    };
    let name = this.rename_edit.text.trim().to_string();
    let unchanged = this
        .items
        .iter()
        .any(|a| a.automation_id == id && a.name == name);
    if name.is_empty() || unchanged {
        return;
    }
    this.busy.insert(id.clone());
    let job_id = id.clone();
    jobs::run(
        s,
        "automations",
        move |c| async move { c.rename_automation(&job_id, &name).await },
        move |s, result, _| {
            let this = &mut s.pages.automations;
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
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn open_thread(thread: String) -> impl Fn(&mut State) + 'static {
    move |s: &mut State| crate::workspace::open_thread(s, thread.clone())
}

fn render_row(s: &State, k: &Kit, a: &RebornAutomationInfo) -> El {
    let p = &s.palette;
    let this = &s.pages.automations;
    let id = a.automation_id.clone();
    let busy = this.busy.contains(&id);
    let confirming = this.confirm_delete.as_deref() == Some(id.as_str());
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
            RebornAutomationRecentRunStatus::Ok => (&icons::CIRCLE_CHECK, p.success),
            RebornAutomationRecentRunStatus::Error => (&icons::CIRCLE_X, p.danger),
            RebornAutomationRecentRunStatus::Running => (&icons::LOADER_CIRCLE, p.text_muted),
            RebornAutomationRecentRunStatus::Unknown => (&icons::DASH, p.text_muted),
        };
        let mut chip = row()
            .key(format!("run-{id}-{rix}"))
            .role("button")
            .label(format!("Run {}", jobs::ago(run.submitted_at)))
            .gap(4.)
            .px(6.)
            .py(2.)
            .radius(crate::design::CONTROL_RADIUS)
            .center_y()
            .child(glyph(icon, 12., color))
            .child(label(jobs::ago(run.submitted_at), XS, 400., p.text_muted));
        if let Some(thread) = run.thread_id.as_ref().map(|t| t.as_str().to_string()) {
            chip = chip
                .cursor(Cursor::Pointer)
                .hover(p.hover)
                .on_press(open_thread(thread));
        }
        chip
    });

    let mut buttons = vec![
        icon_button(&icons::PLAY, "Run now")
            .disabled(busy)
            .on_press({
                let id = id.clone();
                move |s: &mut State| act(s, id.clone(), AutomationAction::RunNow)
            })
            .build(k),
    ];
    if !finished {
        let id = id.clone();
        // Resume is spelled out so it never reads as a second "Run now".
        buttons.push(if paused {
            button("Resume", Btn::Outline)
                .disabled(busy)
                .on_press(move |s: &mut State| act(s, id.clone(), AutomationAction::Resume))
                .build(k)
        } else {
            icon_button(&icons::PAUSE, "Pause schedule")
                .disabled(busy)
                .on_press(move |s: &mut State| act(s, id.clone(), AutomationAction::Pause))
                .build(k)
        });
    }
    buttons.push({
        let id = id.clone();
        if confirming {
            button("Delete?", Btn::Danger)
                .disabled(busy)
                .on_press(move |s: &mut State| act(s, id.clone(), AutomationAction::Delete))
                .build(k)
        } else {
            icon_button(&icons::DELETE, "Delete automation")
                .disabled(busy)
                .on_press(move |s: &mut State| {
                    s.pages.automations.confirm_delete = Some(id.clone())
                })
                .build(k)
        }
    });

    let name = match this.renaming.as_ref().filter(|rid| **rid == id) {
        Some(_) => row()
            .grow(1.0)
            .min_w(0.)
            .gap(4.)
            .center_y()
            .child(
                col().grow(1.0).min_w(0.).child(
                    TextInput::new(&this.rename_edit)
                        .width(None)
                        .on_edit(|s: &mut State| &mut s.pages.automations.rename_edit)
                        .on_submit(|s: &mut State, _| save_rename(s))
                        .build(k),
                ),
            )
            .child(
                button_xs("Save", Btn::Primary)
                    .on_press(save_rename)
                    .build(k),
            )
            .child(
                button_xs("Cancel", Btn::Ghost)
                    .on_press(|s: &mut State| s.pages.automations.renaming = None)
                    .build(k),
            ),
        None => {
            let (rid, name) = (id.clone(), a.name.clone());
            row()
                .key(format!("rename-{id}"))
                .role("button")
                .label(format!("Rename {}", a.name))
                .grow(1.0)
                .min_w(0.)
                .cursor(Cursor::Text)
                .on_press(move |s: &mut State| start_rename(s, rid.clone(), name.clone()))
                .child(label(a.name.clone(), crate::pages::common::BASE, 600., p.text).truncate())
        }
    };

    let last_run = a
        .recent_runs
        .iter()
        .find_map(|r| r.thread_id.as_ref().map(|t| t.as_str().to_string()));
    let mut facts_el = col().key(format!("facts-{id}")).w_full().child(
        text(facts.join("  ·  "))
            .font(super::common::font(XS, 400.))
            .color(p.text_muted)
            .wrap(),
    );
    if let Some(thread) = last_run {
        facts_el = facts_el
            .role("button")
            .label("Open the last run")
            .cursor(Cursor::Pointer)
            .on_press(open_thread(thread));
    }

    let mut c = card(p)
        .key(format!("automation-{id}"))
        .child(
            row()
                .gap(8.)
                .center_y()
                .child(name)
                .child(state_tag(p, a.state))
                .child(actions(buttons)),
        )
        .child(facts_el);
    if let Some(hold) = hold {
        c = c.child(
            row()
                .gap(6.)
                .center_y()
                .child(glyph(&icons::TRIANGLE_ALERT, 12., p.warning))
                .child(label(hold, XS, 400., p.warning)),
        );
    }
    if !a.recent_runs.is_empty() {
        c = c.child(row().flex_wrap().gap(4.).children(runs));
    }
    c
}

fn state_tag(p: &crate::design::Palette, state: RebornAutomationState) -> El {
    match state {
        RebornAutomationState::Active | RebornAutomationState::Scheduled => {
            tag(p, "Active", Tone::Success)
        }
        RebornAutomationState::Paused => tag(p, "Paused", Tone::Warning),
        RebornAutomationState::Completed => tag(p, "Done", Tone::Secondary),
        RebornAutomationState::Disabled => tag(p, "Disabled", Tone::Secondary),
        RebornAutomationState::Inactive => tag(p, "Inactive", Tone::Secondary),
        RebornAutomationState::Unknown => tag(p, "Unknown", Tone::Secondary),
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

pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let this = &s.pages.automations;
    let head_actions = actions([
        button("New automation", Btn::Secondary)
            .icon(&icons::PLUS)
            .disabled(this.form.is_some())
            .on_press(|s: &mut State| form::open_form(s, None))
            .build(k),
        refresh_button(k, refresh),
    ]);
    let rows: Vec<El> = this.items.iter().map(|a| render_row(s, k, a)).collect();
    let schedules: Vec<El> = this
        .schedules
        .iter()
        .map(|sch| schedule_rows::render_schedule(s, k, sch))
        .collect();
    let nothing = this.loaded && rows.is_empty() && schedules.is_empty();
    let section = |title: &str| {
        col()
            .px(8.)
            .pt(16.)
            .pb(8.)
            .child(label(title, SM, 600., p.text_muted))
    };

    let mut body = col().w_full().child(header(p, "Automations", head_actions));
    if let Some(e) = &this.error {
        body = body.child(error_line(p, e));
    }
    if !this.scheduler_enabled {
        body = body.child(notice_line(
            "Scheduling is off on this gateway, so schedules will not fire. Run now still works.",
            p.warning,
        ));
    }
    if let Some(f) = &this.form {
        body = body.child(
            col()
                .w_full()
                .pb(12.)
                .child(form::render_form(s, cx, k, f, layer)),
        );
    }
    if nothing {
        body = body.child(empty_state(
            p,
            "No automations yet. Make one with New automation, or ask IronClaw in a conversation, for example: \"Every weekday at 9:00, summarize my inbox.\"",
        ));
    }
    if !schedules.is_empty() {
        body = body
            .child(section("Schedules and webhooks"))
            .child(col().w_full().gap(12.).children(schedules));
    }
    if !rows.is_empty() {
        body = body
            .child(section("Routines (made in a chat)"))
            .child(col().w_full().gap(12.).children(rows));
    }
    page_body("automations-page", body)
}

/// Fixture state: two schedules (clock and webhook) and two routines.
pub fn fixture(s: &mut State) {
    let now = Utc::now();
    let ms = |d: chrono::Duration| (now + d).timestamp_millis();
    let schedules = serde_json::from_value::<Vec<ScheduleInfo>>(serde_json::json!([
        {"id": "sch_1", "title": "Morning inbox digest", "prompt": "Summarize my inbox.",
         "schedule": {"type": "fixed_time", "timeOfDay": "09:00", "weekdays": [1,2,3,4,5]},
         "describe": "Weekdays at 09:00", "enabled": true, "threadId": "t1",
         "bindToCurrentThread": true, "nextRunAt": ms(chrono::Duration::hours(14)),
         "lastRunAt": ms(-chrono::Duration::hours(10)), "lastError": null,
         "profileId": "default", "model": "anthropic/claude-sonnet-4-5"},
        {"id": "sch_2", "title": "Deploy hook", "prompt": "Check the deploy: {{body}}",
         "schedule": {"type": "webhook"}, "describe": "On webhook", "enabled": false,
         "threadId": "t2", "bindToCurrentThread": false, "nextRunAt": null, "lastRunAt": null,
         "lastError": null, "webhookUrlMasked": "http://127.0.0.1:3000/hooks/••••a91c",
         "signature": {"header": "x-zlo-signature", "prefix": "sha256=", "encoding": "hex", "hasSecret": true},
         "lastDelivery": {"status": "accepted", "at": ms(-chrono::Duration::minutes(42))}}
    ]));
    let items = serde_json::from_value::<Vec<RebornAutomationInfo>>(serde_json::json!([
        {"automation_id": "a1", "name": "Weekly dependency check",
         "source": {"type": "schedule", "cron": "0 8 * * MON", "timezone": "Europe/Zurich"},
         "state": "active", "next_run_at": now + chrono::Duration::days(2),
         "last_run_at": now - chrono::Duration::days(5), "last_status": "ok",
         "recent_runs": [
            {"thread_id": "t1", "status": "ok", "submitted_at": now - chrono::Duration::days(5)},
            {"status": "error", "submitted_at": now - chrono::Duration::days(12)}
         ],
         "active_hold": {"reason": "approval"}},
        {"automation_id": "a2", "name": "Remind me about the review",
         "source": {"type": "once", "at": (now + chrono::Duration::hours(3)).to_rfc3339(), "timezone": "UTC"},
         "state": "paused", "recent_runs": []}
    ]));
    let this = &mut s.pages.automations;
    match (schedules, items) {
        (Ok(sch), Ok(items)) => {
            this.schedules = sch;
            this.items = items;
        }
        (Err(e), _) | (_, Err(e)) => tracing::warn!(error = %e, "automations fixture"),
    }
    this.loaded = true;
}

/// Fixture state with the create form open.
pub fn fixture_form(s: &mut State) {
    fixture(s);
    form::open_form(s, None);
    if let Some(f) = s.pages.automations.form.as_mut() {
        f.fixture();
    }
}
