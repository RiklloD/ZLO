//! The Usage page's Cost and Tokens views (GPUI `pages/usage_history.rs`),
//! after t3code's usage overview: the period's total and each provider's
//! share, a daily chart stacked by provider, token totals by type, and a
//! per-model breakdown. The numbers come from the CLIs' own transcripts
//! (`/usage/history`), priced at list rates, so cost reads as an API-price
//! estimate, not a bill.

use std::collections::BTreeMap;

use ironclaw_gpui_client::usage::{
    UsageBucket, UsageHistory, UsageHistorySource, UsagePricingStatus, UsageSourceStatus,
};
use rune::{Color, El, TextStyle, col, el, row};

use crate::state::State;
use super::common_stub::{LG_SEMIBOLD, SM, SM_MEDIUM, XS, label, pretty_model, rule};
use crate::design::{self, Palette, rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Limits,
    Cost,
    Tokens,
}

#[derive(Default, Clone, Copy)]
struct Sum {
    uncached: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    cost: f64,
    savings: f64,
    records: u32,
    unpriced: u32,
}

impl Sum {
    fn tokens(&self) -> u64 {
        self.uncached + self.cached + self.cache_write + self.output
    }

    fn value(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Cost => self.cost,
            _ => self.tokens() as f64,
        }
    }

    fn add(&mut self, b: &UsageBucket) {
        self.uncached += b.totals.uncached_input_tokens;
        self.cached += b.totals.cached_input_tokens;
        self.cache_write += b.totals.cache_creation_tokens;
        self.output += b.totals.output_tokens;
        self.cost += b.cost_usd;
        self.savings += b.cache_savings_usd;
        self.records += b.records;
        self.unpriced += b.unpriced_records;
    }
}

/// A fixed color per provider account, the same in every chart.
pub(super) fn provider_color(provider: &str) -> Color {
    match provider {
        "claude_cli" => rgb(0xD97757),
        "claude_cli_2" => rgb(0xA78BFA),
        "codex" => rgb(0x4C9AFF),
        _ => rgb(0x8E8E93),
    }
}

fn label_of<'a>(sources: &'a [UsageHistorySource], provider: &'a str) -> &'a str {
    sources
        .iter()
        .find(|s| s.provider == provider)
        .map_or(provider, |s| s.label.as_str())
}

pub fn format_tokens(n: u64) -> String {
    let n = n as f64;
    match n {
        x if x >= 1e9 => format!("{:.2}B", x / 1e9),
        x if x >= 1e6 => format!("{:.1}M", x / 1e6),
        x if x >= 1e3 => format!("{:.1}K", x / 1e3),
        x => format!("{x:.0}"),
    }
}

pub fn format_usd(v: f64) -> String {
    match v {
        x if x <= 0. => "$0".into(),
        x if x < 0.01 => "<$0.01".into(),
        x if x >= 1000. => format!("${:.0}", x),
        x => format!("${x:.2}"),
    }
}

fn format_value(v: f64, metric: Metric) -> String {
    match metric {
        Metric::Cost => format_usd(v),
        _ => format_tokens(v as u64),
    }
}

fn percent(part: f64, whole: f64) -> String {
    if whole <= 0. {
        return "0%".into();
    }
    let p = part / whole * 100.;
    if p > 0. && p < 1. {
        "<1%".into()
    } else {
        format!("{p:.0}%")
    }
}

fn dot(color: Color, size: f32) -> El {
    el().w(size).h(size).radius(size / 2.0).fill(color).a11y_hidden()
}

/// The Cost or Tokens view of `history`; `loading` dims the figure.
pub fn view(s: &State, history: &UsageHistory, metric: Metric, loading: bool) -> El {
    let p = &s.palette;
    let mut total = Sum::default();
    let mut by_provider: BTreeMap<&str, (Sum, u32)> = BTreeMap::new();
    let mut by_model: BTreeMap<(&str, &str), Sum> = BTreeMap::new();
    let mut by_day: BTreeMap<&str, BTreeMap<&str, Sum>> = BTreeMap::new();
    for b in &history.buckets {
        total.add(b);
        by_provider.entry(&b.provider).or_default().0.add(b);
        by_model.entry((&b.provider, &b.model)).or_default().add(b);
        by_day.entry(&b.day).or_default().entry(&b.provider).or_default().add(b);
    }
    for s in &history.sources {
        if let Some(entry) = by_provider.get_mut(s.provider.as_str()) {
            entry.1 = s.distinct_sessions;
        }
    }
    let sessions: u32 = history.sources.iter().map(|s| s.distinct_sessions).sum();
    let figure = format_value(total.value(metric), metric);

    if total.tokens() == 0 {
        return col()
            .w_full()
            .gap(12.0)
            .child(label("No token use in this period.", SM, p.text_muted))
            .child(sources_note(p, history));
    }

    // Overview: the figure and providers on the left, the chart beside.
    let mut providers: Vec<(&str, Sum, u32)> =
        by_provider.into_iter().map(|(k, (s, n))| (k, s, n)).collect();
    providers.sort_by(|a, b| b.1.value(metric).total_cmp(&a.1.value(metric)));
    let whole = total.value(metric);
    let caption = match metric {
        Metric::Cost if total.unpriced > 0 => format!(
            "{sessions} sessions · API estimate · {} unpriced",
            percent(f64::from(total.unpriced), f64::from(total.records))
        ),
        Metric::Cost => format!("{sessions} sessions · API estimate"),
        _ => format!("{sessions} sessions"),
    };
    let mut left = col().key("overview-left").w(260.0).gap(16.0).child(
        col()
            .gap(4.0)
            .child(
                label(figure, TextStyle::new(34.0, 600.0), p.text)
                    .opacity(if loading { 0.55 } else { 1.0 }),
            )
            .child(label(caption, XS, p.text_muted)),
    );
    for (provider, sum, n) in &providers {
        let share = percent(sum.value(metric), whole);
        let detail = match metric {
            Metric::Cost => format!("{share} of cost · {} tokens", format_tokens(sum.tokens())),
            _ => format!("{share} of tokens · {}", format_usd(sum.cost)),
        };
        left = left.child(
            col()
                .key(format!("share-{provider}"))
                .w_full()
                .gap(2.0)
                .child(
                    row()
                        .w_full()
                        .justify_between()
                        .center_y()
                        .gap(12.0)
                        .child(
                            row()
                                .gap(8.0)
                                .center_y()
                                .min_w(0.0)
                                .shrink()
                                .child(dot(provider_color(provider), 8.0))
                                .child(crate::design::brands::provider_logo(s, &[provider], 14.0))
                                .child(
                                    label(label_of(&history.sources, provider).to_string(), SM, p.text)
                                        .truncate()
                                        .shrink(),
                                )
                                .child(label(
                                    format!("{n} {}", if *n == 1 { "session" } else { "sessions" }),
                                    XS,
                                    p.text_faint,
                                )),
                        )
                        .child(label(format_value(sum.value(metric), metric), SM_MEDIUM, p.text)),
                )
                .child(label(detail, XS, p.text_muted)),
        );
    }

    let overview = row()
        .key("overview")
        .w_full()
        .gap(24.0)
        .items_start()
        .child(left)
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(12.0)
                .child(label(
                    match metric {
                        Metric::Cost => "Daily cost",
                        _ => "Daily processed tokens",
                    },
                    SM_MEDIUM,
                    p.text,
                ))
                .child(daily_chart(p, history, &by_day, metric)),
        );

    // Totals, t3code's metric row.
    let tile = |key: &str, title: &str, value: String| {
        col()
            .key(key.to_string())
            .grow(1.0)
            .min_w(110.0)
            .gap(4.0)
            .padding(12.0)
            .radius(design::PANEL_RADIUS)
            .border(1.0, p.border)
            .fill(p.surface_card)
            .child(label(title, XS, p.text_muted))
            .child(label(value, LG_SEMIBOLD, p.text))
    };
    let totals = col()
        .key("totals")
        .w_full()
        .gap(12.0)
        .child(label("Totals", SM_MEDIUM, p.text))
        .child(
            row()
                .w_full()
                .flex_wrap()
                .gap(12.0)
                .child(tile("tile-processed", "Processed tokens", format_tokens(total.tokens())))
                .child(tile("tile-cached", "Cached input", format_tokens(total.cached)))
                .child(tile("tile-uncached", "Uncached input", format_tokens(total.uncached)))
                .child(tile("tile-output", "Output", format_tokens(total.output)))
                .child(tile("tile-savings", "Cache savings", format_usd(total.savings))),
        )
        .child(share_bar(
            p,
            "Tokens by type",
            &[
                ("Uncached input", total.uncached as f64, rgb(0x4C9AFF)),
                ("Cached input", total.cached as f64, rgb(0x34C759)),
                ("Cache write", total.cache_write as f64, rgb(0xFFB020)),
                ("Output", total.output as f64, rgb(0xD97757)),
            ],
            |v| format_tokens(v as u64),
        ));

    // Breakdown by model, largest first.
    let mut models: Vec<((&str, &str), Sum)> = by_model.into_iter().collect();
    models.sort_by(|a, b| b.1.value(metric).total_cmp(&a.1.value(metric)));
    let peak = models.first().map_or(0., |m| m.1.value(metric));
    let right = |w: f32, t: El| row().w(w).justify_end().child(t);
    let mut table = col()
        .w_full()
        .radius(design::PANEL_RADIUS)
        .border(1.0, p.border)
        .fill(p.surface_card)
        .child(
            row()
                .key("breakdown-head")
                .w_full()
                .px(12.0)
                .py(8.0)
                .gap(12.0)
                .child(el().grow(1.0).child(label("Model", XS, p.text_faint)))
                .child(right(90.0, label("Cost", XS, p.text_faint)))
                .child(right(90.0, label("Tokens", XS, p.text_faint)))
                .child(el().w(120.0)),
        );
    for (ix, ((provider, model), sum)) in models.iter().enumerate() {
        let unpriced = sum.records > 0 && sum.unpriced == sum.records;
        let share = if peak > 0. { (sum.value(metric) / peak) as f32 } else { 0. };
        table = table.child(rule(p).key(format!("model-rule-{ix}"))).child(
            row()
                .key(format!("usage-model-{ix}"))
                .w_full()
                .px(12.0)
                .py(8.0)
                .gap(12.0)
                .center_y()
                .child(
                    row()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(8.0)
                        .center_y()
                        .child(dot(provider_color(provider), 8.0))
                        .child(label(pretty_model(model), SM, p.text).truncate().shrink())
                        .child(label(label_of(&history.sources, provider).to_string(), XS, p.text_faint)),
                )
                .child(right(
                    90.0,
                    if unpriced {
                        label("–", SM, p.text_faint)
                    } else {
                        label(format_usd(sum.cost), SM, p.text)
                    },
                ))
                .child(right(90.0, label(format_tokens(sum.tokens()), SM, p.text)))
                .child(
                    el().w(120.0)
                        .h(6.0)
                        .radius(3.0)
                        .fill(p.surface_raised)
                        .child(el().h_full().w_frac(share).radius(3.0).fill(provider_color(provider))),
                ),
        );
    }
    let breakdown = col()
        .key("breakdown")
        .w_full()
        .gap(12.0)
        .child(label("Breakdown", SM_MEDIUM, p.text))
        .child(table);

    let mut out = col().w_full().gap(32.0).child(overview).child(totals).child(breakdown);
    if let Some(c) = super::compactions::view(p, history) {
        out = out.child(c.key("compactions"));
    }
    out.child(sources_note(p, history).key("sources"))
}

/// One column per day of the period, stacked by provider; empty days keep
/// their slot so the axis reads as time.
fn daily_chart(
    p: &Palette,
    history: &UsageHistory,
    by_day: &BTreeMap<&str, BTreeMap<&str, Sum>>,
    metric: Metric,
) -> El {
    const HEIGHT: f32 = 160.;
    let days = period_days(&history.since_day, &history.until_day);
    let peak = days
        .iter()
        .filter_map(|d| by_day.get(d.as_str()))
        .map(|m| m.values().map(|s| s.value(metric)).sum::<f64>())
        .fold(0., f64::max);
    let mut bars = row()
        .key("chart")
        .w_full()
        .h(HEIGHT)
        .items_end()
        .gap(if days.len() > 40 { 1.0 } else { 3.0 });
    for (ix, day) in days.iter().enumerate() {
        let stack = by_day.get(day.as_str());
        let day_total: f64 = stack.map_or(0., |m| m.values().map(|s| s.value(metric)).sum());
        let mut column = col()
            .key(format!("usage-day-{ix}"))
            .grow(1.0)
            .min_w(2.0)
            .h_full()
            .justify_end()
            .clip_corners([3.0, 3.0, 0.0, 0.0])
            .help(format!("{day}: {}", format_value(day_total, metric)));
        if let Some(stack) = stack {
            // Largest provider at the bottom.
            let mut parts: Vec<(&str, f64)> = stack.iter().map(|(k, s)| (*k, s.value(metric))).collect();
            parts.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (provider, v) in parts {
                let h = if peak > 0. { (v / peak) as f32 * HEIGHT } else { 0. };
                column = column.child(
                    el().key(provider.to_string()).w_full().h(h).fill(provider_color(provider)),
                );
            }
        }
        bars = bars.child(column);
    }
    let first = days.first().cloned().unwrap_or_default();
    let last = days.last().cloned().unwrap_or_default();
    col()
        .w_full()
        .gap(4.0)
        .child(bars)
        .child(rule(p))
        .child(
            row()
                .w_full()
                .justify_between()
                .child(label(short_day(&first), XS, p.text_faint))
                .child(label(format!("peak {}", format_value(peak, metric)), XS, p.text_faint))
                .child(label(short_day(&last), XS, p.text_faint)),
        )
}

fn period_days(since: &str, until: &str) -> Vec<String> {
    let parse = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    let (Some(mut day), Some(end)) = (parse(since), parse(until)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while day <= end && out.len() < 400 {
        out.push(day.format("%Y-%m-%d").to_string());
        day = day.succ_opt().unwrap_or(end + chrono::Days::new(1));
    }
    out
}

fn short_day(day: &str) -> String {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map(|d| d.format("%-d %b").to_string())
        .unwrap_or_default()
}

/// A labelled bar split by share, with a legend underneath.
fn share_bar(p: &Palette, title: &str, parts: &[(&'static str, f64, Color)], format: impl Fn(f64) -> String) -> El {
    let whole: f64 = parts.iter().map(|x| x.1).sum();
    let mut bar = row()
        .key("share-bar")
        .w_full()
        .h(8.0)
        .radius(4.0)
        .clip_rounded(4.0)
        .fill(p.surface_raised);
    for (name, v, color) in parts.iter().filter(|x| x.1 > 0.) {
        let f = if whole > 0. { (*v / whole) as f32 } else { 0. };
        bar = bar.child(el().key(*name).h_full().w_frac(f).fill(*color));
    }
    let mut legend = row().w_full().flex_wrap().gap(16.0);
    for (name, v, color) in parts {
        legend = legend.child(
            row()
                .key(format!("legend-{name}"))
                .gap(6.0)
                .center_y()
                .child(dot(*color, 8.0))
                .child(label(format!("{name} {} · {}", format(*v), percent(*v, whole)), XS, p.text_muted)),
        );
    }
    col()
        .w_full()
        .gap(8.0)
        .child(label(title, XS, p.text_muted))
        .child(bar)
        .child(legend)
}

/// Where the prices came from and any source that could not be read.
fn sources_note(p: &Palette, history: &UsageHistory) -> El {
    let pricing = match history.pricing.status {
        UsagePricingStatus::Fresh | UsagePricingStatus::Cached => format!(
            "List prices from models.dev ({} models){}.",
            history.pricing.known_models,
            history
                .pricing
                .fetched_at
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|at| format!(", updated {}", crate::jobs::ago(at)))
                .unwrap_or_default()
        ),
        UsagePricingStatus::Unavailable => "Prices unavailable: costs show as unpriced.".into(),
    };
    let mut c = col().w_full().gap(4.0).child(
        label(format!("{pricing} Read from the CLIs' transcripts in {} ms.", history.scan_ms), XS, p.text_faint)
            .wrap(),
    );
    for (ix, s) in history.sources.iter().enumerate() {
        if s.status == UsageSourceStatus::Ok {
            continue;
        }
        let line = format!(
            "{}: {}",
            s.label,
            s.message.clone().unwrap_or_else(|| "could not be read".into())
        );
        c = c.child(label(line, XS, p.text_faint).wrap().key(format!("issue-{ix}")));
    }
    c
}

/// A deterministic `days`-day history for the headless scenes: two Claude
/// logins and Codex, a gap of idle days, two idle compactions and one
/// source that failed to read.
pub(super) fn fixture(days: u32) -> Result<UsageHistory, serde_json::Error> {
    let today = chrono::Utc::now().date_naive();
    let since = today - chrono::Days::new(u64::from(days.saturating_sub(1)));
    let mut buckets = Vec::new();
    for i in 0..days {
        // A quiet week in the middle of the period.
        if (12..17).contains(&i) {
            continue;
        }
        let day = (since + chrono::Days::new(u64::from(i))).format("%Y-%m-%d").to_string();
        let wave = 1.0 + ((i as f64) * 0.7).sin().abs() * 2.0;
        for (provider, model, scale, price) in [
            ("claude_cli", "claude-opus-4-5", 3.0, 1.8),
            ("claude_cli_2", "claude-sonnet-4-5", 1.4, 0.6),
            ("codex", "gpt-5.1-codex", 2.0, 0.4),
        ] {
            let k = wave * scale;
            buckets.push(serde_json::json!({
                "day": day, "provider": provider, "model": model,
                "totals": {
                    "uncachedInputTokens": (90_000.0 * k) as u64,
                    "cachedInputTokens": (1_400_000.0 * k) as u64,
                    "cacheCreationTokens": (120_000.0 * k) as u64,
                    "outputTokens": (40_000.0 * k) as u64,
                    "reasoningTokens": 0
                },
                "costUsd": price * k, "cacheSavingsUsd": price * k * 2.5,
                "costSource": "model_priced",
                "records": 40, "unpricedRecords": if provider == "codex" { 4 } else { 0 },
                "sessions": 3
            }));
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    serde_json::from_value(serde_json::json!({
        "sinceDay": since.format("%Y-%m-%d").to_string(),
        "untilDay": today.format("%Y-%m-%d").to_string(),
        "readAt": now, "scanMs": 412,
        "buckets": buckets,
        "sources": [
            { "provider": "claude_cli", "label": "Claude (work)", "status": "ok", "scannedFiles": 120, "distinctSessions": 48 },
            { "provider": "claude_cli_2", "label": "Claude (home)", "status": "ok", "scannedFiles": 40, "distinctSessions": 1 },
            { "provider": "codex", "label": "Codex", "status": "partial", "scannedFiles": 60, "distinctSessions": 22,
              "message": "3 transcripts could not be parsed" }
        ],
        "pricing": { "status": "fresh", "fetchedAt": now - 3_600_000, "knownModels": 412 },
        "compactions": [
            { "at": now - 2 * 3_600_000, "provider": "claude_cli", "threadId": "th_8f2a91c0d1",
              "model": "claude-opus-4-5", "tokensBefore": 182_000, "tokensAfter": 21_000,
              "costUsd": 0.42, "ok": true },
            { "at": now - 26 * 3_600_000, "provider": "claude_cli_2", "threadId": "th_31bb07e2aa",
              "model": "claude-sonnet-4-5", "tokensBefore": 96_000,
              "costUsd": 0.08, "ok": false, "error": "compact timed out" }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_like_t3code() {
        assert_eq!(format_tokens(1_234_567), "1.2M");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_usd(0.004), "<$0.01");
        assert_eq!(format_usd(12.345), "$12.35");
        assert_eq!(percent(0.5, 100.), "<1%");
        assert_eq!(period_days("2026-10-30", "2026-11-02").len(), 4);
    }

    #[test]
    fn the_fixture_decodes() {
        let h = fixture(30).expect("fixture decodes");
        assert_eq!(h.compactions.len(), 2);
        assert!(!h.buckets.is_empty());
    }
}
