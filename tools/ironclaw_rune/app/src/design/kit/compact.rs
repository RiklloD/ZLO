//! Compact chat controls (model picker, goals, orchestration, computer
//! use): text at the GPUI app's sizes, icons, small ghost and variant
//! buttons, a tinted spinner and the composer-matched popover surface.
//! Moved here from `chat/extras_ui.rs`; the `⌘N` badge is [`super::shortcut_badge`].

use rune::{Color, El, Icon, Shadow, TextStyle, el, icon, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::design::{self, Palette};
use crate::state::State;

/// GPUI's `text_xs`, `text_sm` and the icon sizes `xsmall`, `small`.
pub const XS: f32 = 12.;
pub const SM: f32 = 14.;
pub const ICON_XS: f32 = 12.;
pub const ICON_SM: f32 = 14.;

pub fn txt(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s).font(TextStyle::new(size, 400.)).color(color)
}

pub fn txt_medium(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s).font(TextStyle::new(size, 500.)).color(color)
}

pub fn ico(i: &'static Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color)
}

/// A ghost icon-only button (GPUI `Button::icon(..).ghost().xsmall()`), with
/// a tooltip that is also its accessible name.
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

/// A ghost labelled button, optionally with a leading icon (GPUI
/// `Button::new(..).label(..).ghost().xsmall()`). `press` is dropped when
/// disabled.
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
    rune_kit::Spinner::new().size(size).color(color).build(k)
}

/// `a` mixed toward `b` by `t`, in OKLab.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let (x, y) = (a.to_oklab(), b.to_oklab());
    let l = |i: usize| x[i] + (y[i] - x[i]) * t;
    Color::from_oklab([l(0), l(1), l(2), l(3)])
}

/// The composer's fill and resting rim (GPUI `chat::composer_surface(p, 0., true)`).
pub fn composer_surface(p: &Palette) -> (Color, Color) {
    let ink = if p.light { Color::BLACK } else { Color::WHITE };
    let fill = if p.light {
        Color::WHITE.alpha(0.94)
    } else {
        mix(p.surface_card, p.bg, 0.25).alpha(0.96)
    };
    (fill, ink.alpha(0.07))
}

/// The popovers' surface: the composer's fill and rim, a soft drop.
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
        .fill(fill)
        .border(1., rim)
        .box_shadow(drop(a, 8., 24., -6.))
        .box_shadow(drop(b, 1., 3., 0.))
}

/// A fixed-width empty column (keeps labels still when a check appears).
pub fn spacer(w: f32) -> El {
    el().w(w).h(1.)
}
