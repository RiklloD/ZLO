//! Usage: subscription windows per provider (Claude CLI logins, the Z.ai
//! coding plan) as bars with reset countdowns; the provider logic behind it
//! is `ironclaw_usage` (ported from CodexBar). [`UsageModel`] holds the last
//! report; the page and the status bar both read it, so one request feeds
//! both.

use std::time::{Duration, Instant};

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::usage::{ProviderUsage, UsageReport, UsageStatus, UsageWindow};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::design::Palette;
use crate::jobs;
use crate::ui::*;

/// The gateway polls each provider at most once a minute; asking more often
/// only returns its cache.
const MIN_REFRESH: Duration = Duration::from_secs(60);

pub struct UsageModel {
    backend: Backend,
    pub report: Option<UsageReport>,
    pub error: Option<SharedString>,
    fetched: Option<Instant>,
    in_flight: bool,
}

impl UsageModel {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            report: None,
            error: None,
            fetched: None,
            in_flight: false,
        }
    }

    /// Fetch unless the last answer is younger than a minute (`force` skips
    /// that, for the Refresh button).
    pub fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        let fresh = self.fetched.is_some_and(|at| at.elapsed() < MIN_REFRESH);
        if self.in_flight || (fresh && !force) {
            return;
        }
        self.in_flight = true;
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.usage_report().await },
            |this, result, cx| {
                this.in_flight = false;
                this.fetched = Some(Instant::now());
                match result {
                    Ok(report) => {
                        this.report = Some(report);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// The busiest window across providers, for the status bar.
    pub fn peak(&self) -> Option<(&ProviderUsage, &UsageWindow)> {
        self.report
            .as_ref()?
            .providers
            .iter()
            .flat_map(|p| p.windows.iter().map(move |w| (p, w)))
            .max_by(|a, b| a.1.used_percent.total_cmp(&b.1.used_percent))
    }
}

pub struct UsageView {
    model: Entity<UsageModel>,
    _observe: Subscription,
}

impl EventEmitter<PageEvent> for UsageView {}

impl UsageView {
    pub fn new(model: Entity<UsageModel>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&model, |_, _, cx| cx.notify());
        model.update(cx, |m, cx| m.refresh(false, cx));
        Self {
            model,
            _observe: observe,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(false, cx));
    }
}

impl Render for UsageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let body: AnyElement = match (&model.report, &model.error) {
            (None, None) => empty_state("Loading usage…", cx).into_any_element(),
            (None, Some(_)) => div().into_any_element(),
            (Some(report), _) if report.providers.is_empty() => {
                empty_state("No providers to show.", cx).into_any_element()
            }
            (Some(report), _) => v_flex()
                .gap_4()
                .children(
                    report
                        .providers
                        .iter()
                        .enumerate()
                        .map(|(ix, p)| provider_card(ix, p, cx)),
                )
                .into_any_element(),
        };
        let error = model.error.clone();
        page_body(
            "usage-page",
            v_flex()
                .child(header(
                    "Usage",
                    refresh_button("usage-refresh").on_click(cx.listener(|this, _, _, cx| {
                        this.model.update(cx, |m, cx| m.refresh(true, cx))
                    })),
                ))
                .when_some(error, |d, e| d.child(error_line(&e, cx)))
                .child(body),
        )
    }
}

fn provider_card(ix: usize, provider: &ProviderUsage, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let status = match provider.status {
        UsageStatus::Ok => None,
        UsageStatus::NotConfigured => Some("Not set up"),
        UsageStatus::Unauthorized => Some("Login expired"),
        UsageStatus::RateLimited => Some("Rate limited"),
        UsageStatus::Error => Some("Unavailable"),
    };
    v_flex()
        .id(("usage-provider", ix))
        .gap_3()
        .p_4()
        .rounded(px(crate::design::PANEL_RADIUS))
        .border_1()
        .border_color(p.border)
        .bg(p.surface_card)
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(div().font_semibold().child(provider.label.clone()))
                .when_some(status, |d, s| {
                    d.child(div().text_xs().text_color(p.text_faint).child(s))
                }),
        )
        .when_some(provider.message.clone(), |d, m| {
            d.child(
                div()
                    .text_sm()
                    .text_color(p.text_muted)
                    .whitespace_normal()
                    .child(m),
            )
        })
        .children(provider.windows.iter().map(|w| window_row(w, cx)))
        .when_some(provider.fetched_at, |d, at| {
            d.child(
                div()
                    .text_xs()
                    .text_color(p.text_faint)
                    .child(format!("Updated {}", jobs::ago(from_millis(at)))),
            )
        })
}

fn window_row(window: &UsageWindow, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let pct = window.used_percent.clamp(0., 100.);
    let resets = window.resets_at.map(countdown);
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .justify_between()
                .text_sm()
                .child(window.label.clone())
                .child(
                    h_flex()
                        .gap_2()
                        .child(format!("{pct:.0}%"))
                        .when_some(resets, |d, r| {
                            d.child(div().text_color(p.text_faint).child(r))
                        }),
                ),
        )
        .child(
            div()
                .h(px(6.))
                .w_full()
                .rounded_full()
                .bg(p.surface_raised)
                .child(
                    div()
                        .h_full()
                        .w(gpui_kit::relative((pct / 100.) as f32))
                        .rounded_full()
                        .bg(level_color(pct, cx)),
                ),
        )
}

/// Green until 70 %, amber until 90 %, then red.
pub fn level_color(pct: f64, cx: &App) -> gpui_kit::Hsla {
    let p = Palette::of(cx);
    match pct {
        x if x >= 90. => p.danger,
        x if x >= 70. => p.warning,
        _ => p.success,
    }
}

fn from_millis(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_else(chrono::Utc::now)
}

/// "resets in 2 h 14 min", "resets in 3 d 4 h", "resetting".
pub fn countdown(resets_at_ms: i64) -> String {
    let secs = (resets_at_ms - chrono::Utc::now().timestamp_millis()) / 1000;
    match secs {
        s if s <= 0 => "resetting".into(),
        s if s < 3600 => format!("resets in {} min", (s + 59) / 60),
        s if s < 86_400 => format!("resets in {} h {} min", s / 3600, (s % 3600) / 60),
        s => format!("resets in {} d {} h", s / 86_400, (s % 86_400) / 3600),
    }
}
