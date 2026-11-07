//! Memory and the memory draft (GPUI `pages/memory.rs`, `pages/memory_draft.rs`).
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

use rune::{Cx, El, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct MemoryState {}

/// The page became visible: load on first visit, refresh after.
pub fn opened(_s: &mut State) {}

pub fn view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .child(text("MemoryState").color(s.palette.text_muted))
}
