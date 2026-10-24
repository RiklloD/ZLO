//! Usage, after t3code's page: Limits shows subscription windows per
//! provider (Claude CLI logins, Codex, Z.ai, Antigravity, Muse Code) as bars
//! with reset countdowns and a pace marker; Cost and Tokens show what the
//! coding CLIs spent over a period (`usage_history`). The provider logic
//! behind both is `ironclaw_usage`. [`UsageModel`] holds the last answers;
//! the page and the status bar both read it, so one request feeds both.

use std::time::{Duration, Instant};

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::usage::{
    ProviderUsage, UsageHistory, UsageReport, UsageStatus, UsageWindow,
};

use super::usage_history::{self, Metric};
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
    /// Token history for the period in `history.0` (days).
    pub history: Option<(u32, UsageHistory)>,
    pub history_error: Option<SharedString>,
    history_fetched: Option<(u32, Instant)>,
    history_in_flight: bool,
}

impl UsageModel {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            report: None,
            error: None,
            fetched: None,
            in_flight: false,
            history: None,
            history_error: None,
            history_fetched: None,
            history_in_flight: false,
        }
    }

    /// Fetch `days` of token history unless that period was read within the
    /// minute (`force` skips that). A scan reads transcripts from disk, so
    /// it is never polled.
    pub fn refresh_history(&mut self, days: u32, force: bool, cx: &mut Context<Self>) {
        let fresh = self
            .history_fetched
            .is_some_and(|(d, at)| d == days && at.elapsed() < MIN_REFRESH);
        if self.history_in_flight || (fresh && !force) {
            return;
        }
        self.history_in_flight = true;
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.usage_history(days).await },
            move |this, result, cx| {
                this.history_in_flight = false;
                this.history_fetched = Some((days, Instant::now()));
                match result {
                    Ok(history) => {
                        this.history = Some((days, history));
                        this.history_error = None;
                    }
                    Err(e) => this.history_error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    pub fn history_loading(&self) -> bool {
        self.history_in_flight
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
    /// t3code's metric switch: Limits, Cost or Tokens.
    metric: Metric,
    /// The Cost and Tokens period, in days.
    days: u32,
    _observe: Subscription,
}

/// t3code's periods.
const PERIODS: [(u32, &str); 4] = [(1, "24h"), (7, "7d"), (30, "30d"), (90, "90d")];

impl EventEmitter<PageEvent> for UsageView {}

impl UsageView {
    pub fn new(model: Entity<UsageModel>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&model, |_, _, cx| cx.notify());
        model.update(cx, |m, cx| m.refresh(false, cx));
        Self {
            model,
            metric: Metric::Limits,
            days: 30,
            _observe: observe,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let days = self.days;
        match self.metric {
            Metric::Limits => self.model.update(cx, |m, cx| m.refresh(false, cx)),
            _ => self
                .model
                .update(cx, |m, cx| m.refresh_history(days, false, cx)),
        }
    }

    fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        self.metric = metric;
        self.refresh(cx);
        cx.notify();
    }

    fn set_days(&mut self, days: u32, cx: &mut Context<Self>) {
        self.days = days;
        self.refresh(cx);
        cx.notify();
    }
}

/// A row of pill buttons, one selected.
fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    on_pick: impl Fn(&mut UsageView, T, &mut Context<UsageView>) + Clone + 'static,
    cx: &mut Context<UsageView>,
) -> impl IntoElement {
    let p = *Palette::of(cx);
    h_flex()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(8.))
        .bg(p.surface_raised)
        .children(options.iter().enumerate().map(|(ix, (value, label))| {
            let value = *value;
            let on = value == current;
            let on_pick = on_pick.clone();
            div()
                .id((id, ix))
                .px_2p5()
                .py_1()
                .rounded(px(6.))
                .text_xs()
                .cursor_pointer()
                .text_color(if on { p.text } else { p.text_muted })
                .when(on, |d| d.bg(p.surface_card).shadow_sm())
                .hover(|s| s.text_color(p.text))
                .child(*label)
                .on_click(cx.listener(move |this, _, _, cx| on_pick(this, value, cx)))
        }))
}

impl Render for UsageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metric = self.metric;
        let days = self.days;
        let switch = segmented(
            "usage-metric",
            &[
                (Metric::Limits, "Limits"),
                (Metric::Cost, "Cost"),
                (Metric::Tokens, "Tokens"),
            ],
            metric,
            |this, m, cx| this.set_metric(m, cx),
            cx,
        )
        .into_any_element();
        let period = (metric != Metric::Limits).then(|| {
            segmented(
                "usage-period",
                &PERIODS,
                days,
                |this, d, cx| this.set_days(d, cx),
                cx,
            )
            .into_any_element()
        });
        let actions = h_flex()
            .gap_2()
            .items_center()
            .child(switch)
            .children(period)
            .child(
                refresh_button("usage-refresh").on_click(cx.listener(move |this, _, _, cx| {
                    match this.metric {
                        Metric::Limits => this.model.update(cx, |m, cx| m.refresh(true, cx)),
                        _ => this
                            .model
                            .update(cx, |m, cx| m.refresh_history(days, true, cx)),
                    }
                })),
            );
        let model = self.model.read(cx);
        if metric != Metric::Limits {
            let loading = model.history_loading();
            let body = match (&model.history, &model.history_error) {
                (Some((_, history)), _) => usage_history::render(
                    history,
                    metric,
                    loading || model.history.as_ref().is_some_and(|(d, _)| *d != days),
                    cx,
                ),
                (None, Some(_)) => div().into_any_element(),
                (None, None) => {
                    empty_state("Reading the CLIs' transcripts…", cx).into_any_element()
                }
            };
            let error = model.history_error.clone();
            return page_body(
                "usage-page",
                v_flex()
                    .child(header("Usage", actions))
                    .when_some(error, |d, e| d.child(error_line(&e, cx)))
                    .child(body),
            )
            .into_any_element();
        }
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
                .child(header("Usage", actions))
                .when_some(error, |d, e| d.child(error_line(&e, cx)))
                .child(body),
        )
        .into_any_element()
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
    // Only overspending earns a caption; the tick shows the rest.
    let ahead = elapsed.and_then(|e| ahead_of_pace(pct, e));
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
        .when_some(ahead, |d, label| {
            d.child(div().text_xs().text_color(p.warning).child(label))
        })
}

/// Share of the window already gone, from its reset and length.
fn elapsed_share(window: &UsageWindow, now: i64) -> Option<f64> {
    let resets = window.resets_at?;
    let length = f64::from(window.window_minutes?) * 60_000.;
    (length > 0.).then(|| ((length - (resets - now) as f64) / length).clamp(0., 1.))
}

/// t3code's pace verdict, kept only for "ahead": more than 5 points over
/// even spending for the time gone.
fn ahead_of_pace(used_pct: f64, elapsed: f64) -> Option<String> {
    let gap = used_pct - elapsed * 100.;
    (gap > 5.).then(|| format!("{gap:.0}% ahead of pace"))
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
