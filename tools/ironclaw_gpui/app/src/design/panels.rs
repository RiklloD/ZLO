//! Side panel chrome, after Zeron's shell (MIT): the sidebar glyph whose
//! inner panel morphs as a column opens or closes, the titlebar button that
//! carries it, and the reveal clock both columns slide on.
//!
//! A column stays laid out at its full width while a clipping wrapper
//! grows or shrinks around it, so nothing rewraps mid-slide, and a second
//! click reverses from the drawn width (`motion::state_t`).

use gpui_kit::{
    App, Div, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::motion::{self, EASE, MotionSpec};
use super::{Palette, Style};

/// Zeron Icons' state morph: 280 ms on `cubic-bezier(.22, 1, .36, 1)`.
pub const GLYPH: MotionSpec = MotionSpec::new(280, motion::EASE_OUT_QUINT);
/// A column opening or closing: Zeron's panel clock (CSS `ease`) at its
/// longest stretch, since a sidebar is always a wide move.
pub const REVEAL: MotionSpec = MotionSpec::new(240, EASE);
/// Titlebar button target and its glyph.
pub const TOGGLE_SIZE: f32 = 28.;
const GLYPH_SIZE: f32 = 16.;
/// Where the titlebar cluster ends, measured from the window's left edge.
pub const CLUSTER_END: f32 = super::TRAFFIC_LIGHTS_WIDTH + TOGGLE_SIZE;

/// Eased 0..1 reveal of the column `key`, 1 when shown.
pub fn reveal(key: &str, open: bool, cx: &App) -> f32 {
    motion::state_t(key, open, REVEAL, cx.reduce_motion())
}

/// The sidebar glyph (Zeron Icons "Sidebar" / "Right sidebar"): a rounded
/// frame holding a panel whose width runs 1.75 → 5.5 units as `open` goes
/// 0 → 1. gpui SVGs are static, so it is quads in the icon's 24-unit space.
pub fn glyph(open: f32, right: bool, size: f32, color: Hsla) -> Div {
    let s = size / 24.;
    // `<rect x=3 y=4 w=18 h=16 rx=4 stroke-width=1.75>`; a centered stroke
    // grows the box by half the stroke on each side.
    let stroke = 1.75;
    let frame = div()
        .absolute()
        .left(px((3. - stroke / 2.) * s))
        .top(px((4. - stroke / 2.) * s))
        .w(px((18. + stroke) * s))
        .h(px((16. + stroke) * s))
        .rounded(px((4. + stroke / 2.) * s))
        .border(px(stroke * s))
        .border_color(color);
    let inset = px(6.5 * s);
    let panel = div()
        .absolute()
        .top(px(7.5 * s))
        .w(px(motion::lerp(1.75, 5.5, open.clamp(0., 1.)) * s))
        .h(px(9. * s))
        .rounded(px(0.875 * s))
        .bg(color);
    let panel = if right {
        panel.right(inset)
    } else {
        panel.left(inset)
    };
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(frame)
        .child(panel)
}

/// Zeron's session-card glyph: the sidebar frame with a panel in only its
/// top-right corner, which grows a little while the card is open.
pub fn card_glyph(open: f32, size: f32, color: Hsla) -> Div {
    let s = size / 24.;
    let stroke = 1.75;
    let frame = div()
        .absolute()
        .left(px((3. - stroke / 2.) * s))
        .top(px((4. - stroke / 2.) * s))
        .w(px((18. + stroke) * s))
        .h(px((16. + stroke) * s))
        .rounded(px((4. + stroke / 2.) * s))
        .border(px(stroke * s))
        .border_color(color);
    let side = motion::lerp(4.5, 6., open.clamp(0., 1.));
    let corner = div()
        .absolute()
        .top(px(7.5 * s))
        .right(px(6.5 * s))
        .w(px(side * s))
        .h(px(side * s))
        .rounded(px(1. * s))
        .bg(color);
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(frame)
        .child(corner)
}

/// The titlebar button for a column: a soft hover wash and the morphing
/// glyph. The caller adds the click.
pub fn toggle(
    id: &'static str,
    open: bool,
    right: bool,
    label: &'static str,
    cx: &App,
) -> Stateful<Div> {
    toggle_with(id, open, label, cx, move |morph, color| {
        glyph(morph, right, GLYPH_SIZE, color).into_any_element()
    })
}

/// [`toggle`] around any glyph: `draw(morph, ink)` paints it for the
/// 0..1 open state and the hover-mixed ink.
pub fn toggle_with(
    id: &'static str,
    open: bool,
    label: &'static str,
    cx: &App,
    draw: impl FnOnce(f32, Hsla) -> gpui_kit::AnyElement + 'static,
) -> Stateful<Div> {
    let p = *Palette::of(cx);
    let reduced = cx.reduce_motion();
    let morph = motion::state_t(&format!("{id}:glyph"), open, GLYPH, reduced);
    let key = SharedString::from(format!("{id}:hover"));
    let hover = motion::hover_t(&key);
    let wash = if p.light {
        gpui_kit::hsla(0., 0., 0., 0.07)
    } else {
        gpui_kit::hsla(0., 0., 1., 0.09)
    };
    div()
        .id(id)
        .aria_label(label)
        .size(px(TOGGLE_SIZE))
        .flex_none()
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .active(|s| s.opacity(0.7))
        .on_hover(motion::hover_listener(key, Style::of(cx).hover))
        .bg(wash.opacity(wash.a * hover))
        .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(label).build(w, cx))
        .child(draw(morph, motion::mix(p.text_muted, p.text, hover)))
}

/// The glyph size [`toggle_with`] callers draw at.
pub const GLYPH_PX: f32 = GLYPH_SIZE;
