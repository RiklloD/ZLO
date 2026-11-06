//! The create/edit form for schedules (`t3_schedule` rows): name, prompt,
//! profile and model, when it runs (friendly presets or raw cron, with the
//! next three runs previewed by the backend), time zone, where results go,
//! and webhook signing. Creating makes a thread on the chosen profile and
//! model, then binds the schedule to it — the same steps as a new chat.

use chrono::{DateTime, Local, Utc};
use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::profiles::ProfileListResponse;
use ironclaw_gpui_client::schedules::{
    DraftField, Preset, ScheduleDraft, ScheduleInfo, SchedulePreview, ScheduleSpec, ScheduleWrite,
};

use super::AutomationsView;
use crate::chat::model_picker::{ModelPicker, PickerEvent};
use crate::design::kit;
use crate::jobs;
use crate::ui::*;

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const FORM_LABEL_WIDTH: f32 = 120.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Clock,
    Webhook,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// The backend's own zone (this Mac's).
    Local,
    Utc,
    Other,
}

#[derive(Default)]
struct Errors {
    prompt: Option<SharedString>,
    schedule: Option<(DraftField, SharedString)>,
    save: Option<SharedString>,
}

pub(super) struct Form {
    /// `None`: a new schedule.
    pub editing: Option<ScheduleInfo>,
    name: Entity<TextareaState>,
    prompt: Entity<TextareaState>,
    time: Entity<InputState>,
    hours: Entity<InputState>,
    cron: Entity<InputState>,
    zone_input: Entity<InputState>,
    kind: Kind,
    preset: Preset,
    days: Vec<u8>,
    zone: Zone,
    /// `(profile_id, display name)`; empty until loaded.
    profiles: Vec<(String, String)>,
    profile: String,
    picker: Option<Entity<ModelPicker>>,
    /// Each run starts a fresh thread instead of continuing one.
    fresh_thread: bool,
    signed: bool,
    enabled: bool,
    preview: Option<Result<SchedulePreview, SharedString>>,
    /// Drops a preview answer older than the latest edit.
    preview_gen: u64,
    errors: Errors,
    pub saving: bool,
    _subs: Vec<Subscription>,
}

/// "Mon 3 Nov, 09:00" in this Mac's zone.
fn local_time(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|at| {
            at.with_timezone(&Local)
                .format("%a %-d %b, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "?".into())
}

impl Form {
    pub fn new(
        editing: Option<ScheduleInfo>,
        window: &mut Window,
        cx: &mut Context<AutomationsView>,
    ) -> Self {
        let spec = editing.as_ref().and_then(|s| s.spec());
        let draft = spec
            .as_ref()
            .and_then(ScheduleDraft::from_spec)
            .unwrap_or_default();
        let kind = match spec {
            Some(ScheduleSpec::Webhook) => Kind::Webhook,
            _ => Kind::Clock,
        };
        let zone = match draft.timezone.as_deref() {
            None => Zone::Local,
            Some("UTC") => Zone::Utc,
            Some(_) => Zone::Other,
        };
        let area = |cx: &mut Context<AutomationsView>,
                    window: &mut Window,
                    placeholder: &'static str,
                    rows: usize,
                    value: String| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, rows)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let line = |cx: &mut Context<AutomationsView>,
                    window: &mut Window,
                    placeholder: &'static str,
                    value: String| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let e = editing.as_ref();
        let name = area(
            cx,
            window,
            "Name, e.g. Morning inbox digest",
            1,
            e.map(|s| s.title.clone()).unwrap_or_default(),
        );
        let prompt = area(
            cx,
            window,
            "What should it do each time? Webhooks can quote the request: {{body}}, {{body.field}}, {{headers.name}}, {{query.name}}",
            12,
            e.map(|s| s.prompt.clone()).unwrap_or_default(),
        );
        let time = line(cx, window, "09:00", draft.time.clone());
        let hours = line(cx, window, "4", draft.hours.clone());
        let cron = line(cx, window, "0 9 * * MON-FRI", draft.cron.clone());
        let zone_input = line(
            cx,
            window,
            "Europe/Zurich",
            draft
                .timezone
                .clone()
                .filter(|z| z != "UTC")
                .unwrap_or_default(),
        );
        let mut subs = Vec::new();
        for state in [&time, &hours, &cron, &zone_input] {
            subs.push(cx.subscribe_in(state, window, |this, _, event, _, cx| {
                if let InputEvent::Change = event {
                    this.form_changed(cx);
                }
            }));
        }
        subs.push(cx.subscribe_in(&prompt, window, |this, _, event, _, cx| {
            if let InputEvent::Change = event
                && let Some(form) = this.form.as_mut()
            {
                form.errors.prompt = None;
                cx.notify();
            }
        }));
        let form = Self {
            editing: editing.clone(),
            name,
            prompt,
            time,
            hours,
            cron,
            zone_input,
            kind,
            preset: draft.preset,
            days: draft.days.clone(),
            zone,
            profiles: Vec::new(),
            profile: e
                .and_then(|s| s.profile_id.clone())
                .unwrap_or_else(|| ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID.into()),
            picker: None,
            fresh_thread: e.is_some_and(|s| !s.bind_to_current_thread),
            signed: e.is_none_or(|s| s.signature.as_ref().is_some_and(|sig| sig.has_secret)),
            enabled: e.is_none_or(|s| s.enabled),
            preview: None,
            preview_gen: 0,
            errors: Errors::default(),
            saving: false,
            _subs: subs,
        };
        form.name.update(cx, |s, cx| s.focus(window, cx));
        form
    }

    fn draft(&self, cx: &App) -> ScheduleDraft {
        let value = |s: &Entity<InputState>| s.read(cx).value().trim().to_string();
        ScheduleDraft {
            preset: self.preset,
            time: value(&self.time),
            days: self.days.clone(),
            hours: value(&self.hours),
            cron: value(&self.cron),
            timezone: match self.zone {
                Zone::Local => None,
                Zone::Utc => Some("UTC".into()),
                Zone::Other => Some(value(&self.zone_input)),
            },
        }
    }

    /// The spec the form describes, or the field that is wrong.
    fn spec(&self, cx: &App) -> Result<ScheduleSpec, (DraftField, String)> {
        match self.kind {
            Kind::Webhook => Ok(ScheduleSpec::Webhook),
            Kind::Clock => self.draft(cx).to_spec().map_err(|e| (e.field, e.message)),
        }
    }

    fn write(&self, spec: ScheduleSpec, cx: &App) -> ScheduleWrite {
        let webhook = spec.is_webhook();
        let was_signed = self
            .editing
            .as_ref()
            .and_then(|s| s.signature.as_ref())
            .is_some_and(|s| s.has_secret);
        ScheduleWrite {
            title: self.name.read(cx).value().trim().to_string(),
            prompt: self.prompt.read(cx).value().trim().to_string(),
            schedule: spec,
            bind_to_current_thread: !self.fresh_thread,
            enabled: self.enabled,
            // Editing keeps the stored secret unless signing was switched.
            signed: match (webhook, self.editing.is_some(), self.signed == was_signed) {
                (false, _, _) => None,
                (true, true, true) => None,
                (true, _, _) => Some(self.signed),
            },
        }
    }
}

impl AutomationsView {
    /// Open the form: a new schedule (`None`) or an existing one.
    pub(super) fn open_form(
        &mut self,
        editing: Option<ScheduleInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let creating = editing.is_none();
        self.form = Some(Form::new(editing, window, cx));
        if creating {
            self.load_form_choices(window, cx);
        }
        self.form_changed(cx);
        cx.notify();
    }

    pub(super) fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    /// Profiles, and a model picker read through any thread (the catalog
    /// is per user).
    fn load_form_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        jobs::run_in(
            &self.backend,
            window,
            cx,
            |c| async move {
                let profiles = match c.profiles().await {
                    Ok(list) => list,
                    // A gateway without profiles: the default profile only.
                    Err(e) if e.is_not_found() => ProfileListResponse {
                        profiles: Vec::new(),
                        last_used_profile_id: None,
                    },
                    Err(e) => return Err(e),
                };
                let threads = c.list_threads(1, None).await?;
                Ok((
                    profiles,
                    threads.threads.first().map(|t| t.thread_id.clone()),
                ))
            },
            |this, result, window, cx| {
                let backend = this.backend.clone();
                let Some(form) = this.form.as_mut() else {
                    return;
                };
                match result {
                    Ok((list, catalog)) => {
                        form.profiles = list
                            .profiles
                            .iter()
                            .map(|p| {
                                let name = if p.settings.display_name.trim().is_empty() {
                                    p.profile_id.clone()
                                } else {
                                    p.settings.display_name.clone()
                                };
                                (p.profile_id.clone(), name)
                            })
                            .collect();
                        if let Some(last) = list.last_used_profile_id
                            && form.profiles.iter().any(|(id, _)| *id == last)
                        {
                            form.profile = last;
                        }
                        let picker =
                            cx.new(|cx| ModelPicker::new(backend, None, catalog, window, cx));
                        let profile = form.profile.clone();
                        picker.update(cx, |p, cx| p.set_profile(profile, cx));
                        form._subs.push(cx.subscribe(
                            &picker,
                            |this, _, event: &PickerEvent, cx| {
                                if let (PickerEvent::Failed(message), Some(form)) =
                                    (event, this.form.as_mut())
                                {
                                    form.errors.save = Some(message.clone().into());
                                    cx.notify();
                                }
                            },
                        ));
                        form.picker = Some(picker);
                    }
                    Err(e) => form.errors.save = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// Re-check the schedule and ask the backend for the next runs.
    pub(super) fn form_changed(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.preview_gen += 1;
        let generation = form.preview_gen;
        form.errors.schedule = None;
        let spec = match form.spec(cx) {
            Ok(spec) => spec,
            Err((field, message)) => {
                form.errors.schedule = Some((field, message.into()));
                form.preview = None;
                cx.notify();
                return;
            }
        };
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.preview_schedule(&spec, None).await },
            move |this, result, cx| {
                let Some(form) = this.form.as_mut().filter(|f| f.preview_gen == generation) else {
                    return;
                };
                form.preview = Some(result.map_err(|e| e.to_string().into()));
                cx.notify();
            },
        );
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.saving {
            return;
        }
        let spec = match form.spec(cx) {
            Ok(spec) => spec,
            Err((field, message)) => {
                form.errors.schedule = Some((field, message.into()));
                cx.notify();
                return;
            }
        };
        if let Some(Err(message)) = &form.preview {
            form.errors.save = Some(message.clone());
            cx.notify();
            return;
        }
        let write = form.write(spec, cx);
        if write.prompt.is_empty() {
            form.errors.prompt = Some("Say what it should do.".into());
            cx.notify();
            return;
        }
        form.errors.save = None;
        form.saving = true;
        let editing = form.editing.as_ref().map(|s| s.id.clone());
        let profile = form.profile.clone();
        let (pick, effort) = form
            .picker
            .as_ref()
            .map(|p| (p.read(cx).pending(), p.read(cx).pending_effort()))
            .unwrap_or_default();
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                // Validate the prompt first, so a refusal leaves no thread behind.
                c.preview_schedule(&write.schedule, Some(&write.prompt))
                    .await?;
                if let Some(id) = editing {
                    return c.update_schedule(&id, &write).await;
                }
                let thread = new_home_thread(&c, &profile, pick, effort).await?;
                c.create_schedule(&thread, &write).await
            },
            |this, result: Result<ScheduleInfo, ClientError>, cx| {
                match result {
                    Ok(_) => {
                        this.form = None;
                        this.refresh(cx);
                    }
                    Err(e) => {
                        if let Some(form) = this.form.as_mut() {
                            form.saving = false;
                            form.errors.save = Some(e.to_string().into());
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    pub(super) fn render_form(&self, form: &Form, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let editing = form.editing.is_some();
        let error = |field: DraftField| {
            form.errors
                .schedule
                .as_ref()
                .filter(|(f, _)| *f == field)
                .map(|(_, m)| m.clone())
        };
        let field_error = move |message: Option<SharedString>| {
            message.map(|m| div().text_xs().text_color(danger).child(m))
        };
        let row = |label: &'static str, control: AnyElement| {
            h_flex()
                .w_full()
                .gap_3()
                .items_start()
                .child(
                    div()
                        .w(px(FORM_LABEL_WIDTH))
                        .flex_none()
                        .pt_1()
                        .text_sm()
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(control))
                .into_any_element()
        };
        let choice = |id: (&'static str, usize), label: &'static str, on: bool| {
            Button::new(id)
                .label(label)
                .small()
                .when(on, |b| b.primary())
                .when(!on, |b| b.ghost())
        };

        // Profile and model (new schedules: the run thread is made on them).
        let who = if editing {
            let e = form.editing.as_ref();
            let model = e
                .and_then(|s| s.model.clone())
                .map(|m| crate::chat::model_picker::pretty_model(&m))
                .unwrap_or_else(|| "profile default".into());
            let profile = e
                .and_then(|s| s.profile_id.clone())
                .unwrap_or_else(|| "default".into());
            div()
                .text_sm()
                .text_color(muted)
                .whitespace_normal()
                .child(format!(
                    "{profile} · {model} (set when the automation was made; open its thread to switch model)"
                ))
                .into_any_element()
        } else {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(form.profiles.iter().enumerate().map(|(ix, (id, name))| {
                            let id = id.clone();
                            Button::new(("profile", ix))
                                .label(name.clone())
                                .small()
                                .when(form.profile == id, |b| b.primary())
                                .when(form.profile != id, |b| b.ghost())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(form) = this.form.as_mut() {
                                        form.profile = id.clone();
                                        if let Some(picker) = form.picker.clone() {
                                            let id = id.clone();
                                            picker.update(cx, |p, cx| p.set_profile(id, cx));
                                        }
                                        cx.notify();
                                    }
                                }))
                        })),
                )
                .when_some(form.picker.clone(), |d, picker| {
                    d.child(div().relative().child(picker))
                })
                .into_any_element()
        };

        let kind = kit::segmented(
            "automation-kind",
            &["Clock", "Webhook"],
            usize::from(form.kind == Kind::Webhook),
            editing,
            {
                let weak = cx.entity().downgrade();
                move |ix, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(form) = this.form.as_mut() {
                            form.kind = if ix == 1 { Kind::Webhook } else { Kind::Clock };
                        }
                        this.form_changed(cx);
                    });
                }
            },
            cx,
        );

        let schedule = (form.kind == Kind::Clock).then(|| {
            let presets = h_flex().flex_wrap().gap_1().children(
                Preset::ALL.iter().enumerate().map(|(ix, preset)| {
                    let preset = *preset;
                    choice(("preset", ix), preset.label(), form.preset == preset).on_click(
                        cx.listener(move |this, _, _, cx| {
                            if let Some(form) = this.form.as_mut() {
                                form.preset = preset;
                            }
                            this.form_changed(cx);
                        }),
                    )
                }),
            );
            let detail = match form.preset {
                Preset::Daily | Preset::Weekdays | Preset::Weekly => h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(muted).child("at"))
                    .child(div().w(px(80.)).child(Input::new(&form.time).small()))
                    .when(form.preset == Preset::Weekly, |d| {
                        d.child(h_flex().gap_0p5().children(DAY_NAMES.iter().enumerate().map(
                            |(day, name)| {
                                let day = day as u8;
                                let on = form.days.contains(&day);
                                choice(("day", day as usize), name, on).xsmall().on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        if let Some(form) = this.form.as_mut() {
                                            if let Some(at) =
                                                form.days.iter().position(|d| *d == day)
                                            {
                                                form.days.remove(at);
                                            } else {
                                                form.days.push(day);
                                            }
                                        }
                                        this.form_changed(cx);
                                    }),
                                )
                            },
                        )))
                    })
                    .into_any_element(),
                Preset::EveryHours => h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(muted).child("every"))
                    .child(div().w(px(80.)).child(Input::new(&form.hours).small()))
                    .child(div().text_sm().text_color(muted).child("hours"))
                    .into_any_element(),
                Preset::Cron => v_flex()
                    .gap_1()
                    .child(Input::new(&form.cron).small())
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("minute hour day-of-month month day-of-week, e.g. 30 8 * * MON-FRI (use day names: numbers count from 1 = Sunday)"),
                    )
                    .into_any_element(),
            };
            let error = error(DraftField::Time)
                .or_else(|| error(DraftField::Days))
                .or_else(|| error(DraftField::Hours))
                .or_else(|| error(DraftField::Cron));
            let preview = match &form.preview {
                None => None,
                Some(Ok(p)) if p.next_runs.is_empty() => {
                    Some(div().text_xs().text_color(muted).child(p.describe.clone()))
                }
                Some(Ok(p)) => Some(div().text_xs().text_color(muted).whitespace_normal().child(
                    format!(
                        "{} — next: {}",
                        p.describe,
                        p.next_runs
                            .iter()
                            .map(|ms| local_time(*ms))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
                Some(Err(e)) => Some(div().text_xs().text_color(danger).child(e.clone())),
            };
            v_flex()
                .gap_2()
                .child(presets)
                .child(detail)
                .when_some(field_error(error), |d, e| d.child(e))
                .when_some(preview, |d, p| d.child(p))
                .into_any_element()
        });

        let zone = (form.kind == Kind::Clock && form.preset != Preset::EveryHours).then(|| {
            let zones = [
                (Zone::Local, "This Mac"),
                (Zone::Utc, "UTC"),
                (Zone::Other, "Other"),
            ];
            h_flex()
                .gap_1()
                .items_center()
                .children(zones.iter().enumerate().map(|(ix, (zone, label))| {
                    let zone = *zone;
                    choice(("zone", ix), label, form.zone == zone).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(form) = this.form.as_mut() {
                                form.zone = zone;
                            }
                            this.form_changed(cx);
                        },
                    ))
                }))
                .when(form.zone == Zone::Other, |d| {
                    d.child(
                        div()
                            .w(px(200.))
                            .child(Input::new(&form.zone_input).small()),
                    )
                })
                .into_any_element()
        });

        let switch = |id: &'static str, on: bool, set: fn(&mut Form, bool)| {
            Switch::new(id)
                .checked(on)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    if let Some(form) = this.form.as_mut() {
                        set(form, *checked);
                    }
                    cx.notify();
                }))
        };
        let delivery = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        choice(("delivery", 0), "Same thread each run", !form.fresh_thread)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = this.form.as_mut() {
                                    form.fresh_thread = false;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        choice(("delivery", 1), "New thread each run", form.fresh_thread)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = this.form.as_mut() {
                                    form.fresh_thread = true;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(div().text_xs().text_color(muted).whitespace_normal().child(
                "Each run's reply lands in that thread, listed in your inbox. One thread keeps earlier runs as context; a new thread each run starts clean.",
            ))
            .into_any_element();

        let save_label = match (editing, form.saving) {
            (_, true) => "Saving…",
            (true, false) => "Save",
            (false, false) => "Create automation",
        };
        card(cx)
            .gap_3()
            .child(
                div()
                    .font_semibold()
                    .child(if editing { "Edit automation" } else { "New automation" }),
            )
            .child(row("Name", Textarea::new(&form.name).into_any_element()))
            .child(row(
                "Prompt",
                v_flex()
                    .gap_1()
                    .child(Textarea::new(&form.prompt))
                    .when_some(field_error(form.errors.prompt.clone()), |d, e| d.child(e))
                    .into_any_element(),
            ))
            .child(row("Runs on", who))
            .child(row("Trigger", kind))
            .when_some(schedule, |d, s| d.child(row("When", s)))
            .when_some(zone, |d, z| d.child(row("Time zone", z)))
            .when(form.kind == Kind::Webhook, |d| {
                d.child(row(
                    "Signing",
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(switch("automation-signed", form.signed, |f, on| f.signed = on))
                        .child(div().text_sm().text_color(muted).whitespace_normal().child(
                            "Require an HMAC-SHA256 signature (x-zlo-signature). The secret is made for you; reveal it after saving.",
                        ))
                        .into_any_element(),
                ))
            })
            .child(row("Results", delivery))
            .child(row(
                "Enabled",
                switch("automation-enabled", form.enabled, |f, on| f.enabled = on)
                    .into_any_element(),
            ))
            .when_some(form.errors.save.clone(), |d, e| {
                d.child(div().text_sm().text_color(danger).whitespace_normal().child(e))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("automation-cancel")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
                    )
                    .child(
                        Button::new("automation-save")
                            .label(save_label)
                            .primary()
                            .small()
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                    ),
            )
            .into_any_element()
    }
}

/// A thread on `profile` (and the picked model) for a new schedule's runs.
async fn new_home_thread(
    c: &ironclaw_gpui_client::Client,
    profile: &str,
    pick: Option<(String, String)>,
    effort: Option<String>,
) -> Result<String, ClientError> {
    let id = match c.create_profile_thread(profile).await {
        Ok(created) => created.thread_id,
        Err(e) if e.is_not_found() => c.create_thread().await?.thread.thread_id,
        Err(e) => return Err(e),
    };
    let mut home = id.clone();
    match (pick, effort) {
        (Some((provider, model)), effort) => {
            let change = c
                .set_thread_route(&id, &provider, &model, effort.as_deref())
                .await?;
            if let Some(fork) = change.fork_thread_id {
                home = fork;
            }
        }
        (None, Some(effort)) => {
            c.set_thread_effort(&id, &effort).await?;
        }
        (None, None) => {}
    }
    Ok(home)
}
