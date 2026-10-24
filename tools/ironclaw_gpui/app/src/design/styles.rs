//! Design styles: four complete looks for the same app. A style owns the
//! geometry, the materials, the type and the motion; a theme preset only
//! owns color. Picking a style also loads its signature palette, and the
//! palette can still be changed afterwards.
//!
//! Every component in `kit` reads [`Style::of`] instead of hardcoding a
//! radius or a duration, so a fifth style is one more constructor here.

use gpui_kit::{App, Global};

use super::motion::{CubicBezier, EASE, EASE_OUT_EXPO, EASE_OUT_QUINT, MotionSpec, Spring};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StyleId {
    Forge,
    Glass,
    Paper,
    Console,
}

impl StyleId {
    pub const ALL: [StyleId; 4] = [Self::Forge, Self::Glass, Self::Paper, Self::Console];

    pub fn key(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::Glass => "glass",
            Self::Paper => "paper",
            Self::Console => "console",
        }
    }

    pub fn parse(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|s| s.key() == key)
            .unwrap_or(Self::Forge)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Forge => "Forge",
            Self::Glass => "Glass",
            Self::Paper => "Paper",
            Self::Console => "Console",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Self::Forge => "Graphite and ember. Machined buttons, firm motion.",
            Self::Glass => "The desktop shows through. Frosted panels, springy motion.",
            Self::Paper => "Light and quiet. Ink on paper, soft shadows.",
            Self::Console => "Monospace and hairlines. Dense, instant.",
        }
    }

    /// The palette preset a style loads when picked.
    pub fn theme(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::Glass => "glass",
            Self::Paper => "paper",
            Self::Console => "console",
        }
    }
}

/// How raised controls are painted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Material {
    /// Vertical gradient, hairline rim, a lit top edge and a short drop.
    Metal,
    /// Translucent white over the backdrop, a bright rim.
    Frost,
    /// Opaque fill and a soft ambient shadow, no rim.
    Ink,
    /// Flat fill and a 1 px line, nothing else.
    Line,
}

#[derive(Clone, Copy)]
pub struct Style {
    pub id: StyleId,
    pub material: Material,
    /// Ask macOS for the blurred window backdrop (NSVisualEffectView).
    pub vibrancy: bool,
    /// Opacity of the shell and sidebar fill; below 1 shows the backdrop.
    pub shell_alpha: f32,
    /// Opacity of the content panel fill.
    pub panel_alpha: f32,
    pub panel_inset: f32,
    pub panel_radius: f32,
    pub control_radius: f32,
    /// Sidebar and list rows. Sized to T3 Code's proportions (UX_REFERENCE.md).
    pub row_height: f32,
    /// Monospace for the whole interface, not only code.
    pub mono_ui: bool,
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

    pub fn get(id: StyleId) -> Self {
        match id {
            StyleId::Forge => Self {
                id,
                material: Material::Metal,
                vibrancy: false,
                shell_alpha: 1.,
                panel_alpha: 1.,
                panel_inset: 6.,
                panel_radius: 10.,
                control_radius: 7.,
                row_height: 34.,
                mono_ui: false,
                font_size: 16.,
                elevation: 0.6,
                hover: MotionSpec::new(140, TAILWIND),
                enter: MotionSpec::new(420, EASE_OUT_EXPO),
                enter_rise: 6.,
                pop: Spring::new(0.82, 260),
                slide: MotionSpec::new(260, EASE_OUT_QUINT),
            },
            StyleId::Glass => Self {
                id,
                material: Material::Frost,
                vibrancy: true,
                shell_alpha: 0.42,
                panel_alpha: 0.62,
                panel_inset: 8.,
                panel_radius: 16.,
                control_radius: 10.,
                row_height: 36.,
                mono_ui: false,
                font_size: 16.,
                elevation: 1.,
                hover: MotionSpec::new(180, TAILWIND),
                enter: MotionSpec::new(520, EASE_OUT_EXPO),
                enter_rise: 10.,
                // Under-damped: popovers overshoot by a hair and settle.
                pop: Spring::new(0.62, 420),
                slide: MotionSpec::new(340, EASE_OUT_QUINT),
            },
            StyleId::Paper => Self {
                id,
                material: Material::Ink,
                vibrancy: false,
                shell_alpha: 1.,
                panel_alpha: 1.,
                panel_inset: 8.,
                panel_radius: 12.,
                control_radius: 8.,
                row_height: 34.,
                mono_ui: false,
                font_size: 16.,
                elevation: 0.8,
                hover: MotionSpec::new(200, EASE),
                enter: MotionSpec::new(360, EASE_OUT_QUINT),
                enter_rise: 3.,
                pop: Spring::new(1., 220),
                slide: MotionSpec::new(300, EASE_OUT_QUINT),
            },
            StyleId::Console => Self {
                id,
                material: Material::Line,
                vibrancy: false,
                shell_alpha: 1.,
                panel_alpha: 1.,
                panel_inset: 0.,
                panel_radius: 0.,
                control_radius: 3.,
                row_height: 30.,
                mono_ui: true,
                font_size: 15.,
                elevation: 0.,
                hover: MotionSpec::new(60, EASE),
                enter: MotionSpec::new(120, EASE),
                enter_rise: 0.,
                pop: Spring::new(1., 110),
                slide: MotionSpec::new(120, EASE),
            },
        }
    }
}

/// Tailwind's `transition-colors` curve.
const TAILWIND: CubicBezier = CubicBezier::new(0.4, 0., 0.2, 1.);
