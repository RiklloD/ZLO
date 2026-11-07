//! Providers: model providers, keys and the catalog (GPUI `pages/providers.rs`, `providers/catalog.rs`).
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

use rune::{Cx, El, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct ProvidersState {}

/// The page became visible: load on first visit, refresh after.
pub fn opened(_s: &mut State) {}

pub fn view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .child(text("ProvidersState").color(s.palette.text_muted))
}

// PORT-STUB(area C): the thread the orchestrator catalog (CLI agents) is
// read through (GPUI `ProvidersView::set_catalog_thread`).
pub fn set_catalog_thread(_s: &mut State, _thread: Option<String>) {}
