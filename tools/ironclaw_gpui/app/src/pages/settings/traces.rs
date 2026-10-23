//! Trace Commons: credit earned for contributed redacted traces, traces held
//! for privacy review, and a one-time link into the Trace Commons account.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::{RebornAccountTrace, RebornTraceCreditsResponse};

use crate::jobs;
use crate::pages::{empty_state, error_line};
use crate::ui::*;

pub struct TracePanel {
    backend: Backend,
    credits: Option<RebornTraceCreditsResponse>,
    traces: Option<(bool, Vec<RebornAccountTrace>)>,
    credits_error: Option<SharedString>,
    traces_error: Option<SharedString>,
    authorizing: Option<String>,
    opening: bool,
    notice: Option<SharedString>,
}

impl TracePanel {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            credits: None,
            traces: None,
            credits_error: None,
            traces_error: None,
            authorizing: None,
            opening: false,
            notice: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.trace_credits().await },
            |this, result, cx| {
                match result {
                    Ok(credits) => {
                        this.credits = Some(credits);
                        this.credits_error = None;
                    }
                    Err(e) => this.credits_error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.account_traces().await },
            |this, result, cx| {
                match result {
                    Ok(r) => {
                        this.traces = Some((r.enrolled, r.traces));
                        this.traces_error = None;
                    }
                    Err(e) => this.traces_error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn authorize(&mut self, submission_id: String, cx: &mut Context<Self>) {
        if self.authorizing.is_some() {
            return;
        }
        self.authorizing = Some(submission_id.clone());
        self.notice = None;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.authorize_trace_hold(&submission_id).await },
            |this, result, cx| {
                this.authorizing = None;
                this.notice = Some(match result {
                    Ok(r) if r.authorized => "Trace authorized for submission.".into(),
                    Ok(_) => "That trace is no longer held.".into(),
                    Err(e) => e.to_string().into(),
                });
                this.refresh(cx);
            },
        );
    }

    /// Each mint burns a single-use link, so a second click while one is in
    /// flight is ignored. The URL goes straight to the browser, never to logs.
    fn open_account(&mut self, cx: &mut Context<Self>) {
        if self.opening {
            return;
        }
        self.opening = true;
        self.notice = None;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.trace_account_login_link().await },
            |this, result, cx| {
                this.opening = false;
                match result {
                    Ok(r) => match r.url.filter(|u| r.minted && is_web_url(u)) {
                        Some(url) => cx.open_url(&url),
                        None => {
                            this.notice = Some(
                                "Could not open your Trace Commons account. Check enrollment and try again."
                                    .into(),
                            )
                        }
                    },
                    Err(e) => this.notice = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn enrolled(&self) -> bool {
        self.credits.as_ref().is_some_and(|c| c.enrolled)
            || self.traces.as_ref().is_some_and(|(enrolled, _)| *enrolled)
    }

    fn render_credits(&self, c: &RebornTraceCreditsResponse, cx: &Context<Self>) -> AnyElement {
        let last = |at: Option<chrono::DateTime<chrono::Utc>>| at.map_or("never".into(), jobs::ago);
        let rows = [
            (
                "Enrollment",
                None,
                if c.enrolled {
                    "Enrolled".into()
                } else {
                    "Not enrolled".into()
                },
            ),
            (
                "Pending credit",
                Some("Earned but not yet finalized"),
                credit(c.pending_credit),
            ),
            (
                "Final credit",
                Some("Confirmed credit"),
                credit(c.final_credit),
            ),
            (
                "Delayed ledger",
                Some("Can still change after review"),
                signed_credit(c.delayed_credit_delta),
            ),
            (
                "Submissions",
                None,
                format!(
                    "{} submitted, {} accepted of {} total",
                    c.submissions_submitted, c.submissions_accepted, c.submissions_total
                ),
            ),
            ("Last submission", None, last(c.last_submission_at)),
            (
                "Last credit sync",
                Some("Local view as of last sync"),
                last(c.last_credit_sync_at),
            ),
        ];
        let muted = cx.theme().muted_foreground;
        let holds = c.holds.iter().map(|hold| {
            let id = hold.submission_id.clone();
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(hold.reason.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(hold.submission_id.clone()),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("authorize-{id}")))
                        .label("Authorize")
                        .outline()
                        .small()
                        .loading(self.authorizing.as_deref() == Some(id.as_str()))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.authorize(id.clone(), cx)),
                        ),
                )
        });

        v_flex()
            .gap_3()
            .child(
                card(cx).children(rows.into_iter().map(|(label, hint, value)| {
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().child(label))
                                .when_some(hint, |d, h| {
                                    d.child(div().text_xs().text_color(muted).child(h))
                                }),
                        )
                        .child(div().text_sm().font_family("monospace").child(value))
                })),
            )
            .when(!c.recent_explanations.is_empty(), |d| {
                d.child(
                    v_flex().gap_1().text_xs().text_color(muted).children(
                        c.recent_explanations
                            .iter()
                            .map(|l| div().child(format!("• {l}"))),
                    ),
                )
            })
            .when(!c.holds.is_empty(), |d| {
                d.child(
                    card(cx)
                        .child(div().font_semibold().child("Held for review"))
                        .child(div().text_xs().text_color(muted).whitespace_normal().child(
                            "Held because of higher privacy risk; review and authorize to submit.",
                        ))
                        .children(holds),
                )
            })
            .into_any_element()
    }

    fn render_traces(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if let Some(e) = &self.traces_error {
            return Some(error_line(e, cx).into_any_element());
        }
        let (enrolled, traces) = self.traces.as_ref()?;
        if !enrolled || traces.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let rows = traces.iter().map(|t| {
            let final_credit = t.final_credit.map_or("–".into(), credit);
            v_flex()
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(t.submission_id.clone()),
                        )
                        .child(div().text_sm().child(t.status.clone())),
                )
                .child(div().text_xs().text_color(muted).child(format!(
                    "pending {} · final {} · received {}",
                    credit(t.pending_credit),
                    final_credit,
                    t.received_at.as_deref().unwrap_or("never"),
                )))
        });
        Some(
            card(cx)
                .child(div().font_semibold().child("Submitted traces"))
                .children(rows)
                .into_any_element(),
        )
    }
}

impl Render for TracePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match (&self.credits, &self.credits_error) {
            (_, Some(e)) => error_line(e, cx).into_any_element(),
            (None, None) => empty_state("Loading…", cx).into_any_element(),
            (Some(c), None) if !c.enrolled && c.submissions_total == 0 => empty_state(
                "Not enrolled. Ask your agent to onboard with a Trace Commons invite.",
                cx,
            )
            .into_any_element(),
            (Some(c), None) => self.render_credits(c, cx),
        };
        let note = self.credits.as_ref().map(|c| c.note.clone());
        v_flex()
            .gap_3()
            .child(body)
            .children(self.render_traces(cx))
            .when(self.enrolled(), |d| {
                d.child(
                    h_flex().child(
                        Button::new("open-trace-account")
                            .label("Open Trace Commons account")
                            .outline()
                            .small()
                            .loading(self.opening)
                            .on_click(cx.listener(|this, _, _, cx| this.open_account(cx))),
                    ),
                )
            })
            .when_some(self.notice.clone(), |d, n| {
                d.child(div().text_sm().child(n))
            })
            .when_some(note.filter(|_| self.enrolled()), |d, n| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .whitespace_normal()
                        .child(n),
                )
            })
    }
}

/// Two decimals; `+ 0.0` folds the gateway's `-0.0` into `0.00`.
fn credit(value: f32) -> String {
    format!("{:.2}", value + 0.0)
}

fn signed_credit(value: f32) -> String {
    let value = value + 0.0;
    format!("{}{value:.2}", if value >= 0.0 { "+" } else { "" })
}

/// The login link is a credential that lands in the browser; refuse anything
/// that is not plain http(s).
fn is_web_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_and_link_formatting() {
        assert_eq!(credit(-0.0), "0.00");
        assert_eq!(signed_credit(-0.0), "+0.00");
        assert_eq!(signed_credit(-1.5), "-1.50");
        assert!(is_web_url("HTTPS://traces.example/login?code=x"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("file:///etc/passwd"));
    }
}
