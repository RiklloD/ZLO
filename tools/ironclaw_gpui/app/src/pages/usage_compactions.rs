//! The Usage page's idle-compaction list: each `/compact` a Claude CLI
//! thread sent while idle, with the context it shrank and what it cost, so
//! the setting can be judged against the cache savings above it.

use ironclaw_gpui_client::usage::UsageHistory;

use super::usage_history::{format_tokens, format_usd};
use crate::chat::model_picker::pretty_model;
use crate::design::Palette;
use crate::jobs;
use crate::ui::*;

/// `None` when the period had no idle compaction.
pub fn render(history: &UsageHistory, cx: &App) -> Option<AnyElement> {
    if history.compactions.is_empty() {
        return None;
    }
    let p = *Palette::of(cx);
    let runs = &history.compactions;
    let total: f64 = runs.iter().map(|r| r.cost_usd).sum();
    let saved: u64 = runs
        .iter()
        .filter_map(|r| Some(r.tokens_before?.saturating_sub(r.tokens_after?)))
        .sum();
    let failed = runs.iter().filter(|r| !r.ok).count();
    let summary = format!(
        "{} {} · {} · {} tokens of context dropped{}",
        runs.len(),
        if runs.len() == 1 { "run" } else { "runs" },
        format_usd(total),
        format_tokens(saved),
        if failed > 0 {
            format!(" · {failed} failed")
        } else {
            String::new()
        },
    );
    let header_row = h_flex()
        .px_3()
        .py_2()
        .gap_3()
        .text_xs()
        .text_color(p.text_faint)
        .child(div().w(px(90.)).child("When"))
        .child(div().flex_1().child("Thread"))
        .child(div().w(px(140.)).text_right().child("Context"))
        .child(div().w(px(70.)).text_right().child("Cost"));
    let rows = runs.iter().enumerate().map(|(ix, run)| {
        let when = chrono::DateTime::from_timestamp_millis(run.at).map_or("–".into(), jobs::ago);
        let context = match (run.tokens_before, run.tokens_after) {
            (Some(before), Some(after)) => {
                format!("{} → {}", format_tokens(before), format_tokens(after))
            }
            (Some(before), None) => format!("{} → ?", format_tokens(before)),
            _ => "–".into(),
        };
        let thread: String = run.thread_id.chars().take(8).collect();
        let model = run.model.as_deref().map(pretty_model).unwrap_or_default();
        h_flex()
            .id(("usage-compaction", ix))
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .text_sm()
            .border_t_1()
            .border_color(p.border)
            .child(div().w(px(90.)).text_color(p.text_muted).child(when))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .child(div().truncate().child(model))
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.text_faint)
                            .flex_none()
                            .child(format!("{} · {thread}", run.provider)),
                    )
                    .when(!run.ok, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .flex_none()
                                .child(run.error.clone().unwrap_or_else(|| "failed".into())),
                        )
                    }),
            )
            .child(div().w(px(140.)).text_right().child(context))
            .child(
                div()
                    .w(px(70.))
                    .text_right()
                    .child(format_usd(run.cost_usd)),
            )
    });
    Some(
        v_flex()
            .gap_3()
            .child(div().text_sm().font_medium().child("Idle compactions"))
            .child(div().text_xs().text_color(p.text_muted).child(summary))
            .child(
                v_flex()
                    .rounded(px(crate::design::PANEL_RADIUS))
                    .border_1()
                    .border_color(p.border)
                    .bg(p.surface_card)
                    .child(header_row)
                    .children(rows),
            )
            .into_any_element(),
    )
}
