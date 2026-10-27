//! The app's one look: geometry, materials and motion. A theme only owns
//! color. The single knob is translucency: on, macOS blurs the desktop
//! through the sidebar and the panels turn to frosted glass.
//!
//! Every component in `kit` reads [`Style::of`] instead of hardcoding a
//! radius or a duration.

use gpui_kit::{App, Global};

use super::motion::{CubicBezier, EASE_OUT_EXPO, EASE_OUT_QUINT, MotionSpec, Spring};

#[derive(Clone, Copy)]
pub struct Style {
    /// Ask macOS for the blurred window backdrop (NSVisualEffectView).
    pub vibrancy: bool,
    /// Opacity of the shell and sidebar fill; below 1 shows the backdrop.
    pub shell_alpha: f32,
    /// Opacity of the content panel fill.
    pub panel_alpha: f32,
    pub panel_radius: f32,
    pub control_radius: f32,
    /// Sidebar and list rows. Sized to T3 Code's proportions (UX_REFERENCE.md).
    pub row_height: f32,
    /// The rem: gpui-component sizes text and spacing from it, so 16 gives
    /// Tailwind's scale (text-sm 14, text-xs 12, p-4 16).
    pub font_size: f32,
    /// Panel and popover elevation, 0 for none.
    pub elevation: f32,
    /// Hover color fade.
    pub hover: MotionSpec,
    /// Page and thread entrances.
    pub enter: MotionSpec,
    pub enter_rise: f32,
    /// Popovers and menus.
    pub pop: Spring,
    /// Sliding selection indicators and toggles.
    pub slide: MotionSpec,
}

impl Global for Style {}

impl Style {
    pub fn of(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn new(translucent: bool) -> Self {
        Self {
            vibrancy: translucent,
            shell_alpha: if translucent { 0.55 } else { 1. },
            panel_alpha: if translucent { 0.78 } else { 1. },
            panel_radius: super::PANEL_RADIUS,
            control_radius: super::CONTROL_RADIUS,
            row_height: 34.,
            font_size: 16.,
            elevation: 1.,
            hover: MotionSpec::new(160, TAILWIND),
            enter: MotionSpec::new(460, EASE_OUT_EXPO),
            enter_rise: 8.,
            // A hair under critical: popovers settle without a wobble.
            pop: Spring::new(0.78, 300),
            slide: MotionSpec::new(300, EASE_OUT_QUINT),
        }
    }
}

/// Tailwind's `transition-colors` curve.
const TAILWIND: CubicBezier = CubicBezier::new(0.4, 0., 0.2, 1.);
