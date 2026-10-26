//! The Usage page's Cost and Tokens views, after t3code's usage overview:
//! the period's total and each provider's share, a daily chart stacked by
//! provider, token totals by type, and a per-model breakdown. The numbers
//! come from the CLIs' own transcripts (`/usage/history`), priced at list
//! rates, so cost reads as an API-price estimate, not a bill.

use std::collections::BTreeMap;

use ironclaw_gpui_client::usage::{
    UsageHistory, UsageHistorySource, UsagePricingStatus, UsageSourceStatus,
};

use crate::chat::model_picker::pretty_model;
use crate::design::{Palette, brands};
use crate::ui::*;

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

    fn add(&mut self, b: &ironclaw_gpui_client::usage::UsageBucket) {
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
fn provider_color(provider: &str) -> gpui_kit::Hsla {
    match provider {
        "claude_cli" => gpui_kit::rgb(0xD97757).into(),
        "claude_cli_2" => gpui_kit::rgb(0xA78BFA).into(),
        "codex" => gpui_kit::rgb(0x4C9AFF).into(),
        _ => gpui_kit::rgb(0x8E8E93).into(),
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

pub fn render(history: &UsageHistory, metric: Metric, loading: bool, cx: &App) -> AnyElement {
    let p = *Palette::of(cx);
    let mut total = Sum::default();
    let mut by_provider: BTreeMap<&str, (Sum, u32)> = BTreeMap::new();
    let mut by_model: BTreeMap<(&str, &str), Sum> = BTreeMap::new();
    let mut by_day: BTreeMap<&str, BTreeMap<&str, Sum>> = BTreeMap::new();
    for b in &history.buckets {
        total.add(b);
        by_provider.entry(&b.provider).or_default().0.add(b);
        by_model.entry((&b.provider, &b.model)).or_default().add(b);
        by_day
            .entry(&b.day)
            .or_default()
            .entry(&b.provider)
            .or_default()
            .add(b);
    }
    for s in &history.sources {
        if let Some(entry) = by_provider.get_mut(s.provider.as_str()) {
            entry.1 = s.distinct_sessions;
        }
    }
    let sessions: u32 = history.sources.iter().map(|s| s.distinct_sessions).sum();
    let figure = format_value(total.value(metric), metric);
    let dim = |d: gpui_kit::Div| if loading { d.opacity(0.55) } else { d };

    if total.tokens() == 0 {
        return v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(p.text_muted)
                    .child("No token use in this period."),
            )
            .child(sources_note(history, cx))
            .into_any_element();
    }

    // Overview: the figure and providers on the left, the chart beside.
    let mut providers: Vec<(&str, Sum, u32)> = by_provider
        .into_iter()
        .map(|(k, (s, n))| (k, s, n))
        .collect();
    providers.sort_by(|a, b| b.1.value(metric).total_cmp(&a.1.value(metric)));
    let whole = total.value(metric);
    let left = v_flex()
        .w(px(260.))
        .flex_none()
        .gap_4()
        .child(
            v_flex()
                .gap_1()
                .child(dim(div()
                    .text_size(px(34.))
                    .font_semibold()
                    .text_color(p.text)
                    .child(figure)))
                .child(
                    div()
                        .text_xs()
                        .text_color(p.text_muted)
                        .child(match metric {
                            Metric::Cost if total.unpriced > 0 => format!(
                                "{sessions} sessions · API estimate · {} unpriced",
                                percent(f64::from(total.unpriced), f64::from(total.records))
                            ),
                            Metric::Cost => format!("{sessions} sessions · API estimate"),
                            _ => format!("{sessions} sessions"),
                        }),
                ),
        )
        .children(providers.iter().map(|(provider, sum, n)| {
            let share = percent(sum.value(metric), whole);
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .justify_between()
                        .items_center()
                        .gap_3()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .min_w_0()
                                .child(
                                    div()
                                        .size(px(8.))
                                        .rounded_full()
                                        .flex_none()
                                        .bg(provider_color(provider)),
                                )
                                .child(brands::provider_logo(&[provider], 14., cx))
                                .child(
                                    div()
                                        .text_sm()
                                        .truncate()
                                        .child(label_of(&history.sources, provider).to_string()),
                                )
                                .child(div().text_xs().text_color(p.text_faint).flex_none().child(
                                    format!("{n} {}", if *n == 1 { "session" } else { "sessions" }),
                                )),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .flex_none()
                                .child(format_value(sum.value(metric), metric)),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.text_muted)
                        .child(match metric {
                            Metric::Cost => {
                                format!("{share} of cost · {} tokens", format_tokens(sum.tokens()))
                            }
                            _ => format!("{share} of tokens · {}", format_usd(sum.cost)),
                        }),
                )
        }));

    let chart = daily_chart(history, &by_day, metric, cx);
    let overview = h_flex().gap_6().items_start().child(left).child(
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_3()
            .child(div().text_sm().font_medium().child(match metric {
                Metric::Cost => "Daily cost",
                _ => "Daily processed tokens",
            }))
            .child(chart),
    );

    // Totals, t3code's metric row.
    let tile = |label: &'static str, value: String| {
        v_flex()
            .flex_1()
            .min_w(px(110.))
            .gap_1()
            .p_3()
            .rounded(px(crate::design::PANEL_RADIUS))
            .border_1()
            .border_color(p.border)
            .bg(p.surface_card)
            .child(div().text_xs().text_color(p.text_muted).child(label))
            .child(div().text_lg().font_semibold().child(value))
    };
    let totals = v_flex()
        .gap_3()
        .child(div().text_sm().font_medium().child("Totals"))
        .child(
            h_flex()
                .flex_wrap()
                .gap_3()
                .child(tile("Processed tokens", format_tokens(total.tokens())))
                .child(tile("Cached input", format_tokens(total.cached)))
                .child(tile("Uncached input", format_tokens(total.uncached)))
                .child(tile("Output", format_tokens(total.output)))
                .child(tile("Cache savings", format_usd(total.savings))),
        )
        .child(share_bar(
            "Tokens by type",
            &[
                (
                    "Uncached input",
                    total.uncached as f64,
                    gpui_kit::rgb(0x4C9AFF).into(),
                ),
                (
                    "Cached input",
                    total.cached as f64,
                    gpui_kit::rgb(0x34C759).into(),
                ),
                (
                    "Cache write",
                    total.cache_write as f64,
                    gpui_kit::rgb(0xFFB020).into(),
                ),
                (
                    "Output",
                    total.output as f64,
                    gpui_kit::rgb(0xD97757).into(),
                ),
            ],
            |v| format_tokens(v as u64),
            cx,
        ));

    // Breakdown by model, largest first.
    let mut models: Vec<((&str, &str), Sum)> = by_model.into_iter().collect();
    models.sort_by(|a, b| b.1.value(metric).total_cmp(&a.1.value(metric)));
    let peak = models.first().map_or(0., |m| m.1.value(metric));
    let header_row = h_flex()
        .px_3()
        .py_2()
        .gap_3()
        .text_xs()
        .text_color(p.text_faint)
        .child(div().flex_1().child("Model"))
        .child(div().w(px(90.)).text_right().child("Cost"))
        .child(div().w(px(90.)).text_right().child("Tokens"))
        .child(div().w(px(120.)));
    let rows = models
        .iter()
        .enumerate()
        .map(|(ix, ((provider, model), sum))| {
            let unpriced = sum.records > 0 && sum.unpriced == sum.records;
            h_flex()
                .id(("usage-model", ix))
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .text_sm()
                .border_t_1()
                .border_color(p.border)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .size(px(8.))
                                .rounded_full()
                                .flex_none()
                                .bg(provider_color(provider)),
                        )
                        .child(div().truncate().child(pretty_model(model)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.text_faint)
                                .flex_none()
                                .child(label_of(&history.sources, provider).to_string()),
                        ),
                )
                .child(
                    div()
                        .w(px(90.))
                        .text_right()
                        .when(unpriced, |d| d.text_color(p.text_faint))
                        .child(if unpriced {
                            "–".to_string()
                        } else {
                            format_usd(sum.cost)
                        }),
                )
                .child(
                    div()
                        .w(px(90.))
                        .text_right()
                        .child(format_tokens(sum.tokens())),
                )
                .child(
                    div()
                        .w(px(120.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(p.surface_raised)
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(provider_color(provider))
                                .w(gpui_kit::relative(if peak > 0. {
                                    (sum.value(metric) / peak) as f32
                                } else {
                                    0.
                                })),
                        ),
                )
        });
    let breakdown = v_flex()
        .gap_3()
        .child(div().text_sm().font_medium().child("Breakdown"))
        .child(
            v_flex()
                .rounded(px(crate::design::PANEL_RADIUS))
                .border_1()
                .border_color(p.border)
                .bg(p.surface_card)
                .child(header_row)
                .children(rows),
        );

    v_flex()
        .gap_8()
        .child(overview)
        .child(totals)
        .child(breakdown)
        .children(super::usage_compactions::render(history, cx))
        .child(sources_note(history, cx))
        .into_any_element()
}

/// One column per day of the period, stacked by provider; empty days keep
/// their slot so the axis reads as time.
fn daily_chart(
    history: &UsageHistory,
    by_day: &BTreeMap<&str, BTreeMap<&str, Sum>>,
    metric: Metric,
    cx: &App,
) -> impl IntoElement {
    let p = *Palette::of(cx);
    const HEIGHT: f32 = 160.;
    let days = period_days(&history.since_day, &history.until_day);
    let peak = days
        .iter()
        .filter_map(|d| by_day.get(d.as_str()))
        .map(|m| m.values().map(|s| s.value(metric)).sum::<f64>())
        .fold(0., f64::max);
    let columns = days.iter().enumerate().map(|(ix, day)| {
        let stack = by_day.get(day.as_str());
        let day_total: f64 = stack.map_or(0., |m| m.values().map(|s| s.value(metric)).sum());
        let mut col = v_flex()
            .id(("usage-day", ix))
            .flex_1()
            .min_w(px(2.))
            .h_full()
            .justify_end()
            .rounded_t(px(3.))
            .overflow_hidden()
            .tooltip({
                let text = format!("{day}: {}", format_value(day_total, metric));
                move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
                }
            });
        if let Some(stack) = stack {
            // Largest provider at the bottom.
            let mut parts: Vec<(&str, f64)> =
                stack.iter().map(|(k, s)| (*k, s.value(metric))).collect();
            parts.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (provider, v) in parts {
                let h = if peak > 0. {
                    (v / peak) as f32 * HEIGHT
                } else {
                    0.
                };
                col = col.child(
                    div()
                        .w_full()
                        .h(px(h))
                        .flex_none()
                        .bg(provider_color(provider)),
                );
            }
        }
        col
    });
    let first = days.first().cloned().unwrap_or_default();
    let last = days.last().cloned().unwrap_or_default();
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .h(px(HEIGHT))
                .items_end()
                .gap(px(if days.len() > 40 { 1. } else { 3. }))
                .border_b_1()
                .border_color(p.border)
                .children(columns),
        )
        .child(
            h_flex()
                .justify_between()
                .text_xs()
                .text_color(p.text_faint)
                .child(short_day(&first))
                .child(format!("peak {}", format_value(peak, metric)))
                .child(short_day(&last)),
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
fn share_bar(
    label: &'static str,
    parts: &[(&'static str, f64, gpui_kit::Hsla)],
    format: impl Fn(f64) -> String,
    cx: &App,
) -> impl IntoElement {
    let p = *Palette::of(cx);
    let whole: f64 = parts.iter().map(|x| x.1).sum();
    v_flex()
        .gap_2()
        .child(div().text_xs().text_color(p.text_muted).child(label))
        .child(
            h_flex()
                .h(px(8.))
                .w_full()
                .rounded_full()
                .overflow_hidden()
                .bg(p.surface_raised)
                .children(parts.iter().filter(|x| x.1 > 0.).map(|(_, v, color)| {
                    div()
                        .h_full()
                        .w(gpui_kit::relative(if whole > 0. {
                            (*v / whole) as f32
                        } else {
                            0.
                        }))
                        .bg(*color)
                })),
        )
        .child(
            h_flex()
                .flex_wrap()
                .gap_4()
                .text_xs()
                .text_color(p.text_muted)
                .children(parts.iter().map(|(name, v, color)| {
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(div().size(px(8.)).rounded_full().bg(*color))
                        .child(format!("{name} {} · {}", format(*v), percent(*v, whole)))
                })),
        )
}

/// Where the prices came from and any source that could not be read.
fn sources_note(history: &UsageHistory, cx: &App) -> impl IntoElement {
    let p = *Palette::of(cx);
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
    let issues = history.sources.iter().filter_map(|s| match s.status {
        UsageSourceStatus::Ok => None,
        _ => Some(format!(
            "{}: {}",
            s.label,
            s.message
                .clone()
                .unwrap_or_else(|| "could not be read".into())
        )),
    });
    v_flex()
        .gap_1()
        .text_xs()
        .text_color(p.text_faint)
        .child(format!(
            "{pricing} Read from the CLIs' transcripts in {} ms.",
            history.scan_ms
        ))
        .children(issues.map(|line| div().child(line)))
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
}
