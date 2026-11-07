//! The chat's right pane and work panel: activity, agents, browser, changes, console, git, history, outputs, preview, project, routines, session (GPUI `chat/side.rs`, `chat/side/*`). Area S.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct SideState {}

use crate::state::State;

// PORT-STUB(area S): ⌥⌘B on an open thread (GPUI `ChatView::toggle_side`).
pub fn toggle_right_pane(s: &mut State) {
    s.update_prefs(|p| p.side_open = !p.side_open);
}

// PORT-STUB(area S): ⌘E on an open thread (GPUI `ChatView::toggle_work`).
pub fn toggle_work_panel(s: &mut State) {
    s.update_prefs(|p| p.work_open = !p.work_open);
}

// PORT-STUB(area S): ⌥⌘N (GPUI `ChatView::new_side_chat`).
pub fn new_side_chat(_s: &mut State) {}
