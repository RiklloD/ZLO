//! The app's one look: geometry, materials and motion (GPUI
//! `design/styles.rs`). A theme only owns colour. The single knob is
//! translucency: on, macOS blurs the desktop through the sidebar and the
//! panels turn to frosted glass.
//!
//! GPUI kept `Style` as a global read by `Style::of(cx)`; here it is a small
//! `Copy` value derived from the preferences, [`Style::of`]`(&s.prefs)`,
//! cheap enough to build in every view run. Every widget in `kit` reads it
//! instead of hardcoding a radius or a duration.
//!
//! Vibrancy maps onto Rune's system materials: [`Style::material`] is the
//! `SystemMaterial` a translucent shell asks for (`El::system_material`),
//! drawn by the platform under the element in a transparent window. In an
//! opaque window and in headless renders Rune draws the element's fill
//! instead, which is why `shell_alpha`/`panel_alpha` still apply to fills.
//! A transparent window is a `WindowSpec`/`AppSpec` choice of the
//! foundation (see the area G report).

use rune::{Curve, Spring, SystemMaterial};

use super::motion::{EASE_OUT_QUINT, TAILWIND};
use super::prefs::Prefs;

/// Popovers and menus: a hair under critical, settles without a wobble
/// (GPUI `Spring::new(0.78, 300)`).
pub const POP: Spring = Spring::new(0.30, 0.08);

#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// Ask macOS for the blurred window backdrop (a system material).
    pub vibrancy: bool,
    /// Opacity of the shell and sidebar fill; below 1 shows the backdrop.
    pub shell_alpha: f32,
    /// Opacity of the content panel fill.
    pub panel_alpha: f32,
    pub panel_radius: f32,
    pub control_radius: f32,
    /// Sidebar and list rows. Sized to T3 Code's proportions.
    pub row_height: f32,
    /// The base text size (GPUI's rem). `prefs.ui_font_size` overrides it.
    pub font_size: f32,
    /// Panel and popover elevation, 0 for none.
    pub elevation: f32,
    /// Hover colour fade (`El::hover_motion`).
    pub hover: Curve,
    /// Page and thread entrances.
    pub enter: Spring,
    pub enter_rise: f32,
    /// Popovers and menus.
    pub pop: Spring,
    /// Sliding selection indicators and toggles.
    pub slide: Spring,
    /// The ease-out-quint curve `slide` approximates, for colour morphs.
    pub slide_curve: Curve,
}

impl Style {
    pub fn new(translucent: bool) -> Self {
        Self {
            vibrancy: translucent,
            shell_alpha: if translucent { 0.55 } else { 1. },
            panel_alpha: if translucent { 0.78 } else { 1. },
            panel_radius: super::PANEL_RADIUS,
            control_radius: super::CONTROL_RADIUS,
            row_height: super::ROW_HEIGHT,
            font_size: 16.,
            elevation: 1.,
            hover: Curve::tween(0.160, TAILWIND),
            enter: Spring::new(0.46, 0.0),
            enter_rise: 8.,
            pop: POP,
            slide: Spring::new(0.30, 0.0),
            slide_curve: Curve::tween(0.300, EASE_OUT_QUINT),
        }
    }

    /// The style the preferences ask for.
    pub fn of(prefs: &Prefs) -> Self {
        let mut s = Self::new(prefs.translucent);
        if prefs.ui_font_size > 0. {
            s.font_size = prefs.ui_font_size;
        }
        s
    }

    /// The system material a translucent shell (sidebar, window backdrop)
    /// sits on, or `None` when the window is opaque.
    pub fn material(&self) -> Option<SystemMaterial> {
        self.vibrancy.then_some(SystemMaterial::Popover)
    }

    /// Body text size, scaled from the 16 pt rem (GPUI `text_sm` = 14).
    pub fn text(&self, px_at_16: f32) -> f32 {
        (px_at_16 * self.font_size / 16.).round()
    }
}
