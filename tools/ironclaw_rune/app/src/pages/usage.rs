//! Usage, after t3code's page (GPUI `pages/usage.rs`): Limits shows
//! subscription windows per provider (Claude CLI logins, Codex, Z.ai,
//! Antigravity, Muse Code) as bars with reset countdowns and a pace marker;
//! Cost and Tokens show what the coding CLIs spent over a period
//! (`history.rs`, GPUI `usage_history.rs`, with `compactions.rs`, GPUI
//! `usage_compactions.rs`). The provider logic behind both is
//! `ironclaw_usage`.
//!
//! ## Shared with the status bar (area B)
//!
//! GPUI's `UsageModel` was read by both this page and the workspace status
//! bar, so one request fed both. Here it is [`UsageState`] in
//! `s.pages.usage`, and the status bar uses these accessors:
//!
//! - [`report`]`(s) -> Option<&UsageReport>`: the last subscription report.
//! - [`peak`]`(s) -> Option<(&ProviderUsage, &UsageWindow)>`: the busiest
//!   live window across providers (GPUI `UsageModel::peak`).
//! - [`refresh`]`(s, force)`: fetch unless the last answer is younger than a
//!   minute (`force` skips that). Safe to call from a status poll.
//! - [`level_color`]`(p, pct)`, [`short_countdown`]`(ms)`, [`countdown`]`(ms)`.

mod compactions;
pub(crate) mod common_stub;
mod history;

use std::time::{Duration, Instant};

use ironclaw_gpui_client::usage::{ProviderUsage, UsageHistory, UsageReport, UsageStatus, UsageWindow};
use rune::{Color, Cursor, Cx, El, col, el, row};
use rune_kit::Kit;

use self::common_stub::{
    BODY_SEMIBOLD, SM, XS, empty_state, error_line, header, label, page_body, refresh_button, rule,
};
pub use self::history::Metric;
use crate::design::{self, Palette};
use crate::jobs;
use crate::state::State;

/// The gateway polls each provider at most once a minute; asking more often
/// only returns its cache.
const MIN_REFRESH: Duration = Duration::from_secs(60);

/// t3code's periods.
const PERIODS: [(u32, &str); 4] = [(1, "24h"), (7, "7d"), (30, "30d"), (90, "90d")];

const METRICS: [(Metric, &str); 3] = [
    (Metric::Limits, "Limits"),
    (Metric::Cost, "Cost"),
    (Metric::Tokens, "Tokens"),
];

/// GPUI `UsageModel` plus `UsageView`'s metric and period.
pub struct UsageState {
    pub report: Option<UsageReport>,
    pub error: Option<String>,
    fetched: Option<Instant>,
    in_flight: bool,
    /// Token history for the period in `history.0` (days).
    pub history: Option<(u32, UsageHistory)>,
    pub history_error: Option<String>,
    history_fetched: Option<(u32, Instant)>,
    history_in_flight: bool,
    /// t3code's metric switch: Limits, Cost or Tokens.
    metric: Metric,
    /// The Cost and Tokens period, in days.
    days: u32,
}

impl Default for UsageState {
    fn default() -> Self {
        Self {
            report: None,
            error: None,
            fetched: None,
            in_flight: false,
            history: None,
            history_error: None,
            history_fetched: None,
            history_in_flight: false,
            metric: Metric::Limits,
            days: 30,
        }
    }
}

/// The last subscription report, for the status bar.
pub fn report(s: &State) -> Option<&UsageReport> {
    s.pages.usage.report.as_ref()
}

/// The busiest live window across providers, for the status bar.
pub fn peak(s: &State) -> Option<(&ProviderUsage, &UsageWindow)> {
    report(s)?
        .providers
        .iter()
        .flat_map(|p| p.windows.iter().map(move |w| (p, w)))
        .max_by(|a, b| a.1.used_percent.total_cmp(&b.1.used_percent))
}

/// Fetch the report unless the last answer is younger than a minute
/// (`force` skips that, for the Refresh button).
pub fn refresh(s: &mut State, force: bool) {
    let u = &mut s.pages.usage;
    let fresh = u.fetched.is_some_and(|at| at.elapsed() < MIN_REFRESH);
    if u.in_flight || (fresh && !force) {
        return;
    }
    u.in_flight = true;
    jobs::run(
        s,
        "usage",
        |c| async move { c.usage_report().await },
        |s, result, _tx| {
            let u = &mut s.pages.usage;
            u.in_flight = false;
            u.fetched = Some(Instant::now());
            match result {
                Ok(report) => {
                    u.report = Some(report);
                    u.error = None;
                }
                Err(e) => u.error = Some(e.to_string()),
            }
        },
    );
}

/// Fetch `days` of token history unless that period was read within the
/// minute (`force` skips that). A scan reads transcripts from disk, so it is
/// never polled.
pub fn refresh_history(s: &mut State, days: u32, force: bool) {
    let u = &mut s.pages.usage;
    let fresh = u
        .history_fetched
        .is_some_and(|(d, at)| d == days && at.elapsed() < MIN_REFRESH);
    if u.history_in_flight || (fresh && !force) {
        return;
    }
    u.history_in_flight = true;
    jobs::run(
        s,
        "usage",
        move |c| async move { c.usage_history(days).await },
        move |s, result, _tx| {
            let u = &mut s.pages.usage;
            u.history_in_flight = false;
            u.history_fetched = Some((days, Instant::now()));
            match result {
                Ok(history) => {
                    u.history = Some((days, history));
                    u.history_error = None;
                }
                Err(e) => u.history_error = Some(e.to_string()),
            }
        },
    );
}

/// The page became visible: load on first visit, refresh after (GPUI
/// `UsageView::refresh`, which asks for the shown metric only).
pub fn opened(s: &mut State) {
    let (metric, days) = (s.pages.usage.metric, s.pages.usage.days);
    match metric {
        Metric::Limits => refresh(s, false),
        _ => refresh_history(s, days, false),
    }
}

fn set_metric(s: &mut State, metric: Metric) {
    s.pages.usage.metric = metric;
    opened(s);
}

fn set_days(s: &mut State, days: u32) {
    s.pages.usage.days = days;
    opened(s);
}

/// A row of pill buttons, one selected (GPUI `segmented`).
fn segmented<T: Copy + PartialEq + 'static>(
    p: &Palette,
    key: &str,
    options: &[(T, &'static str)],
    current: T,
    on_pick: fn(&mut State, T),
) -> El {
    let mut r = row().key(key.to_string()).padding(2.0).gap(2.0).radius(8.0).fill(p.surface_raised);
    for (ix, (value, name)) in options.iter().enumerate() {
        let value = *value;
        let on = value == current;
        let mut pill = el()
            .key(format!("{key}-{ix}"))
            .role("radio")
            .checked(on)
            .label(*name)
            .px(10.0)
            .py(4.0)
            .radius(6.0)
            .cursor(Cursor::Pointer)
            .child(label(*name, XS, if on { p.text } else { p.text_muted }))
            .on_press(move |s: &mut State| on_pick(s, value));
        if on {
            pill = pill.fill(p.surface_card).shadow(1.0, 2.0, Color::BLACK.alpha(0.12));
        } else {
            pill = pill.hover(p.hover);
        }
        r = r.child(pill);
    }
    r
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let u = &s.pages.usage;
    let (metric, days) = (u.metric, u.days);
    let mut actions = row()
        .gap(8.0)
        .center_y()
        .child(segmented(p, "usage-metric", &METRICS, metric, set_metric));
    if metric != Metric::Limits {
        actions = actions.child(segmented(p, "usage-period", &PERIODS, days, set_days));
    }
    actions = actions.child(refresh_button(k, "usage-refresh", move |s: &mut State| {
        let (metric, days) = (s.pages.usage.metric, s.pages.usage.days);
        match metric {
            Metric::Limits => refresh(s, true),
            _ => refresh_history(s, days, true),
        }
    }));

    let mut page = col().w_full().child(header(p, "Usage", actions));
    if metric != Metric::Limits {
        if let Some(e) = &u.history_error {
            page = page.child(error_line(p, e));
        }
        let body = match (&u.history, &u.history_error) {
            (Some((d, history)), _) => history::view(
                s,
                history,
                metric,
                u.history_in_flight || *d != days,
            ),
            (None, Some(_)) => el(),
            (None, None) => empty_state(p, "Reading the CLIs' transcripts…"),
        };
        return page_body("usage-page", page.child(body.key(format!("usage-{metric:?}"))));
    }
    if let Some(e) = &u.error {
        page = page.child(error_line(p, e));
    }
    let body = match (&u.report, &u.error) {
        (None, None) => empty_state(p, "Loading usage…"),
        (None, Some(_)) => el(),
        (Some(report), _) if report.providers.is_empty() => empty_state(p, "No providers to show."),
        (Some(report), _) => {
            // Providers with bars first; the ones without collapse to a line
            // each at the end.
            let (live, idle): (Vec<_>, Vec<_>) =
                report.providers.iter().partition(|pr| !pr.windows.is_empty());
            let now = chrono::Utc::now().timestamp_millis();
            let mut c = col().w_full().gap(16.0);
            for (ix, pr) in live.into_iter().enumerate() {
                c = c.child(provider_card(p, ix, pr, now));
            }
            if !idle.is_empty() {
                let mut list = col()
                    .key("usage-idle")
                    .w_full()
                    .radius(design::PANEL_RADIUS)
                    .border(1.0, p.border)
                    .fill(p.surface_card);
                for (ix, pr) in idle.into_iter().enumerate() {
                    if ix > 0 {
                        list = list.child(rule(p).key(format!("usage-idle-rule-{ix}")));
                    }
                    list = list.child(idle_row(p, ix, pr));
                }
                c = c.child(list);
            }
            c
        }
    };
    page_body("usage-page", page.child(body.key("usage-limits")))
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

fn provider_card(p: &Palette, ix: usize, provider: &ProviderUsage, now: i64) -> El {
    let stale = provider.fetched_at.is_some_and(|at| now - at > STALE_MS);
    let mut title = row()
        .gap(8.0)
        .center_y()
        .child(label(provider.label.clone(), BODY_SEMIBOLD, p.text));
    if let Some(plan) = &provider.plan {
        title = title.child(
            el().px(6.0)
                .radius(4.0)
                .fill(p.accent_wash)
                .child(label(plan.clone(), XS, p.accent_strong)),
        );
    }
    let mut top = row().w_full().justify_between().center_y().gap(8.0).child(title);
    if let Some(st) = status_text(provider.status) {
        top = top.child(label(st, XS, p.warning));
    }
    let mut card = col()
        .key(format!("usage-provider-{ix}"))
        .w_full()
        .gap(12.0)
        .padding(16.0)
        .radius(design::PANEL_RADIUS)
        .border(1.0, p.border)
        .fill(p.surface_card)
        .child(top);
    if let Some(m) = &provider.message {
        card = card.child(label(m.clone(), SM, p.text_muted).wrap());
    }
    for (wx, w) in provider.windows.iter().enumerate() {
        card = card.child(window_row(p, w, now).key(format!("window-{wx}")));
    }
    let updated = match provider.fetched_at {
        Some(at) if stale => label(
            format!("Updated {} · may be out of date", jobs::ago(from_millis(at))),
            XS,
            p.warning,
        ),
        Some(at) => label(format!("Updated {}", jobs::ago(from_millis(at))), XS, p.text_faint),
        None => el(),
    };
    let mut foot = row().w_full().justify_between().gap(12.0).child(updated);
    if let Some(detail) = &provider.detail {
        foot = foot.child(label(detail.clone(), XS, p.text_faint).wrap().shrink());
    }
    card.child(foot)
}

/// One line for a provider with nothing to draw: name, then why.
fn idle_row(p: &Palette, ix: usize, provider: &ProviderUsage) -> El {
    let why = provider
        .message
        .clone()
        .or_else(|| status_text(provider.status).map(str::to_string))
        .unwrap_or_else(|| "No limits reported.".to_string());
    row()
        .key(format!("usage-idle-{ix}"))
        .w_full()
        .px(16.0)
        .py(10.0)
        .gap(12.0)
        .items_start()
        .child(el().w(120.0).child(label(provider.label.clone(), SM, p.text_muted)))
        .child(el().grow(1.0).min_w(0.0).child(label(why, SM, p.text_faint).wrap()))
}

fn window_row(p: &Palette, window: &UsageWindow, now: i64) -> El {
    let pct = window.used_percent.clamp(0., 100.);
    let resets = window.resets_at.map(countdown);
    let elapsed = elapsed_share(window, now);
    // Only overspending earns a caption; the tick shows the rest.
    let ahead = elapsed.and_then(|e| ahead_of_pace(pct, e));
    let mut right = row().gap(8.0).child(label(format!("{pct:.0}%"), SM, p.text));
    if let Some(r) = resets {
        right = right.child(label(r, SM, p.text_faint));
    }
    let mut bar = el()
        .key("bar")
        .w_full()
        .h(6.0)
        .radius(3.0)
        .fill(p.surface_raised)
        .child(el().key("fill").h_full().w_frac(pct as f32 / 100.).radius(3.0).fill(level_color(p, pct)));
    // Where even spending would be by now.
    if let Some(e) = elapsed {
        bar = bar.child(
            row()
                .key("pace")
                .absolute(0.0, -2.0)
                .w_full()
                .h(10.0)
                .child(el().w_frac(e as f32))
                .child(el().w(2.0).h(10.0).radius(1.0).fill(p.text_muted)),
        );
    }
    let mut c = col()
        .w_full()
        .gap(4.0)
        .child(
            row()
                .w_full()
                .justify_between()
                .child(label(window.label.clone(), SM, p.text))
                .child(right),
        )
        .child(bar);
    if let Some(caption) = ahead {
        c = c.child(label(caption, XS, p.warning));
    }
    c
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
pub fn level_color(p: &Palette, pct: f64) -> Color {
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

// --- fixtures (scenes.rs) ---

/// The Limits view with three live providers and two idle ones.
pub fn fixture_limits(s: &mut State) {
    let now = chrono::Utc::now().timestamp_millis();
    let report = serde_json::json!({ "providers": [
        { "provider": "claude_cli", "label": "Claude (work)", "status": "ok", "plan": "max",
          "fetchedAt": now - 60_000,
          "windows": [
            { "id": "5h", "label": "5-hour", "usedPercent": 72.0, "resetsAt": now + 2 * 3_600_000 + 14 * 60_000, "windowMinutes": 300 },
            { "id": "7d", "label": "Weekly", "usedPercent": 31.0, "resetsAt": now + 4 * 86_400_000, "windowMinutes": 10080 }
          ] },
        { "provider": "codex", "label": "Codex", "status": "ok", "plan": "plus",
          "fetchedAt": now - 45 * 60_000, "detail": "2 spare resets",
          "windows": [
            { "id": "5h", "label": "5-hour", "usedPercent": 94.0, "resetsAt": now + 20 * 60_000, "windowMinutes": 300 }
          ] },
        { "provider": "zai", "label": "Z.ai", "status": "ok", "fetchedAt": now - 5 * 60_000,
          "windows": [ { "id": "month", "label": "Monthly", "usedPercent": 12.0 } ] },
        { "provider": "antigravity", "label": "Antigravity", "status": "not_configured" },
        { "provider": "muse", "label": "Muse Code", "status": "unauthorized",
          "message": "The token was rejected; sign in to Muse Code again." }
    ]});
    match serde_json::from_value(report) {
        Ok(r) => s.pages.usage.report = Some(r),
        Err(e) => tracing::warn!(error = %e, "usage fixture does not decode"),
    }
    s.pages.usage.fetched = Some(Instant::now());
}

/// The Cost view over 30 days, with idle compactions.
pub fn fixture_history(s: &mut State, metric: Metric) {
    match history::fixture(30) {
        Ok(h) => s.pages.usage.history = Some((30, h)),
        Err(e) => tracing::warn!(error = %e, "usage history fixture does not decode"),
    }
    s.pages.usage.history_fetched = Some((30, Instant::now()));
    s.pages.usage.metric = metric;
    s.pages.usage.days = 30;
}


/// Refresh the shared usage report (the status bar's name for [`refresh`]).
pub fn refresh_report(s: &mut State, force: bool) {
    refresh(s, force);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_flags_only_overspending() {
        assert_eq!(ahead_of_pace(60., 0.5), Some("10% ahead of pace".into()));
        assert_eq!(ahead_of_pace(52., 0.5), None);
    }

    #[test]
    fn countdowns_read_like_t3code() {
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(short_countdown(now - 1000), None);
        assert_eq!(countdown(now - 1000), "resetting");
        assert_eq!(short_countdown(now + 14 * 60_000 + 30_000), Some("15m".into()));
    }
}
