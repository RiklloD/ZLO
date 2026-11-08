//! Schedule rows (`t3_schedule`, agent- or page-made): when it runs next,
//! how the last run went, run now / pause / edit / delete, and for webhooks
//! the URL and signing secret — masked until the user reveals them — with
//! rotation, the last request's status and a signed `curl` example. Port of
//! GPUI `pages/automations/schedule_rows.rs`.

use chrono::{DateTime, Utc};
use zlo_client::schedules::{LastOutcome, ScheduleInfo, ScheduleSecret, curl_example};
use rune::{Cursor, El, col, el, row, text};
use rune_kit::Kit;

use super::{ScheduleAction, form, schedule_act, schedule_key};
use crate::icons;
use crate::jobs;
use crate::pages::common::{
    BASE, Btn, Tone, XS, actions, block_fill, button, button_xs, card, font, glyph, icon_button,
    label, tag,
};
use crate::state::State;

const MASK: &str = "••••••••••••••••";

fn at(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

#[derive(Clone, Copy)]
enum CopyWhat {
    Url,
    Secret,
    Curl,
}

pub(super) fn render_schedule(st: &State, k: &Kit, s: &ScheduleInfo) -> El {
    let p = &st.palette;
    let this = &st.pages.automations;
    let id = s.id.clone();
    let busy = this.busy.contains(&schedule_key(&id));
    let confirming = this.confirm_delete_schedule.as_deref() == Some(id.as_str());

    let mut facts: Vec<String> = vec![if s.describe.is_empty() {
        s.schedule.to_string()
    } else {
        s.describe.clone()
    }];
    if let Some(next) = s.next_run_at.filter(|_| s.enabled).and_then(at) {
        facts.push(format!("Next {}", jobs::when(next)));
    }
    // The row knows when a run was queued, not how its turn ended:
    // that is in the run's thread (one click away).
    facts.push(match s.last_outcome() {
        LastOutcome::Never => "Never run".into(),
        LastOutcome::Queued(ms) => match at(ms) {
            Some(last) => format!("Last queued {}", jobs::ago(last)),
            None => "Last queued".into(),
        },
        LastOutcome::Failed(_) => "Last attempt failed".into(),
    });
    let profile = s.profile_id.clone().unwrap_or_else(|| "default".into());
    facts.push(match &s.model {
        Some(model) => format!(
            "{profile} · {}",
            crate::chat::model_picker::pretty_model(model)
        ),
        None => profile,
    });
    if !s.bind_to_current_thread {
        facts.push("New thread each run".into());
    }
    // The thread of the last run, else the schedule's own thread.
    let open = s
        .last_run_thread_id
        .clone()
        .unwrap_or_else(|| s.thread_id.clone());

    let run = {
        let id = id.clone();
        icon_button(&icons::PLAY, "Run now")
            .disabled(busy)
            .on_press(move |s: &mut State| schedule_act(s, id.clone(), ScheduleAction::RunNow))
            .build(k)
    };
    let pause = {
        let id = id.clone();
        if s.enabled {
            icon_button(&icons::PAUSE, "Pause")
                .disabled(busy)
                .on_press(move |s: &mut State| schedule_act(s, id.clone(), ScheduleAction::Pause))
                .build(k)
        } else {
            button("Resume", Btn::Outline)
                .disabled(busy)
                .on_press(move |s: &mut State| schedule_act(s, id.clone(), ScheduleAction::Resume))
                .build(k)
        }
    };
    let edit = {
        let row = s.clone();
        button("Edit", Btn::Ghost)
            .disabled(busy)
            .on_press(move |s: &mut State| form::open_form(s, Some(row.clone())))
            .build(k)
    };
    let delete = {
        let id = id.clone();
        if confirming {
            button("Delete?", Btn::Danger)
                .disabled(busy)
                .on_press(move |s: &mut State| schedule_act(s, id.clone(), ScheduleAction::Delete))
                .build(k)
        } else {
            icon_button(&icons::DELETE, "Delete automation")
                .disabled(busy)
                .on_press(move |s: &mut State| {
                    s.pages.automations.confirm_delete_schedule = Some(id.clone())
                })
                .build(k)
        }
    };

    let mut title_row = row().gap(8.).center_y().child(
        row()
            .key(format!("s-open-{id}"))
            .role("button")
            .label(format!("Open the last run of {}", s.title))
            .grow(1.0)
            .min_w(0.)
            .cursor(Cursor::Pointer)
            .on_press(super::open_thread(open.clone()))
            .child(label(s.title.clone(), BASE, 600., p.text).truncate()),
    );
    if s.is_webhook() {
        title_row = title_row.child(tag(p, "Webhook", Tone::Secondary));
    }
    title_row = title_row
        .child(if s.enabled {
            tag(p, "Active", Tone::Success)
        } else {
            tag(p, "Paused", Tone::Warning)
        })
        .child(actions([run, pause, edit, delete]));

    let mut c = card(p)
        .key(format!("schedule-{id}"))
        .child(title_row)
        .child(
            col()
                .key(format!("s-facts-{id}"))
                .w_full()
                .cursor(Cursor::Pointer)
                .on_press(super::open_thread(open))
                .child(
                    text(facts.join("  ·  "))
                        .font(font(XS, 400.))
                        .color(p.text_muted)
                        .wrap(),
                ),
        );
    if let Some(e) = &s.last_error {
        c = c.child(
            row()
                .gap(6.)
                .center_y()
                .child(glyph(&icons::CIRCLE_ALERT, 12., p.danger))
                .child(label(e.clone(), XS, 400., p.danger)),
        );
    }
    if s.is_webhook() {
        c = c.child(render_webhook(st, k, s));
    }
    c
}

fn render_webhook(st: &State, k: &Kit, s: &ScheduleInfo) -> El {
    let p = &st.palette;
    let this = &st.pages.automations;
    let id = s.id.clone();
    let revealed: Option<&ScheduleSecret> = this.revealed.get(&id);
    let signed = s.signature.as_ref().is_some_and(|sig| sig.has_secret);
    let url = revealed
        .and_then(|r| r.webhook_url.clone())
        .or_else(|| s.webhook_url_masked.clone())
        .unwrap_or_else(|| "URL appears once the gateway is listening".into());
    let secret_text = revealed
        .and_then(|r| r.signature.as_ref())
        .map(|sig| sig.secret.clone())
        .unwrap_or_else(|| MASK.into());
    let busy = this.busy.contains(&schedule_key(&id));
    let rotating = this.confirm_rotate.as_deref() == Some(id.as_str());

    let shown = revealed.is_some();
    let reveal_button = || {
        let id = id.clone();
        icon_button(
            if shown { &icons::EYE_OFF } else { &icons::EYE },
            if shown { "Hide" } else { "Reveal" },
        )
        .on_press(move |s: &mut State| toggle_reveal(s, id.clone()))
        .build(k)
    };
    let copy_button = |what: CopyWhat| {
        let id = id.clone();
        icon_button(&icons::COPY, "Copy")
            .on_press(move |s: &mut State| copy_secret(s, id.clone(), what))
            .build(k)
    };
    let line = |title: &str, value: String, extra: El| {
        row()
            .key(format!("hook-{title}"))
            .gap(8.)
            .center_y()
            .child(col().w(64.).child(label(title, XS, 400., p.text_muted)))
            .child(
                text(value)
                    .font(font(XS, 400.).mono())
                    .color(p.text)
                    .truncate()
                    .grow(1.0)
                    .min_w(0.),
            )
            .child(extra)
    };

    let mut d = col().w_full().gap(4.).pt(4.).child(line(
        "URL",
        url,
        row()
            .child(reveal_button())
            .child(copy_button(CopyWhat::Url)),
    ));
    if signed {
        let rotate = {
            let id = id.clone();
            if rotating {
                button_xs("Rotate? Old secret stops working", Btn::Danger)
                    .disabled(busy)
                    .on_press(move |s: &mut State| {
                        schedule_act(s, id.clone(), ScheduleAction::RotateSecret)
                    })
                    .build(k)
            } else {
                icon_button(&icons::REFRESH_CW, "Rotate secret")
                    .disabled(busy)
                    .on_press(move |s: &mut State| {
                        s.pages.automations.confirm_rotate = Some(id.clone())
                    })
                    .build(k)
            }
        };
        d = d.child(line(
            "Secret",
            secret_text,
            row()
                .child(reveal_button())
                .child(copy_button(CopyWhat::Secret))
                .child(rotate),
        ));
    } else {
        d = d.child(label(
            "Unsigned: anyone with the URL can run it. Edit to require a signature.",
            XS,
            400.,
            p.warning,
        ));
    }
    if let Some(delivery) = &s.last_delivery {
        let when = delivery.at.and_then(at).map(jobs::ago).unwrap_or_default();
        let color = if delivery.status.ok() {
            p.success
        } else {
            p.danger
        };
        d = d.child(
            row()
                .gap(6.)
                .child(label("Last request", XS, 400., p.text_muted))
                .child(label(delivery.status.label(), XS, 400., color))
                .child(label(when, XS, 400., p.text_muted)),
        );
    }
    let curl_open = this.show_curl.contains(&id);
    let mut curl_row = row().gap(4.).child({
        let id = id.clone();
        button_xs(
            if curl_open {
                "Hide curl example"
            } else {
                "curl example"
            },
            Btn::Ghost,
        )
        .on_press(move |s: &mut State| {
            let open = &mut s.pages.automations.show_curl;
            if !open.remove(&id) {
                open.insert(id.clone());
            }
        })
        .build(k)
    });
    if curl_open {
        curl_row = curl_row.child(copy_button(CopyWhat::Curl));
    }
    d = d.child(curl_row);
    if curl_open {
        let curl = curl_example(
            revealed.and_then(|r| r.webhook_url.as_deref()),
            revealed.and_then(|r| r.signature.as_ref()),
            signed,
        );
        d = d.child(
            el().w_full()
                .padding(8.)
                .radius(crate::design::CONTROL_RADIUS)
                .fill(block_fill(p))
                .child(
                    text(curl)
                        .font(font(XS, 400.).mono())
                        .color(p.text)
                        .wrap_anywhere(),
                ),
        );
        if revealed.is_none() && signed {
            d = d.child(label(
                "Reveal to fill in the URL and secret, or export ZLO_WEBHOOK_URL and ZLO_WEBHOOK_SECRET.",
                XS,
                400.,
                p.text_muted,
            ));
        }
    }
    d
}

/// Reveal (fetching URL and secret) or hide them again.
fn toggle_reveal(s: &mut State, id: String) {
    if s.pages.automations.revealed.remove(&id).is_some() {
        return;
    }
    let key = id.clone();
    jobs::run(
        s,
        "automations",
        move |c| async move { c.reveal_schedule_secret(&key).await },
        move |s, result, _| {
            let this = &mut s.pages.automations;
            match result {
                Ok(secret) => {
                    this.revealed.insert(id, secret);
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

/// Copy without leaving the secret on screen: fetched on this click.
fn copy_secret(s: &mut State, id: String, what: CopyWhat) {
    let signed = s
        .pages
        .automations
        .schedules
        .iter()
        .find(|sch| sch.id == id)
        .and_then(|sch| sch.signature.as_ref())
        .is_some_and(|sig| sig.has_secret);
    jobs::run(
        s,
        "automations",
        move |c| async move { c.reveal_schedule_secret(&id).await },
        move |s, result, tx| match result {
            Ok(secret) => {
                let url = secret.webhook_url.clone().unwrap_or_default();
                let text = match what {
                    CopyWhat::Url => url,
                    CopyWhat::Secret => secret
                        .signature
                        .as_ref()
                        .map(|sig| sig.secret.clone())
                        .unwrap_or_default(),
                    CopyWhat::Curl => curl_example(Some(&url), secret.signature.as_ref(), signed),
                };
                tx.copy(text);
            }
            Err(e) => s.pages.automations.error = Some(e.to_string()),
        },
    );
}
