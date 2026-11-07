//! Usage, usage history and compactions (GPUI `pages/usage*.rs`).
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

use rune::{Cx, El, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct UsageState {}

/// The page became visible: load on first visit, refresh after.
pub fn opened(_s: &mut State) {}

pub fn view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .child(text("UsageState").color(s.palette.text_muted))
}

/// The busiest usage window, for the status bar.
pub struct UsagePeak {
    pub provider: String,
    pub window: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

// PORT-STUB(area E): refresh the shared usage report (GPUI
// `UsageModel::refresh(force)`; a minute's cache upstream).
pub fn refresh_report(_s: &mut State, _force: bool) {}

// PORT-STUB(area E): the busiest window of the shared report (GPUI
// `UsageModel::peak`).
pub fn peak(_s: &State) -> Option<UsagePeak> {
    None
}

// PORT-STUB(area E): GPUI `pages::usage::short_countdown`: "2d 4h", "2h
// 14m", "14m"; `None` once past.
pub fn short_countdown(resets_at_ms: i64) -> Option<String> {
    let secs = (resets_at_ms - chrono::Utc::now().timestamp_millis()) / 1000;
    match secs {
        s if s <= 0 => None,
        s if s < 3600 => Some(format!("{}m", (s + 59) / 60)),
        s if s < 86_400 => Some(format!("{}h {}m", s / 3600, (s % 3600) / 60)),
        s => Some(format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)),
    }
}

// PORT-STUB(area E): GPUI `pages::usage::level_color`.
pub fn level_color(pct: f64, p: &crate::design::Palette) -> rune::Color {
    match pct {
        x if x >= 90. => p.danger,
        x if x >= 70. => p.warning,
        _ => p.success,
    }
}
