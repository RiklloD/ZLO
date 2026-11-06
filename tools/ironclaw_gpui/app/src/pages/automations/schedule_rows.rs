//! Schedule rows (`t3_schedule`, agent- or page-made): when it runs next,
//! how the last run went, run now / pause / edit / delete, and for webhooks
//! the URL and signing secret — masked until the user reveals them — with
//! rotation, the last request's status and a signed `curl` example.

use chrono::{DateTime, Utc};
use ironclaw_gpui_client::schedules::{LastOutcome, ScheduleInfo, ScheduleSecret, curl_example};

use super::{AutomationsView, ScheduleAction};
use crate::jobs;
use crate::ui::*;

const MASK: &str = "••••••••••••••••";

fn at(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

impl AutomationsView {
    pub(super) fn render_schedule(
        &self,
        ix: usize,
        s: &ScheduleInfo,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id = s.id.clone();
        let busy = self.busy.contains(&super::schedule_key(&id));
        let confirming = self.confirm_delete_schedule.as_deref() == Some(id.as_str());

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

        let actions = h_flex()
            .gap_1()
            .child(
                Button::new(("s-run", ix))
                    .icon(IconName::Play)
                    .ghost()
                    .small()
                    .tooltip("Run now")
                    .accessibility_label("Run now")
                    .disabled(busy)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.schedule_act(id.clone(), ScheduleAction::RunNow, cx)
                        }
                    })),
            )
            .child({
                let id = id.clone();
                let (action, button) = if s.enabled {
                    (
                        ScheduleAction::Pause,
                        Button::new(("s-pause", ix))
                            .icon(IconName::Pause)
                            .ghost()
                            .small()
                            .tooltip("Pause"),
                    )
                } else {
                    (
                        ScheduleAction::Resume,
                        Button::new(("s-pause", ix))
                            .label("Resume")
                            .outline()
                            .small(),
                    )
                };
                button.disabled(busy).on_click(
                    cx.listener(move |this, _, _, cx| this.schedule_act(id.clone(), action, cx)),
                )
            })
            .child({
                let row = s.clone();
                Button::new(("s-edit", ix))
                    .label("Edit")
                    .ghost()
                    .small()
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_form(Some(row.clone()), window, cx)
                    }))
            })
            .child(if confirming {
                let id = id.clone();
                Button::new(("s-confirm-delete", ix))
                    .label("Delete?")
                    .danger()
                    .small()
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.schedule_act(id.clone(), ScheduleAction::Delete, cx)
                    }))
            } else {
                let id = id.clone();
                Button::new(("s-delete", ix))
                    .icon(IconName::Delete)
                    .ghost()
                    .small()
                    .tooltip("Delete automation")
                    .accessibility_label("Delete automation")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_delete_schedule = Some(id.clone());
                        cx.notify();
                    }))
            });

        let tag = if s.enabled {
            Tag::success().small().child("Active")
        } else {
            Tag::warning().small().child("Paused")
        };
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .id(("s-open", ix))
                            .role(Role::Button)
                            .aria_label(format!("Open the last run of {}", s.title))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let open = open.clone();
                                move |_, _, _, cx| {
                                    cx.emit(super::PageEvent::OpenThread(open.clone()))
                                }
                            }))
                            .child(s.title.clone()),
                    )
                    .when(s.is_webhook(), |d| {
                        d.child(Tag::secondary().small().child("Webhook"))
                    })
                    .child(tag)
                    .child(actions),
            )
            .child(
                div()
                    .id(("s-facts", ix))
                    .text_xs()
                    .text_color(muted)
                    .whitespace_normal()
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(super::PageEvent::OpenThread(open.clone()))
                    }))
                    .child(facts.join("  ·  ")),
            )
            .when_some(s.last_error.clone(), |d, e| {
                d.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(Icon::new(IconName::CircleAlert).xsmall())
                        .child(e),
                )
            })
            .when(s.is_webhook(), |d| d.child(self.render_webhook(ix, s, cx)))
            .into_any_element()
    }

    fn render_webhook(&self, ix: usize, s: &ScheduleInfo, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id = s.id.clone();
        let revealed: Option<&ScheduleSecret> = self.revealed.get(&id);
        let signed = s.signature.as_ref().is_some_and(|sig| sig.has_secret);
        let url = revealed
            .and_then(|r| r.webhook_url.clone())
            .or_else(|| s.webhook_url_masked.clone())
            .unwrap_or_else(|| "URL appears once the gateway is listening".into());
        let secret_text = revealed
            .and_then(|r| r.signature.as_ref())
            .map(|sig| sig.secret.clone())
            .unwrap_or_else(|| MASK.into());
        let busy = self.busy.contains(&super::schedule_key(&id));
        let rotating = self.confirm_rotate.as_deref() == Some(id.as_str());
        let mono = theme.mono_font_family.clone();

        let reveal_button = |key: &'static str| {
            let id = id.clone();
            let shown = revealed.is_some();
            Button::new((key, ix))
                .icon(if shown {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .ghost()
                .xsmall()
                .tooltip(if shown { "Hide" } else { "Reveal" })
                .accessibility_label(if shown { "Hide" } else { "Reveal" })
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_reveal(id.clone(), cx)))
        };
        let copy_button = |key: &'static str, what: CopyWhat| {
            let id = id.clone();
            Button::new((key, ix))
                .icon(IconName::Copy)
                .ghost()
                .xsmall()
                .tooltip("Copy")
                .accessibility_label("Copy")
                .on_click(cx.listener(move |this, _, _, cx| this.copy_secret(id.clone(), what, cx)))
        };
        let line = |label: &'static str, value: String, extra: AnyElement| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(64.))
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_family(mono.clone())
                        .child(value),
                )
                .child(extra)
        };

        let delivery = s.last_delivery.as_ref().map(|d| {
            let when = d.at.and_then(at).map(jobs::ago).unwrap_or_default();
            let color = if d.status.ok() {
                theme.success
            } else {
                theme.danger
            };
            h_flex()
                .gap_1p5()
                .text_xs()
                .child(div().text_color(muted).child("Last request"))
                .child(div().text_color(color).child(d.status.label()))
                .child(div().text_color(muted).child(when))
        });
        let curl_open = self.show_curl.contains(&id);
        let curl = curl_example(
            revealed.and_then(|r| r.webhook_url.as_deref()),
            revealed.and_then(|r| r.signature.as_ref()),
            signed,
        );

        v_flex()
            .gap_1()
            .pt_1()
            .child(line(
                "URL",
                url,
                h_flex()
                    .child(reveal_button("hook-reveal-url"))
                    .child(copy_button("hook-copy-url", CopyWhat::Url))
                    .into_any_element(),
            ))
            .when(signed, |d| {
                let rotate = if rotating {
                    let id = id.clone();
                    Button::new(("hook-rotate-confirm", ix))
                        .label("Rotate? Old secret stops working")
                        .danger()
                        .xsmall()
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.schedule_act(id.clone(), ScheduleAction::RotateSecret, cx)
                        }))
                } else {
                    let id = id.clone();
                    Button::new(("hook-rotate", ix))
                        .icon(IconName::RefreshCw)
                        .ghost()
                        .xsmall()
                        .tooltip("Rotate secret")
                        .accessibility_label("Rotate secret")
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_rotate = Some(id.clone());
                            cx.notify();
                        }))
                };
                d.child(line(
                    "Secret",
                    secret_text,
                    h_flex()
                        .child(reveal_button("hook-reveal-secret"))
                        .child(copy_button("hook-copy-secret", CopyWhat::Secret))
                        .child(rotate)
                        .into_any_element(),
                ))
            })
            .when(!signed, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("Unsigned: anyone with the URL can run it. Edit to require a signature."),
                )
            })
            .when_some(delivery, |d, line| d.child(line))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(("hook-curl", ix))
                            .label(if curl_open {
                                "Hide curl example"
                            } else {
                                "curl example"
                            })
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    if !this.show_curl.remove(&id) {
                                        this.show_curl.insert(id.clone());
                                    }
                                    cx.notify();
                                }
                            })),
                    )
                    .when(curl_open, |d| {
                        d.child(copy_button("hook-copy-curl", CopyWhat::Curl))
                    }),
            )
            .when(curl_open, |d| {
                d.child(
                    div()
                        .p_2()
                        .rounded(theme.radius)
                        .bg(super::super::block_fill(cx))
                        .text_xs()
                        .font_family(mono.clone())
                        .whitespace_normal()
                        .child(curl),
                )
                .when(revealed.is_none() && signed, |d| {
                    d.child(div().text_xs().text_color(muted).child(
                        "Reveal to fill in the URL and secret, or export ZLO_WEBHOOK_URL and ZLO_WEBHOOK_SECRET.",
                    ))
                })
            })
            .into_any_element()
    }

    /// Reveal (fetching URL and secret) or hide them again.
    fn toggle_reveal(&mut self, id: String, cx: &mut Context<Self>) {
        if self.revealed.remove(&id).is_some() {
            cx.notify();
            return;
        }
        let key = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.reveal_schedule_secret(&key).await },
            move |this, result, cx| {
                match result {
                    Ok(secret) => {
                        this.revealed.insert(id, secret);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// Copy without leaving the secret on screen: fetched on this click.
    fn copy_secret(&mut self, id: String, what: CopyWhat, cx: &mut Context<Self>) {
        let signed = self
            .schedules
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.signature.as_ref())
            .is_some_and(|s| s.has_secret);
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.reveal_schedule_secret(&id).await },
            move |this, result, cx| {
                match result {
                    Ok(secret) => {
                        let url = secret.webhook_url.clone().unwrap_or_default();
                        let text = match what {
                            CopyWhat::Url => url,
                            CopyWhat::Secret => secret
                                .signature
                                .as_ref()
                                .map(|s| s.secret.clone())
                                .unwrap_or_default(),
                            CopyWhat::Curl => {
                                curl_example(Some(&url), secret.signature.as_ref(), signed)
                            }
                        };
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }
}

#[derive(Clone, Copy)]
enum CopyWhat {
    Url,
    Secret,
    Curl,
}
