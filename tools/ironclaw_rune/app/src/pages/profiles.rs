//! Profiles and their pickers (GPUI `pages/profiles.rs`, `profiles/pickers.rs`).
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

use rune::{Cx, El, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct ProfilesState {}

/// The page became visible: load on first visit, refresh after.
pub fn opened(_s: &mut State) {}

pub fn view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .child(text("ProfilesState").color(s.palette.text_muted))
}

// PORT-STUB(area E): start a new profile (GPUI `ProfilesView::new_profile`),
// after `workspace::show(s, Page::Profiles)`.
pub fn new_profile(_s: &mut State) {}
