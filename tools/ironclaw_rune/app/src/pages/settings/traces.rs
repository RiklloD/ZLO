//! Trace Commons: credit earned for contributed redacted traces, traces held
//! for privacy review, and a one-time link into the Trace Commons account.
//! Port of GPUI `pages/settings/traces.rs`.

use zlo_client::product_wire::{RebornAccountTrace, RebornTraceCreditsResponse};
use rune::{El, col, row};
use rune_kit::Kit;

use crate::jobs;
use crate::pages::common::{
    BASE, Btn, SM, XS, button, card, empty_state, error_line, font, label, loading, para,
};
use crate::state::State;

#[derive(Default)]
pub struct TraceState {
    pub credits: Option<RebornTraceCreditsResponse>,
    pub traces: Option<(bool, Vec<RebornAccountTrace>)>,
    pub credits_error: Option<String>,
    pub traces_error: Option<String>,
    pub authorizing: Option<String>,
    pub opening: bool,
    pub notice: Option<String>,
}

impl TraceState {
    fn enrolled(&self) -> bool {
        self.credits.as_ref().is_some_and(|c| c.enrolled)
            || self.traces.as_ref().is_some_and(|(enrolled, _)| *enrolled)
    }
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "traces",
        |c| async move { c.trace_credits().await },
        |s, result, _| {
            let this = &mut s.pages.settings.traces;
            match result {
                Ok(credits) => {
                    this.credits = Some(credits);
                    this.credits_error = None;
                }
                Err(e) => this.credits_error = Some(e.to_string()),
            }
        },
    );
    jobs::run(
        s,
        "traces",
        |c| async move { c.account_traces().await },
        |s, result, _| {
            let this = &mut s.pages.settings.traces;
            match result {
                Ok(r) => {
                    this.traces = Some((r.enrolled, r.traces));
                    this.traces_error = None;
                }
                Err(e) => this.traces_error = Some(e.to_string()),
            }
        },
    );
}

fn authorize(s: &mut State, submission_id: String) {
    let this = &mut s.pages.settings.traces;
    if this.authorizing.is_some() {
        return;
    }
    this.authorizing = Some(submission_id.clone());
    this.notice = None;
    jobs::run(
        s,
        "traces",
        move |c| async move { c.authorize_trace_hold(&submission_id).await },
        |s, result, _| {
            let this = &mut s.pages.settings.traces;
            this.authorizing = None;
            this.notice = Some(match result {
                Ok(r) if r.authorized => "Trace authorized for submission.".into(),
                Ok(_) => "That trace is no longer held.".into(),
                Err(e) => e.to_string(),
            });
            refresh(s);
        },
    );
}

/// Each mint burns a single-use link, so a second click while one is in
/// flight is ignored. The URL goes straight to the browser, never to logs.
fn open_account(s: &mut State) {
    let this = &mut s.pages.settings.traces;
    if this.opening {
        return;
    }
    this.opening = true;
    this.notice = None;
    jobs::run(
        s,
        "traces",
        |c| async move { c.trace_account_login_link().await },
        |s, result, tx| {
            let this = &mut s.pages.settings.traces;
            this.opening = false;
            match result {
                Ok(r) => match r.url.filter(|u| r.minted && is_web_url(u)) {
                    Some(url) => {
                        if let Err(e) = tx.open_url(&url) {
                            tracing::warn!(error = %e, "could not open the Trace Commons account");
                            this.notice = Some(e.to_string());
                        }
                    }
                    None => {
                        this.notice = Some(
                            "Could not open your Trace Commons account. Check enrollment and try again."
                                .into(),
                        )
                    }
                },
                Err(e) => this.notice = Some(e.to_string()),
            }
        },
    );
}

fn render_credits(s: &State, c: &RebornTraceCreditsResponse, k: &Kit) -> El {
    let p = &s.palette;
    let this = &s.pages.settings.traces;
    let last = |at: Option<chrono::DateTime<chrono::Utc>>| at.map_or("never".into(), jobs::ago);
    let rows: [(&str, Option<&str>, String); 7] = [
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
    let mut facts = card(p);
    for (title, hint, value) in rows {
        let mut left = col()
            .grow(1.0)
            .min_w(0.)
            .child(label(title, SM, 400., p.text));
        if let Some(h) = hint {
            left = left.child(label(h, XS, 400., p.text_muted));
        }
        facts = facts.child(
            row()
                .key(format!("credit-{title}"))
                .gap(12.)
                .center_y()
                .child(left)
                .child(rune::text(value).font(font(SM, 400.).mono()).color(p.text)),
        );
    }
    let mut d = col().w_full().gap(12.).child(facts);
    if !c.recent_explanations.is_empty() {
        d = d.child(
            col().gap(4.).children(
                c.recent_explanations
                    .iter()
                    .map(|l| para(format!("• {l}"), XS, p.text_muted)),
            ),
        );
    }
    if !c.holds.is_empty() {
        let mut holds = card(p)
            .child(label("Held for review", BASE, 600., p.text))
            .child(para(
                "Held because of higher privacy risk; review and authorize to submit.",
                XS,
                p.text_muted,
            ));
        for hold in &c.holds {
            let id = hold.submission_id.clone();
            let busy = this.authorizing.as_deref() == Some(id.as_str());
            holds = holds.child(
                row()
                    .key(format!("hold-{id}"))
                    .gap(12.)
                    .center_y()
                    .child(
                        col()
                            .grow(1.0)
                            .min_w(0.)
                            .child(label(hold.reason.clone(), SM, 400., p.text))
                            .child(
                                label(hold.submission_id.clone(), XS, 400., p.text_muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        loading(button("Authorize", Btn::Outline), busy)
                            .on_press(move |s: &mut State| authorize(s, id.clone()))
                            .build(k),
                    ),
            );
        }
        d = d.child(holds);
    }
    d
}

fn render_traces(s: &State) -> Option<El> {
    let p = &s.palette;
    let this = &s.pages.settings.traces;
    if let Some(e) = &this.traces_error {
        return Some(error_line(p, e));
    }
    let (enrolled, traces) = this.traces.as_ref()?;
    if !enrolled || traces.is_empty() {
        return None;
    }
    let mut c = card(p).child(label("Submitted traces", BASE, 600., p.text));
    for t in traces {
        let final_credit = t.final_credit.map_or("–".into(), credit);
        c = c.child(
            col()
                .key(format!("trace-{}", t.submission_id))
                .child(
                    row()
                        .gap(12.)
                        .child(
                            label(t.submission_id.clone(), XS, 400., p.text_muted)
                                .truncate()
                                .grow(1.0)
                                .min_w(0.),
                        )
                        .child(label(t.status.clone(), SM, 400., p.text)),
                )
                .child(label(
                    format!(
                        "pending {} · final {} · received {}",
                        credit(t.pending_credit),
                        final_credit,
                        t.received_at.as_deref().unwrap_or("never"),
                    ),
                    XS,
                    400.,
                    p.text_muted,
                )),
        );
    }
    Some(c)
}

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let this = &s.pages.settings.traces;
    let body = match (&this.credits, &this.credits_error) {
        (_, Some(e)) => error_line(p, e),
        (None, None) => empty_state(p, "Loading…"),
        (Some(c), None) if !c.enrolled && c.submissions_total == 0 => empty_state(
            p,
            "Not enrolled. Ask your agent to onboard with a Trace Commons invite.",
        ),
        (Some(c), None) => render_credits(s, c, k),
    };
    let mut d = col().w_full().gap(12.).child(body);
    if let Some(t) = render_traces(s) {
        d = d.child(t);
    }
    if this.enrolled() {
        d = d.child(
            row().child(
                loading(
                    button("Open Trace Commons account", Btn::Outline),
                    this.opening,
                )
                .on_press(open_account)
                .build(k),
            ),
        );
    }
    if let Some(n) = &this.notice {
        d = d.child(para(n.clone(), SM, p.text));
    }
    if this.enrolled()
        && let Some(c) = &this.credits
    {
        d = d.child(para(c.note.clone(), XS, p.text_muted));
    }
    d
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

pub fn fixture(s: &mut State) {
    let now = chrono::Utc::now();
    let credits = serde_json::from_value::<RebornTraceCreditsResponse>(serde_json::json!({
        "enrolled": true,
        "pending_credit": 1.25, "final_credit": 4.5, "delayed_credit_delta": -0.0,
        "submissions_total": 9, "submissions_submitted": 8, "submissions_accepted": 6,
        "submissions_revoked": 0, "submissions_expired": 1, "credit_events_total": 7,
        "last_submission_at": now - chrono::Duration::hours(3),
        "last_credit_sync_at": now - chrono::Duration::minutes(20),
        "recent_explanations": ["Accepted: shell session with a fixed test"],
        "holds": [{"submission_id": "sub_7f3a", "reason": "High residual PII risk"}],
        "note": "Credit shown is your local view as of the last sync."
    }));
    let traces = serde_json::from_value::<Vec<RebornAccountTrace>>(serde_json::json!([
        {"submission_id": "sub_51c2", "status": "accepted", "pending_credit": 0.0,
         "final_credit": 0.75, "received_at": "2026-11-05"},
        {"submission_id": "sub_51c3", "status": "pending", "pending_credit": 0.5}
    ]));
    match (credits, traces) {
        (Ok(c), Ok(t)) => {
            let this = &mut s.pages.settings.traces;
            this.credits = Some(c);
            this.traces = Some((true, t));
        }
        (Err(e), _) | (_, Err(e)) => tracing::warn!(error = %e, "trace fixture"),
    }
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
