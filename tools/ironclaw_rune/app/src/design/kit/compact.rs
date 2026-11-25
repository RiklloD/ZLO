//! Compact chat controls (model picker, goals, orchestration, computer
//! use): text at the app's small sizes, icons, small ghost and variant
//! buttons, a tinted spinner and the composer-matched popover surface.
//! Compact chat-extras widgets; the `⌘N` badge is [`super::shortcut_badge`].

use rune::{Color, El, Icon, Shadow, TextStyle, el, icon, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::design::styles::rem;
use crate::design::{self, Palette};
use crate::state::State;

/// The extra-small and small text and icon sizes.
pub const XS: f32 = 12.;
pub const SM: f32 = 14.;
pub const ICON_XS: f32 = 12.;
pub const ICON_SM: f32 = 14.;

/// A line at `size` (at the 16 px base, scaled to the UI font size).
pub fn txt(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s).font(TextStyle::new(rem(size), 400.)).color(color)
}

pub fn txt_medium(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s).font(TextStyle::new(rem(size), 500.)).color(color)
}

pub fn ico(i: &'static Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color)
}

/// A ghost icon-only button, with a tooltip that is also its accessible name.
pub fn ghost_icon(
    k: &Kit,
    key: impl Into<String>,
    i: &'static Icon,
    tip: &str,
    press: impl Fn(&mut State) + 'static,
) -> El {
    IconButton::new(i)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(tip)
        .help(tip)
        .on_press(press)
        .build(k)
        .key(key)
}

/// [`ghost_icon`] whose press also gets the transaction.
pub fn ghost_icon_tx(
    k: &Kit,
    key: impl Into<String>,
    i: &'static Icon,
    tip: &str,
    press: impl Fn(&mut State, &mut rune::Tx) + 'static,
) -> El {
    IconButton::new(i)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(tip)
        .help(tip)
        .on_press_tx(press)
        .build(k)
        .key(key)
}

/// A ghost labelled button, optionally with a leading icon. `press` is dropped
/// when disabled.
pub fn ghost_button(
    k: &Kit,
    key: impl Into<String>,
    label: &str,
    i: Option<&'static Icon>,
    disabled: bool,
    press: impl Fn(&mut State) + 'static,
) -> El {
    variant_button(k, key, label, i, ButtonVariant::Ghost, disabled, press)
}

pub fn variant_button(
    k: &Kit,
    key: impl Into<String>,
    label: &str,
    i: Option<&'static Icon>,
    variant: ButtonVariant,
    disabled: bool,
    press: impl Fn(&mut State) + 'static,
) -> El {
    let mut b = Button::new(label)
        .variant(variant)
        .control_size(ControlSize::Small)
        .disabled(disabled);
    if let Some(i) = i {
        b = b.icon(i);
    }
    b.on_press(press).build(k).key(key)
}

/// A small spinner in `color`.
pub fn spinner(k: &Kit, size: f32, color: Color) -> El {
    crate::design::kit::Spinner::new()
        .size(size)
        .color(color)
        .build(k)
}

/// The composer's fill and resting rim (`chat::composer::composer_surface`),
/// so a popover over the composer reads as part of it.
pub fn composer_surface(p: &Palette) -> (Color, Color) {
    crate::chat::composer::composer_surface(p, false)
}

/// The popovers' surface: the composer's fill and rim, a soft drop, on
/// continuous corners. It pops in and out in place (`motion::pop(0.)`)
/// where its commit animates; inside `rune_kit::popover::anchored` (which
/// lands its contents at once and fades them out) the anchor's motion wins.
pub fn panel_surface(p: &Palette, e: El) -> El {
    let (fill, rim) = composer_surface(p);
    let drop = |a: f32, y: f32, blur: f32, spread: f32| Shadow {
        x: 0.,
        y,
        blur,
        spread,
        color: Color::BLACK.alpha(a),
        inset: false,
    };
    let (a, b) = if p.light { (0.08, 0.04) } else { (0.35, 0.2) };
    e.radius(design::PANEL_RADIUS)
        .continuous_corners()
        .transition(design::motion::pop(0.))
        .fill(fill)
        .border(1., rim)
        .box_shadow(drop(a, 8., 24., -6.))
        .box_shadow(drop(b, 1., 3., 0.))
}

/// A fixed-width empty column (keeps labels still when a check appears).
pub fn spacer(w: f32) -> El {
    el().w(w).h(1.)
}
