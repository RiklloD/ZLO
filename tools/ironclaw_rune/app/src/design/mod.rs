//! Design system: one palette, layout numbers, fonts and the rune-kit `Kit`
//! built from them. Ported from the GPUI app's `design/mod.rs`; the colour
//! derivation (`Palette::from_seed`) is the same, in Rune's `Color`.
//!
//! The palette is resolved from `State::prefs` with [`resolve`] and kept in
//! `State::palette`; every view reads it from there. [`kit`] maps it onto
//! rune-kit's semantic tokens, so kit components (Button, Menu, Select,
//! TextInput, Markdown, ...) paint in the same colours as our own elements.

pub mod battery;
pub mod brands;
pub mod gallery;
pub mod kit;
pub mod look;
pub mod motion;
pub mod paint;
pub mod panels;
pub mod prefs;
pub mod smooth_scroll;
pub mod styles;
pub mod themes;
pub mod wallpaper;

use rune::Color;
use rune_kit::{Kit, PaletteSeed};

// Layout numbers. Colors are paint; these drive layout and never depend on it.
pub const SIDEBAR_WIDTH: f32 = 296.;
/// Bounds for the dragged sidebar width.
pub const SIDEBAR_MIN_WIDTH: f32 = 220.;
pub const SIDEBAR_MAX_WIDTH: f32 = 520.;
pub const TITLEBAR_HEIGHT: f32 = rune_kit::shell::TOOLBAR_HEIGHT;
/// Room the macOS traffic lights take at the sidebar's top left.
pub const TRAFFIC_LIGHTS_WIDTH: f32 = 86.;
pub const STATUS_BAR_HEIGHT: f32 = 24.;
pub const SIDEBAR_CARD_MIN_HEIGHT: f32 = 72.;
pub const SIDEBAR_SLIM_HEIGHT: f32 = 42.;
pub const SIDEBAR_SEARCH_HEIGHT: f32 = 44.;
pub const SIDEBAR_TITLE_SIZE: f32 = 15.;
pub const SIDEBAR_META_SIZE: f32 = 13.;
pub const SIDEBAR_CONTROL: f32 = 32.;
// Radius scale, after T3 Code's design pass: 4/6/8/12/16. Controls 6, rows
// 8, cards and popovers 12, composer and bubbles 16. Nothing off the scale.
pub const PANEL_RADIUS: f32 = 12.;
pub const CONTROL_RADIUS: f32 = 6.;
pub const ROW_RADIUS: f32 = 8.;
pub const BUBBLE_RADIUS: f32 = 16.;
pub const ROW_HEIGHT: f32 = 34.;

pub const FONT_SANS: &str = "Geist";
pub const FONT_MONO: &str = "Geist Mono";

/// `AppSpec::fonts`: Geist and Geist Mono (OFL, `assets/fonts`).
pub const FONTS: &[&[u8]] = &[
    include_bytes!("../../assets/fonts/Geist.ttf"),
    include_bytes!("../../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/GeistMono.ttf"),
];
/// `AppSpec::families`: (sans, mono).
pub const FAMILIES: (&str, &str) = (FONT_SANS, FONT_MONO);

/// Paint tokens, in `State::palette`.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub light: bool,
    /// Main content panel, the deepest plane.
    pub bg: Color,
    /// Shell and sidebar, one step up from `bg`.
    pub surface: Color,
    /// Opaque pills and chips.
    pub surface_raised: Color,
    pub surface_card: Color,
    pub surface_overlay: Color,
    pub hover: Color,
    pub active: Color,
    pub border: Color,
    pub border_strong: Color,
    pub text: Color,
    pub text_muted: Color,
    pub text_faint: Color,
    /// Label color on solid status fills (danger, warning, success).
    pub on_solid: Color,
    pub accent: Color,
    pub accent_strong: Color,
    pub accent_wash: Color,
    pub danger: Color,
    pub warning: Color,
    pub success: Color,
}

impl Palette {
    /// Every token from a preset's eleven seed colors.
    pub fn from_seed(t: &themes::ThemeSeed) -> Self {
        let accent = rgb(t.accent);
        let accent_strong = scale_lightness(accent, 0.85);
        let text = rgb(t.text);
        let light = t.is_light();
        // Lines are ink on light paper and light on dark glass.
        let line = if light { Color::BLACK } else { Color::WHITE };
        Self {
            light,
            bg: rgb(t.bg),
            surface: rgb(t.shell),
            surface_raised: rgb(t.raised),
            surface_card: rgb(t.card),
            surface_overlay: if light {
                rgb(t.card)
            } else {
                lift(rgb(t.card), 0.035)
            },
            hover: text.alpha(if light { 0.055 } else { 0.07 }),
            active: text.alpha(if light { 0.085 } else { 0.11 }),
            border: line.alpha(if light { 0.09 } else { 0.08 }),
            border_strong: line.alpha(if light { 0.16 } else { 0.14 }),
            text,
            text_muted: rgb(t.muted),
            text_faint: rgb(t.faint),
            on_solid: rgb(t.card),
            accent,
            accent_strong,
            accent_wash: accent_strong.alpha(if light { 0.12 } else { 0.18 }),
            danger: rgb(t.danger),
            warning: rgb(t.warning),
            success: rgb(t.success),
        }
    }

    /// Label color on an accent fill: dark ink on bright accents.
    pub fn on_accent(&self) -> Color {
        on_fill(self.accent)
    }
}

/// Whether the scheme resolves to dark, given the system appearance
/// (`Cx::system_dark`, or `Env::system_dark` in `poll`).
pub fn scheme_is_dark(prefs: &prefs::Prefs, system_dark: bool) -> bool {
    match prefs.scheme.as_str() {
        "light" => false,
        "system" => system_dark,
        _ => true,
    }
}

/// The palette the preferences ask for: the scheme's theme, then the accent
/// preset over it.
pub fn resolve(prefs: &prefs::Prefs, system_dark: bool) -> Palette {
    let dark = scheme_is_dark(prefs, system_dark);
    let seed = if dark {
        themes::find_for(&prefs.theme, false)
    } else {
        themes::find_for(&prefs.light_theme, true)
    };
    let mut p = Palette::from_seed(seed);
    if let Some((_, _, on_dark, on_light)) =
        themes::ACCENTS.iter().find(|(id, ..)| *id == prefs.accent)
    {
        p.accent = rgb(if p.light { *on_light } else { *on_dark });
        p.accent_strong = scale_lightness(p.accent, 0.85);
        p.accent_wash = p.accent_strong.alpha(if p.light { 0.12 } else { 0.18 });
    }
    p
}

/// rune-kit's tokens from our palette, so kit components match our own
/// elements. Build it once per view run (it is cheap) and pass `&Kit` down.
pub fn kit(p: &Palette) -> Kit {
    let mut k = Kit::from_palette(PaletteSeed {
        bg: p.bg,
        fg: p.text,
        accent: p.accent,
    });
    let c = &mut k.c;
    c.text_base = p.text;
    c.text_muted = p.text_muted;
    c.text_faint = p.text_faint;
    c.text_accent = p.accent;
    c.bg_accent = p.accent;
    // GPUI's kit primary is an accent fill: rune-kit's Submit and Contrast
    // buttons (and contrast icon buttons, accent badges) read these tokens,
    // which `from_palette` sets to the inverse ink (a white plate on dark).
    let on_accent = p.on_accent();
    c.bg_contrast = p.accent;
    c.bg_icon_button_contrast = p.accent;
    c.text_contrast = on_accent;
    c.icon_contrast = on_accent;
    c.submit_fg = on_accent;
    c.submit_icon = on_accent;
    k.radius.sm = CONTROL_RADIUS;
    // rune-kit's components (buttons, inputs, menus, selects) follow
    // Appearance → UI font size with the app's own text; code stays put.
    let ty = &mut k.ty;
    for style in [
        &mut ty.ui,
        &mut ty.ui_medium,
        &mut ty.ui_compact,
        &mut ty.small,
        &mut ty.small_medium,
        &mut ty.body,
        &mut ty.body_medium,
        &mut ty.text_small,
        &mut ty.text_small_medium,
        &mut ty.text_xsmall,
        &mut ty.text_xsmall_medium,
    ] {
        *style = styles::scaled(*style);
    }
    k
}

/// `0xRRGGBB` as a Rune colour.
pub fn rgb(rgb: u32) -> Color {
    rune::color::hex(&format!("#{rgb:06x}")).unwrap_or(Color::BLACK)
}

/// Lighter by `by` of OKLab lightness (0..1).
pub fn lift(c: Color, by: f32) -> Color {
    let [l, a, b, alpha] = c.to_oklab();
    Color::from_oklab([(l + by).min(1.), a, b, alpha])
}

/// OKLab lightness times `k` (the GPUI app's `l * 0.85` on HSL).
pub fn scale_lightness(c: Color, k: f32) -> Color {
    let [l, a, b, alpha] = c.to_oklab();
    Color::from_oklab([l * k, a, b, alpha])
}

/// Dark ink on bright fills, white on dark ones.
pub fn on_fill(fill: Color) -> Color {
    if fill.luminance() > 0.4 {
        rgb(0x0f0f10)
    } else {
        Color::WHITE
    }
}
