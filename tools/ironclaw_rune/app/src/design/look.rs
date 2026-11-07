//! The app's look beyond colour (area G): the wallpaper, the battery saver,
//! motion preferences and the provider logo cache, in one slot,
//! `State::look`. Widgets live in `design::kit`, geometry and materials in
//! `design::styles`, timings in `design::motion`, panel chrome in
//! `design::panels`, logos in `design::brands`.
//!
//! GPUI origins: `design/{wallpaper,battery,panels,paint,dock,flip,pulse,
//! motion,smooth_scroll,styles,brands,kit}.rs`, `assets.rs`, `ui.rs`.
//!
//! # Public API
//!
//! Lifecycle (wired in `main.rs`):
//! - `start(s)` once from `App::init`: logo cache, folder rotation timer.
//! - `poll(s, tx) -> bool` from the app's poll: battery saver idle/wake,
//!   repaint when the palette's brightness changed.
//! - `on_focus(s, focused, tx)` from `App::on_focus`.
//! - `apply_motion(s, cx)` at the top of every view run: sets
//!   `Cx::reduced_motion` from `prefs.reduce_motion`, the OS setting and the
//!   battery saver.
//!
//! For the workspace shell (area B):
//! - `wallpaper_layer(s, area) -> Option<El>` and
//!   `wallpaper_layer_rounded(s, area, radius) -> Option<El>`: call it for each
//!   of `"window"`, `"conversation"`, `"sidebar"`, `"pane"` and add the result
//!   as the LAST child of that area's container (it is pinned to the
//!   container's edges, inert, and lies over the content at the chosen
//!   opacity, as in GPUI). Only the area `prefs.wallpaper_area` names gets one.
//! - `reshuffle(s)` when a new chat opens (a wallpaper folder moves on).
//! - `saving(s) -> bool`: pollers skip their tick while the saver is engaged.
//!
//! For the Appearance page (area D):
//! - `reload(s)` after any wallpaper/theme preference change.
//! - `wallpaper::{AREAS, enabled}`, `paint::{SCENES, EFFECTS}` for the pickers.
//!
//! Fixtures: `fixture_wallpaper(s, size)` paints synchronously.

use rune::{Cx, El, Tx};

use super::battery::{self, BatterySaver};
use super::brands::Logos;
use super::wallpaper::{self, WallpaperState};
use crate::state::State;

#[derive(Default)]
pub struct LookState {
    pub wallpaper: WallpaperState,
    pub battery: BatterySaver,
    pub logos: Logos,
    /// A headless fixture's preview in place of the workspace (`gallery`).
    pub preview: Option<super::gallery::Preview>,
}

/// Once, from `App::init`, before the first frame.
pub fn start(s: &mut State) {
    let handle = s.rt.handle.clone();
    s.look.logos.start(std::sync::Arc::new(move || handle.wake()));
    wallpaper::rotate(s);
}

/// From the app's poll. Returns whether visible state changed.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    let changed = s.look.battery.poll(tx);
    if wallpaper::stale_for(s) {
        // The repaint lands on its own; nothing visible changes yet.
        wallpaper::reload(s);
    }
    changed
}

/// The window gained or lost focus (`App::on_focus`).
pub fn on_focus(s: &mut State, focused: bool, _tx: &mut Tx) {
    s.look.battery.set_active(focused);
}

/// Set this commit's reduced motion. Call it at the top of every view run
/// (Rune's override holds for one commit only).
pub fn apply_motion(s: &State, cx: &mut Cx<State>) {
    let reduced = battery::reduced(&s.prefs, &s.look.battery, cx.system_reduced_motion());
    cx.reduced_motion(reduced);
}

/// The battery saver is engaged (window in the background or idle for
/// 30 s). Pollers skip their tick while this holds.
pub fn saving(s: &State) -> bool {
    s.look.battery.saving()
}

/// Repaint the wallpaper after a preference or the palette changed.
pub fn reload(s: &mut State) {
    wallpaper::reload(s);
}

/// A new chat opened: a wallpaper folder moves on to another picture.
pub fn reshuffle(s: &mut State) {
    wallpaper::reshuffle(s);
}

/// The wallpaper over `area`, when the preferences put it there. See the
/// module docs for where it goes.
pub fn wallpaper_layer(s: &State, area: &str) -> Option<El> {
    wallpaper::layer(s, area)
}

/// [`wallpaper_layer`] with rounded corners, for a rounded panel.
pub fn wallpaper_layer_rounded(s: &State, area: &str, radius: f32) -> Option<El> {
    wallpaper::layer_rounded(s, area, radius)
}

/// Paint the wallpaper at `size` points now (headless fixtures drop async
/// replies, so the normal background paint never lands there).
pub fn fixture_wallpaper(s: &mut State, size: (u32, u32)) {
    wallpaper::paint_blocking(s, size);
}

/// Any input reached the window. The battery saver reads Rune's own input
/// clock (`rune::last_input`), so there is nothing to record; kept so the
/// workspace's input hooks have one place to report to.
pub fn input(_s: &mut State) {}
