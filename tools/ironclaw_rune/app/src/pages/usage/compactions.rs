//! The Usage page's idle-compaction list (GPUI `pages/usage_compactions.rs`):
//! each `/compact` a Claude CLI thread sent while idle, with the context it
//! shrank and what it cost, so the setting can be judged against the cache
//! savings above it.

use ironclaw_gpui_client::usage::UsageHistory;
use rune::{El, col, el, row};

use super::common_stub::{SM, SM_MEDIUM, XS, label, rule};
use super::common_stub::pretty_model;
use super::history::{format_tokens, format_usd};
use crate::design::{self, Palette};
use crate::jobs;

/// `None` when the period had no idle compaction.
pub fn view(p: &Palette, history: &UsageHistory) -> Option<El> {
    if history.compactions.is_empty() {
        return None;
    }
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
        if failed > 0 { format!(" · {failed} failed") } else { String::new() },
    );
    let right = |w: f32, t: El| row().w(w).justify_end().child(t);
    let mut table = col()
        .w_full()
        .radius(design::PANEL_RADIUS)
        .border(1.0, p.border)
        .fill(p.surface_card)
        .child(
            row()
                .key("compactions-head")
                .w_full()
                .px(12.0)
                .py(8.0)
                .gap(12.0)
                .child(el().w(90.0).child(label("When", XS, p.text_faint)))
                .child(el().grow(1.0).child(label("Thread", XS, p.text_faint)))
                .child(right(140.0, label("Context", XS, p.text_faint)))
                .child(right(70.0, label("Cost", XS, p.text_faint))),
        );
    for (ix, run) in runs.iter().enumerate() {
        let when = chrono::DateTime::from_timestamp_millis(run.at).map_or("–".into(), jobs::ago);
        let context = match (run.tokens_before, run.tokens_after) {
            (Some(before), Some(after)) => format!("{} → {}", format_tokens(before), format_tokens(after)),
            (Some(before), None) => format!("{} → ?", format_tokens(before)),
            _ => "–".into(),
        };
        let thread: String = run.thread_id.chars().take(8).collect();
        let model = run.model.as_deref().map(pretty_model).unwrap_or_default();
        let mut middle = row()
            .grow(1.0)
            .min_w(0.0)
            .gap(8.0)
            .center_y()
            .child(label(model, SM, p.text).truncate().shrink())
            .child(label(format!("{} · {thread}", run.provider), XS, p.text_faint));
        if !run.ok {
            middle = middle.child(label(
                run.error.clone().unwrap_or_else(|| "failed".into()),
                XS,
                p.danger,
            ));
        }
        table = table.child(rule(p).key(format!("compaction-rule-{ix}"))).child(
            row()
                .key(format!("usage-compaction-{ix}"))
                .w_full()
                .px(12.0)
                .py(8.0)
                .gap(12.0)
                .center_y()
                .child(el().w(90.0).child(label(when, SM, p.text_muted)))
                .child(middle)
                .child(right(140.0, label(context, SM, p.text)))
                .child(right(70.0, label(format_usd(run.cost_usd), SM, p.text))),
        );
    }
    Some(
        col()
            .w_full()
            .gap(12.0)
            .child(label("Idle compactions", SM_MEDIUM, p.text))
            .child(label(summary, XS, p.text_muted))
            .child(table),
    )
}
