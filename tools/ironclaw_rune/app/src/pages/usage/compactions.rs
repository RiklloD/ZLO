//! The Usage page's idle-compaction list (GPUI `pages/usage_compactions.rs`):
//! each `/compact` a Claude CLI thread sent while idle, with the context it
//! shrank and what it cost, so the setting can be judged against the cache
//! savings above it.

use crate::chat::model_picker::pretty_model;
use crate::pages::common::controls::{rule, styled};
use crate::pages::common::ts::{sm, sm_medium, xs};
use ironclaw_gpui_client::usage::UsageHistory;
use rune::{El, col, el, row};

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
        if failed > 0 {
            format!(" · {failed} failed")
        } else {
            String::new()
        },
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
                .child(el().w(90.0).child(styled("When", xs(), p.text_faint)))
                .child(el().grow(1.0).child(styled("Thread", xs(), p.text_faint)))
                .child(right(140.0, styled("Context", xs(), p.text_faint)))
                .child(right(70.0, styled("Cost", xs(), p.text_faint))),
        );
    for (ix, run) in runs.iter().enumerate() {
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
        let mut middle = row()
            .grow(1.0)
            .min_w(0.0)
            .gap(8.0)
            .center_y()
            .child(styled(model, sm(), p.text).truncate().shrink())
            .child(styled(
                format!("{} · {thread}", run.provider),
                xs(),
                p.text_faint,
            ));
        if !run.ok {
            middle = middle.child(styled(
                run.error.clone().unwrap_or_else(|| "failed".into()),
                xs(),
                p.danger,
            ));
        }
        table = table
            .child(rule(p).key(format!("compaction-rule-{ix}")))
            .child(
                row()
                    .key(format!("usage-compaction-{ix}"))
                    .w_full()
                    .px(12.0)
                    .py(8.0)
                    .gap(12.0)
                    .center_y()
                    .child(el().w(90.0).child(styled(when, sm(), p.text_muted)))
                    .child(middle)
                    .child(right(140.0, styled(context, sm(), p.text)))
                    .child(right(70.0, styled(format_usd(run.cost_usd), sm(), p.text))),
            );
    }
    Some(
        col()
            .w_full()
            .gap(12.0)
            .child(styled("Idle compactions", sm_medium(), p.text))
            .child(styled(summary, xs(), p.text_muted))
            .child(table),
    )
}
