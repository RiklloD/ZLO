//! Notifications (GPUI `pages/notifications.rs`).
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

use rune::{Cx, El, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct NotificationsState {}

/// The page became visible: load on first visit, refresh after.
pub fn opened(_s: &mut State) {}

pub fn view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .child(text("NotificationsState").color(s.palette.text_muted))
}

// PORT-STUB(area D): the unread count changed; reload the list if it was
// ever opened (GPUI `NotificationsView::refresh`). The page writes
// `s.workspace.unread` itself where GPUI emitted `PageEvent::Unread`.
pub fn refresh(_s: &mut State) {}
