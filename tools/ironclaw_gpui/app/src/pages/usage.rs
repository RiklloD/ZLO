//! Usage: subscription windows per provider (Claude CLI logins, Codex,
//! Z.ai, Antigravity, Muse Code) as bars with reset countdowns and a pace
//! marker, after t3code's limits view; the provider logic behind it is
//! `ironclaw_usage`. [`UsageModel`] holds the last report; the page and the
//! status bar both read it, so one request feeds both.

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

    /// The busiest live window across providers, for the status bar.
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
            (Some(report), _) => {
                // Providers with bars first; the ones without collapse to a
                // line each at the end.
                let (live, idle): (Vec<_>, Vec<_>) =
                    report.providers.iter().partition(|p| !p.windows.is_empty());
                v_flex()
                    .gap_4()
                    .children(
                        live.into_iter()
                            .enumerate()
                            .map(|(ix, p)| provider_card(ix, p, cx).into_any_element()),
                    )
                    .when(!idle.is_empty(), |d| {
                        d.child(
                            v_flex()
                                .rounded(px(crate::design::PANEL_RADIUS))
                                .border_1()
                                .border_color(Palette::of(cx).border)
                                .bg(Palette::of(cx).surface_card)
                                .children(
                                    idle.into_iter()
                                        .enumerate()
                                        .map(|(ix, p)| idle_row(ix, p, cx)),
                                ),
                        )
                    })
                    .into_any_element()
            }
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

fn status_text(status: UsageStatus) -> Option<&'static str> {
    match status {
        UsageStatus::Ok => None,
        UsageStatus::NotConfigured => Some("Not set up"),
        UsageStatus::Unauthorized => Some("Sign-in needed"),
        UsageStatus::RateLimited => Some("Rate limited"),
        UsageStatus::Error => Some("Unavailable"),
    }
}

/// A reading older than this is flagged: limits move within minutes.
const STALE_MS: i64 = 30 * 60 * 1000;

fn provider_card(ix: usize, provider: &ProviderUsage, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let now = chrono::Utc::now().timestamp_millis();
    let stale = provider.fetched_at.is_some_and(|at| now - at > STALE_MS);
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
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().font_semibold().child(provider.label.clone()))
                        .when_some(provider.plan.clone(), |d, plan| {
                            d.child(
                                div()
                                    .px_1p5()
                                    .rounded(px(4.))
                                    .bg(p.accent_wash)
                                    .text_xs()
                                    .text_color(p.accent_strong)
                                    .child(plan),
                            )
                        }),
                )
                .when_some(status_text(provider.status), |d, s| {
                    d.child(div().text_xs().text_color(p.warning).child(s))
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
        .children(provider.windows.iter().map(|w| window_row(w, now, cx)))
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .text_xs()
                .text_color(p.text_faint)
                .child(match provider.fetched_at {
                    Some(at) if stale => div().text_color(p.warning).child(format!(
                        "Updated {} · may be out of date",
                        jobs::ago(from_millis(at))
                    )),
                    Some(at) => div().child(format!("Updated {}", jobs::ago(from_millis(at)))),
                    None => div(),
                })
                .when_some(provider.detail.clone(), |d, detail| {
                    d.child(div().whitespace_normal().child(detail))
                }),
        )
}

/// One line for a provider with nothing to draw: name, then why.
fn idle_row(ix: usize, provider: &ProviderUsage, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    h_flex()
        .id(("usage-idle", ix))
        .when(ix > 0, |d| d.border_t_1().border_color(p.border))
        .px_4()
        .py_2p5()
        .gap_3()
        .items_start()
        .text_sm()
        .child(
            div()
                .flex_none()
                .w(px(120.))
                .text_color(p.text_muted)
                .child(provider.label.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(p.text_faint)
                .whitespace_normal()
                .child(
                    provider
                        .message
                        .clone()
                        .or_else(|| status_text(provider.status).map(str::to_string))
                        .unwrap_or_else(|| "No limits reported.".to_string()),
                ),
        )
}

fn window_row(window: &UsageWindow, now: i64, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let pct = window.used_percent.clamp(0., 100.);
    let resets = window.resets_at.map(countdown);
    let elapsed = elapsed_share(window, now);
    let pace = elapsed.map(|e| pace(pct, e));
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
                .relative()
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
                )
                // Where even spending would be by now.
                .when_some(elapsed, |d, e| {
                    d.child(
                        div()
                            .absolute()
                            .top(px(-2.))
                            .left(gpui_kit::relative(e as f32))
                            .w(px(2.))
                            .h(px(10.))
                            .rounded_full()
                            .bg(p.text_muted),
                    )
                }),
        )
        .when_some(pace, |d, (label, ahead)| {
            d.child(
                div()
                    .text_xs()
                    .text_color(if ahead { p.warning } else { p.text_faint })
                    .child(label),
            )
        })
}

/// Share of the window already gone, from its reset and length.
fn elapsed_share(window: &UsageWindow, now: i64) -> Option<f64> {
    let resets = window.resets_at?;
    let length = f64::from(window.window_minutes?) * 60_000.;
    (length > 0.).then(|| ((length - (resets - now) as f64) / length).clamp(0., 1.))
}

/// t3code's verdict: more than 5 points from even pace either way.
fn pace(used_pct: f64, elapsed: f64) -> (String, bool) {
    let gap = used_pct - elapsed * 100.;
    match gap {
        g if g > 5. => (format!("{g:.0}% ahead of pace"), true),
        g if g < -5. => (format!("{:.0}% under pace", -g), false),
        _ => ("On pace".to_string(), false),
    }
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

/// "2d 4h", "2h 14m", "14m", as t3code prints them; `None` once past.
pub fn short_countdown(resets_at_ms: i64) -> Option<String> {
    let secs = (resets_at_ms - chrono::Utc::now().timestamp_millis()) / 1000;
    match secs {
        s if s <= 0 => None,
        s if s < 3600 => Some(format!("{}m", (s + 59) / 60)),
        s if s < 86_400 => Some(format!("{}h {}m", s / 3600, (s % 3600) / 60)),
        s => Some(format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)),
    }
}

/// "resets in 2h 14m", "resetting".
pub fn countdown(resets_at_ms: i64) -> String {
    short_countdown(resets_at_ms)
        .map(|c| format!("resets in {c}"))
        .unwrap_or_else(|| "resetting".into())
}
