//! The app's look beyond colour: wallpaper (painted scenes, pictures,
//! folders, effects), frosted panels, the battery saver and motion prefs
//! (GPUI `design/{wallpaper,battery,panels,paint,dock,flip,pulse,motion,
//! smooth_scroll,styles,brands}.rs`). Area G.
//!
//! FOUNDATION STUB.

#[derive(Default)]
pub struct LookState {}

use crate::state::State;

// PORT-STUB(area G): GPUI `design::paint::SCENES`, painted wallpapers (id, name).
pub const SCENES: [(&str, &str); 3] = [("sky", "Sky"), ("dusk", "Dusk"), ("night", "Night")];

// PORT-STUB(area G): GPUI `design::paint::EFFECTS`, background effects (id, name).
pub const EFFECTS: [(&str, &str); 5] = [
    ("none", "None"),
    ("dither", "Dither"),
    ("halftone", "Halftone"),
    ("ascii", "ASCII"),
    ("scanlines", "Scanlines"),
];

// PORT-STUB(area G): GPUI `design::wallpaper::reload`, rebuild the
// wallpaper after its prefs changed.
pub fn reload_wallpaper(_s: &mut State) {}

// PORT-STUB(area G): GPUI `design::set_translucent`: save the pref, then
// switch the window backdrop (blurred or opaque).
pub fn set_translucent(s: &mut State, on: bool) {
    s.update_prefs(|p| p.translucent = on);
}

// PORT-STUB(area G): GPUI `Style::of(cx).font_size`, the style's own
// interface size when `Prefs::ui_font_size` is 0.
pub fn style_font_size(_s: &State) -> f32 {
    16.
}
