//! The create/edit form for schedules (`t3_schedule` rows): name, prompt,
//! profile and model, when it runs (friendly presets or raw cron, with the
//! next three runs previewed by the backend), time zone, where results go,
//! and webhook signing. Creating makes a thread on the chosen profile and
//! model, then binds the schedule to it — the same steps as a new chat.
//! Port of GPUI `pages/automations/form.rs`.

use chrono::{DateTime, Local, Utc};
use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::profiles::ProfileListResponse;
use ironclaw_gpui_client::schedules::{
    DraftField, Preset, ScheduleDraft, ScheduleInfo, SchedulePreview, ScheduleSpec, ScheduleWrite,
};
use rune::{El, TextEdit, col, row};
use rune_kit::{Kit, TextArea, TextInput};

use crate::chat::model_picker::{self, FixedSlot, ModelPickerState, PickerEvent, Slot};
use crate::jobs::{self, Generation};
use crate::pages::common::{
    BASE, Btn, SM, XS, button, button_xs, card, choice, label, loading, para, segmented, switch,
};
use crate::state::State;

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const FORM_LABEL_WIDTH: f32 = 120.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
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
    prompt: Option<String>,
    schedule: Option<(DraftField, String)>,
    save: Option<String>,
}

pub struct Form {
    /// `None`: a new schedule.
    pub editing: Option<ScheduleInfo>,
    name: TextEdit,
    prompt: TextEdit,
    time: TextEdit,
    hours: TextEdit,
    cron: TextEdit,
    zone_input: TextEdit,
    kind: Kind,
    preset: Preset,
    days: Vec<u8>,
    zone: Zone,
    /// `(profile_id, display name)`; empty until loaded.
    profiles: Vec<(String, String)>,
    profile: String,
    picker: Option<ModelPickerState>,
    /// Each run starts a fresh thread instead of continuing one.
    fresh_thread: bool,
    signed: bool,
    enabled: bool,
    preview: Option<Result<SchedulePreview, String>>,
    /// Drops a preview answer older than the latest edit.
    preview_gen: Generation,
    errors: Errors,
    pub saving: bool,
}

/// Where the form's model picker lives, and where its events go: a failed
/// pick shows as the form's save error (GPUI `PickerEvent::Failed`).
const FORM_PICKER: Slot = Slot::Fixed(FixedSlot {
    get: |s| s.pages.automations.form.as_ref()?.picker.as_ref(),
    get_mut: |s| s.pages.automations.form.as_mut()?.picker.as_mut(),
    event: |s, event| {
        if let (PickerEvent::Failed(message), Some(form)) =
            (event, s.pages.automations.form.as_mut())
        {
            form.errors.save = Some(message);
        }
    },
});

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
    pub fn new(editing: Option<ScheduleInfo>) -> Self {
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
        let e = editing.as_ref();
        let mut name = TextEdit::new(e.map(|s| s.title.clone()).unwrap_or_default());
        name.focused = true;
        Self {
            editing: editing.clone(),
            name,
            prompt: TextEdit::new(e.map(|s| s.prompt.clone()).unwrap_or_default()),
            time: TextEdit::new(draft.time.clone()),
            hours: TextEdit::new(draft.hours.clone()),
            cron: TextEdit::new(draft.cron.clone()),
            zone_input: TextEdit::new(
                draft
                    .timezone
                    .clone()
                    .filter(|z| z != "UTC")
                    .unwrap_or_default(),
            ),
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
            preview_gen: Generation::default(),
            errors: Errors::default(),
            saving: false,
        }
    }

    fn draft(&self) -> ScheduleDraft {
        let value = |e: &TextEdit| e.text.trim().to_string();
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
    fn spec(&self) -> Result<ScheduleSpec, (DraftField, String)> {
        match self.kind {
            Kind::Webhook => Ok(ScheduleSpec::Webhook),
            Kind::Clock => self.draft().to_spec().map_err(|e| (e.field, e.message)),
        }
    }

    fn write(&self, spec: ScheduleSpec) -> ScheduleWrite {
        let webhook = spec.is_webhook();
        let was_signed = self
            .editing
            .as_ref()
            .and_then(|s| s.signature.as_ref())
            .is_some_and(|s| s.has_secret);
        ScheduleWrite {
            title: self.name.text.trim().to_string(),
            prompt: self.prompt.text.trim().to_string(),
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

    /// Fixture values for the `automation-form` scene.
    pub fn fixture(&mut self) {
        self.name.focused = false;
        self.name.replace_text("Morning inbox digest");
        self.prompt
            .replace_text("Summarize my inbox: what needs an answer today, what can wait.");
        self.profiles = vec![
            ("default".into(), "Default".into()),
            ("work".into(), "Work".into()),
        ];
        self.picker = Some(ModelPickerState::new(None, None));
        self.preview = Some(Ok(SchedulePreview {
            schedule: serde_json::Value::Null,
            describe: "Weekdays at 09:00".into(),
            next_runs: Vec::new(),
        }));
    }
}

/// Open the form: a new schedule (`None`) or an existing one.
pub fn open_form(s: &mut State, editing: Option<ScheduleInfo>) {
    let creating = editing.is_none();
    s.pages.automations.form = Some(Form::new(editing));
    if creating {
        load_form_choices(s);
    }
    form_changed(s);
}

fn close_form(s: &mut State) {
    s.pages.automations.form = None;
}

/// Profiles, and a model picker read through any thread (the catalog is
/// per user).
fn load_form_choices(s: &mut State) {
    jobs::run(
        s,
        "automations",
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
        |s, result, _| {
            let Some(form) = s.pages.automations.form.as_mut() else {
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
                    form.picker = Some(ModelPickerState::new(None, catalog));
                    let profile = form.profile.clone();
                    model_picker::set_profile(s, FORM_PICKER, profile);
                }
                Err(e) => form.errors.save = Some(e.to_string()),
            }
        },
    );
}

/// Re-check the schedule and ask the backend for the next runs.
fn form_changed(s: &mut State) {
    let Some(form) = s.pages.automations.form.as_mut() else {
        return;
    };
    let generation = form.preview_gen.bump();
    form.errors.schedule = None;
    let spec = match form.spec() {
        Ok(spec) => spec,
        Err((field, message)) => {
            form.errors.schedule = Some((field, message));
            form.preview = None;
            return;
        }
    };
    jobs::run(
        s,
        "automations",
        move |c| async move { c.preview_schedule(&spec, None).await },
        move |s, result, _| {
            let Some(form) = s
                .pages
                .automations
                .form
                .as_mut()
                .filter(|f| f.preview_gen == generation)
            else {
                return;
            };
            form.preview = Some(result.map_err(|e| e.to_string()));
        },
    );
}

fn save_form(s: &mut State) {
    let Some(form) = s.pages.automations.form.as_mut() else {
        return;
    };
    if form.saving {
        return;
    }
    let spec = match form.spec() {
        Ok(spec) => spec,
        Err((field, message)) => {
            form.errors.schedule = Some((field, message));
            return;
        }
    };
    if let Some(Err(message)) = &form.preview {
        form.errors.save = Some(message.clone());
        return;
    }
    let write = form.write(spec);
    if write.prompt.is_empty() {
        form.errors.prompt = Some("Say what it should do.".into());
        return;
    }
    form.errors.save = None;
    form.saving = true;
    let editing = form.editing.as_ref().map(|s| s.id.clone());
    let profile = form.profile.clone();
    let (pick, effort) = form
        .picker
        .as_ref()
        .map(|p| (p.pending(), p.pending_effort()))
        .unwrap_or_default();
    jobs::run(
        s,
        "automations",
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
        |s, result: Result<ScheduleInfo, ClientError>, _| match result {
            Ok(_) => {
                s.pages.automations.form = None;
                super::refresh(s);
            }
            Err(e) => {
                if let Some(form) = s.pages.automations.form.as_mut() {
                    form.saving = false;
                    form.errors.save = Some(e.to_string());
                }
            }
        },
    );
}

/// Edit the open form, then re-check its schedule.
fn edit_form(s: &mut State, edit: impl FnOnce(&mut Form)) {
    if let Some(form) = s.pages.automations.form.as_mut() {
        edit(form);
    }
    form_changed(s);
}

pub fn render_form(
    s: &State,
    cx: &mut rune::Cx<State>,
    k: &Kit,
    form: &Form,
    layer: &mut Vec<El>,
) -> El {
    let p = &s.palette;
    let (muted, danger) = (p.text_muted, p.danger);
    let editing = form.editing.is_some();
    let error = |field: DraftField| {
        form.errors
            .schedule
            .as_ref()
            .filter(|(f, _)| *f == field)
            .map(|(_, m)| m.clone())
    };
    let field_error = |message: Option<String>| message.map(|m| label(m, XS, 400., danger));
    let frow = |title: &str, control: El| {
        row()
            .key(format!("form-{title}"))
            .w_full()
            .gap(12.)
            .items_start()
            .child(
                col()
                    .w(FORM_LABEL_WIDTH)
                    .pt(4.)
                    .child(label(title, SM, 400., muted)),
            )
            .child(col().grow(1.0).min_w(0.).child(control))
    };
    let input = |edit: &TextEdit, placeholder: &str, at: fn(&mut State) -> &mut TextEdit| {
        TextInput::new(edit)
            .placeholder(placeholder)
            .width(None)
            .on_edit(at)
            .on_change(|s: &mut State, _| form_changed(s))
            .build(k)
    };

    // Profile and model (new schedules: the run thread is made on them).
    let who = if editing {
        let e = form.editing.as_ref();
        let model = e
            .and_then(|s| s.model.clone())
            .map(|m| model_picker::pretty_model(&m))
            .unwrap_or_else(|| "profile default".into());
        let profile = e
            .and_then(|s| s.profile_id.clone())
            .unwrap_or_else(|| "default".into());
        para(
            format!(
                "{profile} · {model} (set when the automation was made; open its thread to switch model)"
            ),
            SM,
            muted,
        )
    } else {
        let chips = row()
            .flex_wrap()
            .gap(4.)
            .children(form.profiles.iter().map(|(id, name)| {
                let id = id.clone();
                choice(name, form.profile == id)
                    .on_press(move |s: &mut State| {
                        if let Some(form) = s.pages.automations.form.as_mut() {
                            form.profile = id.clone();
                            model_picker::set_profile(s, FORM_PICKER, id.clone());
                        }
                    })
                    .build(k)
            }));
        let mut d = col().gap(4.).child(chips);
        if form.picker.is_some() {
            d = d.child(model_picker::pills_for(s, FORM_PICKER, cx, k, layer));
        }
        d
    };

    let kind = segmented(
        k,
        &["Clock", "Webhook"],
        usize::from(form.kind == Kind::Webhook),
        editing,
        |s: &mut State, ix, _| {
            edit_form(s, |f| {
                f.kind = if ix == 1 { Kind::Webhook } else { Kind::Clock }
            })
        },
    );

    let schedule = (form.kind == Kind::Clock).then(|| {
        let presets = row().flex_wrap().gap(4.).children(Preset::ALL.iter().map(|preset| {
            let preset = *preset;
            choice(preset.label(), form.preset == preset)
                .on_press(move |s: &mut State| edit_form(s, |f| f.preset = preset))
                .build(k)
        }));
        let detail = match form.preset {
            Preset::Daily | Preset::Weekdays | Preset::Weekly => {
                let mut d = row()
                    .gap(8.)
                    .center_y()
                    .child(label("at", SM, 400., muted))
                    .child(col().w(80.).child(input(&form.time, "09:00", |s| {
                        field(s, |f| &mut f.time)
                    })));
                if form.preset == Preset::Weekly {
                    d = d.child(row().gap(2.).children(DAY_NAMES.iter().enumerate().map(
                        |(day, name)| {
                            let day = day as u8;
                            choice(name, form.days.contains(&day))
                                .control_size(rune_kit::ControlSize::Small)
                                .on_press(move |s: &mut State| {
                                    edit_form(s, |f| {
                                        if let Some(at) = f.days.iter().position(|d| *d == day) {
                                            f.days.remove(at);
                                        } else {
                                            f.days.push(day);
                                        }
                                    })
                                })
                                .build(k)
                        },
                    )));
                }
                d
            }
            Preset::EveryHours => row()
                .gap(8.)
                .center_y()
                .child(label("every", SM, 400., muted))
                .child(col().w(80.).child(input(&form.hours, "4", |s| {
                    field(s, |f| &mut f.hours)
                })))
                .child(label("hours", SM, 400., muted)),
            Preset::Cron => col()
                .gap(4.)
                .child(input(&form.cron, "0 9 * * MON-FRI", |s| {
                    field(s, |f| &mut f.cron)
                }))
                .child(para(
                    "minute hour day-of-month month day-of-week, e.g. 30 8 * * MON-FRI (use day names: numbers count from 1 = Sunday)",
                    XS,
                    muted,
                )),
        };
        let err = error(DraftField::Time)
            .or_else(|| error(DraftField::Days))
            .or_else(|| error(DraftField::Hours))
            .or_else(|| error(DraftField::Cron));
        let preview = match &form.preview {
            None => None,
            Some(Ok(pv)) if pv.next_runs.is_empty() => Some(label(pv.describe.clone(), XS, 400., muted)),
            Some(Ok(pv)) => Some(para(
                format!(
                    "{} — next: {}",
                    pv.describe,
                    pv.next_runs
                        .iter()
                        .map(|ms| local_time(*ms))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                XS,
                muted,
            )),
            Some(Err(e)) => Some(label(e.clone(), XS, 400., danger)),
        };
        col()
            .gap(8.)
            .child(presets)
            .child(detail)
            .children(field_error(err))
            .children(preview)
    });

    let zone = (form.kind == Kind::Clock && form.preset != Preset::EveryHours).then(|| {
        let zones = [
            (Zone::Local, "This Mac"),
            (Zone::Utc, "UTC"),
            (Zone::Other, "Other"),
        ];
        let mut d = row()
            .gap(4.)
            .center_y()
            .children(zones.iter().map(|(zone, title)| {
                let zone = *zone;
                choice(title, form.zone == zone)
                    .on_press(move |s: &mut State| edit_form(s, |f| f.zone = zone))
                    .build(k)
            }));
        if form.zone == Zone::Other {
            d = d.child(
                col()
                    .w(200.)
                    .child(input(&form.zone_input, "Europe/Zurich", |s| {
                        field(s, |f| &mut f.zone_input)
                    })),
            );
        }
        d
    });

    let toggle = |name: &str, on: bool, set: fn(&mut Form, bool)| {
        switch(k, on, false, name, move |s: &mut State| {
            if let Some(form) = s.pages.automations.form.as_mut() {
                set(form, !on);
            }
        })
    };
    let delivery = col()
        .gap(4.)
        .child(
            row()
                .gap(4.)
                .child(
                    choice("Same thread each run", !form.fresh_thread)
                        .on_press(|s: &mut State| {
                            if let Some(form) = s.pages.automations.form.as_mut() {
                                form.fresh_thread = false;
                            }
                        })
                        .build(k),
                )
                .child(
                    choice("New thread each run", form.fresh_thread)
                        .on_press(|s: &mut State| {
                            if let Some(form) = s.pages.automations.form.as_mut() {
                                form.fresh_thread = true;
                            }
                        })
                        .build(k),
                ),
        )
        .child(para(
            "Each run's reply lands in that thread, listed in your inbox. One thread keeps earlier runs as context; a new thread each run starts clean.",
            XS,
            muted,
        ));

    let save_label = match (editing, form.saving) {
        (_, true) => "Saving…",
        (true, false) => "Save",
        (false, false) => "Create automation",
    };
    let name = TextArea::new(&form.name)
        .placeholder("Name, e.g. Morning inbox digest")
        .auto_grow(1, 1)
        .width(None)
        .on_edit(|s: &mut State| field(s, |f| &mut f.name))
        .build(k);
    let prompt = TextArea::new(&form.prompt)
        .placeholder("What should it do each time? Webhooks can quote the request: {{body}}, {{body.field}}, {{headers.name}}, {{query.name}}")
        .auto_grow(1, 12)
        .width(None)
        .on_edit(|s: &mut State| field(s, |f| &mut f.prompt))
        .on_change(|s: &mut State, _| {
            if let Some(form) = s.pages.automations.form.as_mut() {
                form.errors.prompt = None;
            }
        })
        .build(k);

    let mut c = card(p)
        .key("automation-form")
        .gap(12.)
        .child(label(
            if editing {
                "Edit automation"
            } else {
                "New automation"
            },
            BASE,
            600.,
            p.text,
        ))
        .child(frow("Name", name))
        .child(frow(
            "Prompt",
            col()
                .gap(4.)
                .child(prompt)
                .children(field_error(form.errors.prompt.clone())),
        ))
        .child(frow("Runs on", who))
        .child(frow("Trigger", kind));
    if let Some(sch) = schedule {
        c = c.child(frow("When", sch));
    }
    if let Some(z) = zone {
        c = c.child(frow("Time zone", z));
    }
    if form.kind == Kind::Webhook {
        c = c.child(frow(
            "Signing",
            row()
                .gap(8.)
                .center_y()
                .child(toggle("Require a signature", form.signed, |f, on| f.signed = on))
                .child(para(
                    "Require an HMAC-SHA256 signature (x-zlo-signature). The secret is made for you; reveal it after saving.",
                    SM,
                    muted,
                )),
        ));
    }
    c = c.child(frow("Results", delivery)).child(frow(
        "Enabled",
        row().child(toggle("Enabled", form.enabled, |f, on| f.enabled = on)),
    ));
    if let Some(e) = &form.errors.save {
        c = c.child(para(e.clone(), SM, danger));
    }
    c.child(
        row()
            .w_full()
            .justify_end()
            .gap(8.)
            .child(
                button_xs("Cancel", Btn::Ghost)
                    .on_press(close_form)
                    .build(k),
            )
            .child(
                loading(button(save_label, Btn::Primary), false)
                    .disabled(form.saving)
                    .on_press(save_form)
                    .build(k),
            ),
    )
}

/// A text field of the open form, for `on_edit`. An edit that lands after
/// the form closed goes to a spare field nobody shows.
fn field(s: &mut State, pick: fn(&mut Form) -> &mut TextEdit) -> &mut TextEdit {
    let super::AutomationsState {
        form, spare_edit, ..
    } = &mut s.pages.automations;
    match form.as_mut() {
        Some(form) => pick(form),
        None => spare_edit,
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
