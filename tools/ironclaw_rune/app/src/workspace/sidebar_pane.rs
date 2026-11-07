//! The sidebar pane's state (the frame's `SplitView` resizes, folds and
//! peeks it) and its mirror in prefs.

use crate::state::State;

/// The sidebar pane as the prefs describe it: the saved width (0 is the
/// default) within the sidebar's range, open unless hidden.
pub fn sidebar_pane(prefs: &crate::design::prefs::Prefs) -> rune_kit::shell::PaneState {
    use crate::design::{SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, SIDEBAR_WIDTH};
    let mut pane =
        rune_kit::shell::PaneState::new(SIDEBAR_WIDTH).range(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
    if prefs.sidebar_width > 0. {
        pane = pane.with_width(prefs.sidebar_width);
    }
    if prefs.sidebar_hidden {
        pane = pane.collapsed();
    }
    pane
}

/// Shows or hides the sidebar outright (scenes, restores), prefs too.
pub fn set_sidebar_open(s: &mut State, open: bool) {
    s.workspace.sidebar.set_open(open);
    persist_sidebar(s);
}

/// Saves the sidebar pane's open state and width to prefs when they moved
/// (a toggle, a drag on its edge once released, a drag that folded it).
pub(super) fn persist_sidebar(s: &mut State) {
    let pane = s.workspace.sidebar;
    if pane.is_resizing() {
        return;
    }
    let saved = sidebar_pane(&s.prefs);
    if saved.is_open() == pane.is_open() && saved.width() == pane.width() {
        return;
    }
    let width = if pane.width() == pane.default_width() {
        0.
    } else {
        pane.width()
    };
    s.update_prefs(|p| {
        p.sidebar_hidden = !pane.is_open();
        p.sidebar_width = width;
    });
}
