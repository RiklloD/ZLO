//! Side panel chrome, after Zeron's shell (MIT), ported from the GPUI app's
//! `design/panels.rs`: the sidebar glyph whose inner panel morphs as a
//! column opens or closes, the titlebar button that carries it, and the
//! reveal a column slides on.
//!
//! GPUI drove the morph and the slide from its own tween store
//! (`motion::state_t`). Here they are layout: the glyph's inner panel and the
//! column's clipping wrapper are keyed elements whose width springs on
//! [`GLYPH`] / [`REVEAL`] when `open` flips, and a second click reverses from
//! the drawn width (Rune retargets keep position and velocity).
//!
//! Public API:
//! - [`glyph`]`(key, open, right, size, color) -> El`
//! - [`card_glyph`]`(key, open, size, color) -> El`
//! - [`toggle`]`(s, key, open, right, label) -> El` (caller adds `.on_press`)
//! - [`toggle_with`]`(s, key, label, glyph) -> El`
//! - [`column`]`(key, open, width, right, child) -> El`
//! - consts [`GLYPH`], [`REVEAL`], [`TOGGLE_SIZE`], [`GLYPH_PX`], [`CLUSTER_END`]

use rune::{Color, El, Spring, el};

use super::styles::Style;
use crate::state::State;

/// Zeron Icons' state morph: 280 ms on `cubic-bezier(.22, 1, .36, 1)`.
pub const GLYPH: Spring = Spring::new(0.28, 0.0);
/// A column opening or closing: Zeron's panel clock (CSS `ease`, 240 ms).
pub const REVEAL: Spring = Spring::new(0.24, 0.0);
/// Titlebar button target and its glyph.
pub const TOGGLE_SIZE: f32 = 28.;
const GLYPH_SIZE: f32 = 16.;
/// The glyph size [`toggle_with`] callers draw at.
pub const GLYPH_PX: f32 = GLYPH_SIZE;
/// Where the titlebar cluster ends, measured from the window's left edge.
pub const CLUSTER_END: f32 = super::TRAFFIC_LIGHTS_WIDTH + TOGGLE_SIZE;

/// The rounded frame both glyphs share: `<rect x=3 y=4 w=18 h=16 rx=4
/// stroke-width=1.75>`; a centred stroke grows the box by half the stroke on
/// each side.
fn frame(s: f32, color: Color) -> El {
    let stroke = 1.75;
    el().absolute((3. - stroke / 2.) * s, (4. - stroke / 2.) * s)
        .size((18. + stroke) * s, (16. + stroke) * s)
        .radius((4. + stroke / 2.) * s)
        .border(stroke * s, color)
}

/// The sidebar glyph (Zeron Icons "Sidebar" / "Right sidebar"): a rounded
/// frame holding a panel whose width runs 1.75 → 5.5 units as the column
/// opens. `key` must be unique on screen; the panel springs on [`GLYPH`].
pub fn glyph(key: &str, open: bool, right: bool, size: f32, color: Color) -> El {
    let s = size / 24.;
    let inset = 6.5 * s;
    let w = if open { 5.5 } else { 1.75 } * s;
    let panel = el()
        .key(format!("{key}-panel"))
        .w(w)
        .h(9. * s)
        .radius(0.875 * s)
        .fill(color)
        .motion(GLYPH);
    let panel = if right {
        panel.pin(None, Some(7.5 * s), Some(inset), None)
    } else {
        panel.pin(Some(inset), Some(7.5 * s), None, None)
    };
    el().key(key.to_string())
        .size(size, size)
        
        .a11y_hidden()
        .child(frame(s, color))
        .child(panel)
}

/// Zeron's session-card glyph: the sidebar frame with a panel in only its
/// top-right corner, which grows a little while the card is open.
pub fn card_glyph(key: &str, open: bool, size: f32, color: Color) -> El {
    let s = size / 24.;
    let side = if open { 6. } else { 4.5 } * s;
    el().key(key.to_string())
        .size(size, size)
        
        .a11y_hidden()
        .child(frame(s, color))
        .child(
            el().key(format!("{key}-corner"))
                .pin(None, Some(7.5 * s), Some(6.5 * s), None)
                .size(side, side)
                .radius(s)
                .fill(color)
                .motion(GLYPH),
        )
}

/// The titlebar button for a column: a soft hover wash, the hover-mixed ink
/// and the morphing glyph, with `label` as tooltip and accessible name. The
/// caller adds `.on_press(..)`.
pub fn toggle(s: &State, key: &str, open: bool, right: bool, label: &str) -> El {
    let p = &s.palette;
    let g = glyph(&format!("{key}-glyph"), open, right, GLYPH_SIZE, p.text_muted)
        .group_color(p.text);
    toggle_with(s, key, label, g)
}

/// [`toggle`] around any glyph (draw it in `p.text_muted` with
/// `.group_color(p.text)` so it brightens with the hover).
pub fn toggle_with(s: &State, key: &str, label: &str, glyph: El) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let wash = if p.light {
        Color::BLACK.alpha(0.07)
    } else {
        Color::WHITE.alpha(0.09)
    };
    el().key(key.to_string())
        .role("button")
        .label(label.to_string())
        .help(label.to_string())
        .size(TOGGLE_SIZE, TOGGLE_SIZE)
        
        .radius(7.)
        .center()
        .cursor(rune::Cursor::Pointer)
        .hover(wash)
        .press(wash)
        .hover_motion(st.hover, st.hover)
        .hover_group()
        .no_window_drag()
        .child(glyph)
}

/// A column that slides open or shut: a clipping wrapper whose width
/// springs between 0 and `width` on [`REVEAL`] while `child` stays laid out
/// at its full width, so nothing rewraps mid-slide. `right` anchors the
/// child to the wrapper's right edge (a right pane slides in from the
/// window's edge).
pub fn column(key: &str, open: bool, width: f32, right: bool, child: El) -> El {
    let inner = el().key(format!("{key}-inner")).w(width).h_full().child(child);
    let inner = if right {
        inner.pin(None, Some(0.), Some(0.), Some(0.))
    } else {
        inner.pin(Some(0.), Some(0.), None, Some(0.))
    };
    el().key(key.to_string())
        .w(if open { width } else { 0. })
        .h_full()
        
        .clip_soft(0.)
        .motion(REVEAL)
        .child(inner)
}
