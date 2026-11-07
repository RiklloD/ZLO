//! Settings: General, Appearance, Permissions, Traces and their sections (GPUI `pages/settings.rs`, `settings/*` except `voice.rs`, which the voice port owns as `voice::settings_view`).
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

use rune::{Cx, El, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct SettingsState {}

/// The page became visible: load on first visit, refresh after.
pub fn opened(_s: &mut State) {}

pub fn view(s: &State, _page: crate::state::Page, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .child(text("SettingsState").color(s.palette.text_muted))
}
