//! The app's look beyond colour: wallpaper (painted scenes, pictures,
//! folders, effects), frosted panels, the battery saver and motion prefs
//! (GPUI `design/{wallpaper,battery,panels,paint,dock,flip,pulse,motion,
//! smooth_scroll,styles,brands}.rs`). Area G.
//!
//! FOUNDATION STUB.

#[derive(Default)]
pub struct LookState {}

use rune::{El, col, el};

use crate::design::Palette;
use crate::state::State;

// PORT-STUB(area G): the battery saver holds (background or idle window):
// polls skip (GPUI `BatterySaver::saving`).
pub fn saving(_s: &State) -> bool {
    false
}

// PORT-STUB(area G): user input reached the window (pointer or key), for the
// battery saver's idle clock (GPUI `BatterySaver::input`).
pub fn input(_s: &mut State) {}

// PORT-STUB(area G): the window became key or lost it (GPUI
// `BatterySaver::set_active`).
pub fn window_active(_s: &mut State, _active: bool) {}

// PORT-STUB(area G): the wallpaper layer over one area of the window when
// the person picked that area (`prefs.wallpaper_area`: "window",
// "conversation", "sidebar" or "pane"), absolutely placed over its parent
// (GPUI `Workspace::render_wallpaper` + `wallpaper::overlay`).
pub fn wallpaper(_s: &State, _area: &str) -> Option<El> {
    None
}

// PORT-STUB(area G): a new draft picks a new wallpaper from a folder (GPUI
// `wallpaper::reshuffle`).
pub fn reshuffle(_s: &mut State) {}

// PORT-STUB(area G): the main panel's fill alpha (GPUI `Style::panel_alpha`,
// below 1 when translucent).
pub fn panel_alpha(_s: &State) -> f32 {
    1.
}

// PORT-STUB(area G): the claw mark tile (GPUI `kit::claw_mark`): a 52 px
// accent tile with three strokes.
pub fn claw_mark(p: &Palette) -> El {
    let stroke = |h: f32, dy: f32| el().w(6.).h(h).mt(dy).radius(3.).fill(p.accent);
    rune::row()
        .size(52., 52.)
        .radius(14.)
        .fill(p.accent_wash)
        .center()
        .gap(5.)
        .child(col().child(stroke(18., 6.)))
        .child(col().child(stroke(24., 0.)))
        .child(col().child(stroke(18., 6.)))
}
