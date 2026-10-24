//! Design system: one palette, layout numbers, fonts, motion and the battery
//! saver. Colors and helpers are adapted from Zeron (MIT, see
//! `THIRD_PARTY_NOTICES.md`).
//!
//! `install` writes the palette into gpui-kit's theme, so every existing
//! screen and component repaints from it. New code reads [`Palette`] for the
//! tokens gpui-kit has no slot for (raised surfaces, hairlines, washes).
//! A [`styles::Style`] sits beside the palette and owns geometry, material
//! and motion; light palettes (by background lightness) get dark ink lines.

pub mod battery;
pub mod brands;
pub mod dock;
pub mod kit;
pub mod motion;
pub mod prefs;
pub mod pulse;
pub mod styles;
pub mod themes;
pub mod wallpaper;

use std::borrow::Cow;

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, hsla, px};

// Layout numbers. Colors are paint; these drive layout and never depend on it.
pub const SIDEBAR_WIDTH: f32 = 296.;
/// Bounds for the dragged sidebar width.
pub const SIDEBAR_MIN_WIDTH: f32 = 220.;
pub const SIDEBAR_MAX_WIDTH: f32 = 520.;
pub const TITLEBAR_HEIGHT: f32 = 46.;
/// Room the macOS traffic lights take at the sidebar's top left.
pub const TRAFFIC_LIGHTS_WIDTH: f32 = 86.;
pub const STATUS_BAR_HEIGHT: f32 = 24.;
// Sidebar density: 72 px cards with 15 px titles and 13 px meta, 32 px
// controls.
pub const SIDEBAR_CARD_MIN_HEIGHT: f32 = 72.;
pub const SIDEBAR_SLIM_HEIGHT: f32 = 42.;
pub const SIDEBAR_SEARCH_HEIGHT: f32 = 44.;
pub const SIDEBAR_TITLE_SIZE: f32 = 15.;
pub const SIDEBAR_META_SIZE: f32 = 13.;
pub const SIDEBAR_CONTROL: f32 = 32.;
pub const PANEL_RADIUS: f32 = 10.;
pub const CONTROL_RADIUS: f32 = 6.;

pub const FONT_SANS: &str = "Geist";
pub const FONT_MONO: &str = "Geist Mono";

/// Paint tokens. Read with [`Palette::of`].
#[derive(Clone, Copy)]
pub struct Palette {
    pub light: bool,
    /// Main content panel, the deepest plane.
    pub bg: Hsla,
    /// Shell and sidebar, one step up from `bg`.
    pub surface: Hsla,
    /// Opaque pills and chips.
    pub surface_raised: Hsla,
    pub surface_card: Hsla,
    pub surface_overlay: Hsla,
    pub hover: Hsla,
    pub active: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    /// Label color on solid status fills (danger, warning, success).
    pub on_solid: Hsla,
    pub accent: Hsla,
    pub accent_strong: Hsla,
    pub accent_wash: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
}

impl Global for Palette {}

impl Palette {
    /// Every token from a preset's eleven seed colors.
    pub fn from_seed(t: &themes::ThemeSeed) -> Self {
        let accent = hex(t.accent);
        let accent_strong = Hsla {
            l: accent.l * 0.85,
            ..accent
        };
        let text = hex(t.text);
        let light = hex(t.bg).l > 0.5;
        // Lines are ink on light paper and light on dark glass.
        let line = if light { 0. } else { 1. };
        Self {
            light,
            bg: hex(t.bg),
            surface: hex(t.shell),
            surface_raised: hex(t.raised),
            surface_card: hex(t.card),
            surface_overlay: if light {
                hex(t.card)
            } else {
                lift(hex(t.card), 0.035)
            },
            hover: text.opacity(if light { 0.055 } else { 0.07 }),
            active: text.opacity(if light { 0.085 } else { 0.11 }),
            border: hsla(0., 0., line, if light { 0.09 } else { 0.08 }),
            border_strong: hsla(0., 0., line, if light { 0.16 } else { 0.14 }),
            text,
            text_muted: hex(t.muted),
            text_faint: hex(t.faint),
            on_solid: hex(t.card),
            accent,
            accent_strong,
            accent_wash: accent_strong.opacity(if light { 0.12 } else { 0.18 }),
            danger: hex(t.danger),
            warning: hex(t.warning),
            success: hex(t.success),
        }
    }

    pub fn of(cx: &App) -> &Self {
        cx.global::<Self>()
    }
}

/// Load the fonts and paint gpui-kit's theme from the palette. Call once,
/// right after `gpui_kit::init`.
pub fn install(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../../assets/fonts/Geist.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/Geist-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/Geist-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/GeistMono.ttf")),
    ];
    let have_fonts = match cx.text_system().add_fonts(fonts) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("bundled fonts failed to load, using system fonts: {e}");
            false
        }
    };

    cx.set_global(prefs::Prefs::load());
    cx.set_global(HaveFonts(have_fonts));
    apply_theme(cx, have_fonts);
}

struct HaveFonts(bool);
impl Global for HaveFonts {}

/// Switch design style: loads its geometry, motion and signature palette.
pub fn set_style(id: styles::StyleId, cx: &mut App) {
    prefs::Prefs::update(cx, |p| {
        p.style = id.key().into();
        // The style's signature palette goes in the slot of its brightness
        // and the scheme follows, so picking Paper shows Paper.
        let seed = themes::find(id.theme());
        if seed.is_light() {
            p.light_theme = id.theme().into();
            p.scheme = "light".into();
        } else {
            p.theme = id.theme().into();
            p.scheme = "dark".into();
        }
        // Paper and Console read best on a bare backdrop. A picture the
        // person picked stays.
        if !matches!(p.wallpaper, prefs::Wallpaper::Image { .. }) {
            p.wallpaper = match id {
                styles::StyleId::Forge | styles::StyleId::Glass => prefs::Wallpaper::Aurora,
                styles::StyleId::Paper | styles::StyleId::Console => prefs::Wallpaper::None,
            };
        }
    });
    let fonts = cx.try_global::<HaveFonts>().is_none_or(|f| f.0);
    apply_theme(cx, fonts);
    for window in cx.windows() {
        let _ = window.update(cx, |_, window, _| apply_window(window, Style::get(id)));
    }
}

/// The window backdrop a style asks for. Call when a window opens and when
/// the style changes.
pub fn apply_window(window: &mut gpui_kit::Window, style: Style) {
    window.set_background_appearance(if style.vibrancy {
        gpui_kit::WindowBackgroundAppearance::Blurred
    } else {
        gpui_kit::WindowBackgroundAppearance::Opaque
    });
}

pub use styles::Style;

/// Switch preset: repaint gpui-kit's theme and rebuild the wallpaper.
pub fn set_theme(id: &str, cx: &mut App) {
    let id = id.to_string();
    let light = themes::find(&id).is_light();
    prefs::Prefs::update(cx, |p| {
        if light {
            p.light_theme = id;
        } else {
            p.theme = id;
        }
    });
    apply_theme(cx, true);
}

pub fn set_wallpaper(choice: prefs::Wallpaper, cx: &mut App) {
    prefs::Prefs::update(cx, |p| p.wallpaper = choice);
    wallpaper::reload(cx);
}

/// Whether the color scheme resolves to dark right now: Zeron's System
/// card follows macOS.
pub fn scheme_is_dark(cx: &App) -> bool {
    match prefs::Prefs::of(cx).scheme.as_str() {
        "light" => false,
        "system" => matches!(
            cx.window_appearance(),
            gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
        ),
        _ => true,
    }
}

/// The palette the preferences ask for: the scheme's theme, then the
/// accent preset over it.
pub fn resolve_palette(cx: &App) -> Palette {
    let prefs = prefs::Prefs::of(cx);
    let dark = scheme_is_dark(cx);
    let seed = themes::find(if dark {
        &prefs.theme
    } else {
        &prefs.light_theme
    });
    let mut p = Palette::from_seed(seed);
    if let Some((_, _, on_dark, on_light)) =
        themes::ACCENTS.iter().find(|(id, ..)| *id == prefs.accent)
    {
        p.accent = hex(if p.light { *on_light } else { *on_dark });
        p.accent_strong = Hsla {
            l: p.accent.l * 0.85,
            ..p.accent
        };
        p.accent_wash = p.accent_strong.opacity(if p.light { 0.12 } else { 0.18 });
    }
    p
}

/// Re-resolve the palette after a preference or the system appearance
/// changed.
pub fn reapply(cx: &mut App) {
    let fonts = cx.try_global::<HaveFonts>().is_none_or(|f| f.0);
    apply_theme(cx, fonts);
}

/// Edit appearance preferences and repaint.
pub fn update_appearance(cx: &mut App, edit: impl FnOnce(&mut prefs::Prefs)) {
    prefs::Prefs::update(cx, edit);
    reapply(cx);
}

fn apply_theme(cx: &mut App, have_fonts: bool) {
    let p = resolve_palette(cx);
    let style = Style::get(styles::StyleId::parse(&prefs::Prefs::of(cx).style));
    let ui_size = prefs::Prefs::of(cx).ui_font_size;
    cx.set_global(p);
    cx.set_global(style);
    // Load the base first; a mode switch reloads colors, so the palette
    // goes in a second update (gpui-kit's documented order).
    Theme::change(
        if p.light {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        },
        None,
        cx,
    );
    Theme::update(cx, |t| {
        if have_fonts {
            t.font_family = if style.mono_ui { FONT_MONO } else { FONT_SANS }.into();
            t.mono_font_family = FONT_MONO.into();
        }
        t.font_size = px(if ui_size > 0. {
            ui_size
        } else {
            style.font_size
        });
        t.mono_font_size = px(14.);
        t.radius = px(style.control_radius);
        t.radius_lg = px(style.panel_radius.max(style.control_radius));
        t.shadow = style.elevation > 0.;

        let c = &mut t.colors;
        c.background = p.bg;
        c.foreground = p.text;
        c.border = p.border;
        c.input = p.border_strong;
        c.ring = p.accent.opacity(0.6);
        c.caret = p.accent;
        c.selection = p.accent.opacity(0.35);
        c.muted = p.surface_raised;
        c.muted_foreground = p.text_muted;
        c.accent = p.hover;
        c.accent_foreground = p.text;
        c.popover = p.surface_overlay;
        c.popover_foreground = p.text;
        c.overlay = hsla(0., 0., 0., if p.light { 0.25 } else { 0.6 });
        c.window_border = p.border;
        c.title_bar = p.surface;
        c.title_bar_border = p.border;
        c.status_bar = p.surface;
        c.status_bar_border = p.border;

        c.sidebar = p.surface;
        c.sidebar_foreground = p.text_muted;
        c.sidebar_border = p.border;
        c.sidebar_accent = p.active;
        c.sidebar_accent_foreground = p.text;
        c.sidebar_primary = p.accent;
        c.sidebar_primary_foreground = p.text;

        // Primary buttons carry the accent; it is the one loud color.
        c.primary = p.accent;
        c.primary_hover = lift(p.accent, 0.05);
        c.primary_active = p.accent_strong;
        c.primary_foreground = on_accent(p);
        c.button_primary = p.accent;
        c.button_primary_hover = lift(p.accent, 0.05);
        c.button_primary_active = p.accent_strong;
        c.button_primary_foreground = on_accent(p);
        c.secondary = p.hover;
        c.secondary_hover = p.active;
        c.secondary_active = p.text.opacity(0.16);
        c.secondary_foreground = p.text;

        c.button = p.hover;
        c.button_hover = p.active;
        c.button_active = p.text.opacity(0.16);
        c.button_foreground = p.text;
        c.button_secondary = p.hover;
        c.button_secondary_hover = p.active;
        c.button_secondary_active = p.text.opacity(0.16);
        c.button_secondary_foreground = p.text;

        c.list = p.bg;
        c.list_even = p.bg;
        c.list_head = p.surface;
        c.list_hover = p.hover;
        c.list_active = p.active;
        c.list_active_border = p.border_strong;
        c.table = p.bg;
        c.table_even = p.bg;
        c.table_head = p.surface;
        c.table_head_foreground = p.text_muted;
        c.table_hover = p.hover;
        c.table_active = p.active;
        c.table_active_border = p.border_strong;
        c.table_row_border = p.border;
        c.tab_bar = p.surface;
        c.tab = p.surface;
        c.tab_active = p.bg;
        c.tab_foreground = p.text_muted;
        c.tab_active_foreground = p.text;
        c.group_box = p.surface_card;
        c.group_box_foreground = p.text;
        c.skeleton = p.surface_raised;
        c.scrollbar = hsla(0., 0., 0., 0.);
        c.scrollbar_thumb = p.text.opacity(0.12);
        c.scrollbar_thumb_hover = p.text.opacity(0.2);
        c.switch = p.surface_raised;
        c.switch_thumb = p.text;
        c.slider_bar = p.accent;
        c.slider_thumb = p.text;
        c.progress_bar = p.accent;
        c.link = p.accent;
        c.link_hover = lift(p.accent, 0.08);
        c.link_active = p.accent_strong;
        c.drag_border = p.accent;
        c.drop_target = p.accent_wash;

        c.danger = p.danger;
        c.danger_hover = p.danger.opacity(0.9);
        c.danger_active = p.danger.opacity(0.8);
        c.danger_foreground = p.on_solid;
        c.button_danger = p.danger.opacity(0.16);
        c.button_danger_hover = p.danger.opacity(0.24);
        c.button_danger_active = p.danger.opacity(0.3);
        c.button_danger_foreground = p.danger;
        c.warning = p.warning;
        c.warning_foreground = p.on_solid;
        c.success = p.success;
        c.success_foreground = p.on_solid;
        c.info = p.accent;
        c.info_foreground = p.text;
    });
    wallpaper::reload(cx);
}

/// `0xRRGGBB` as Hsla.
pub fn hex(rgb: u32) -> Hsla {
    gpui_kit::rgb(rgb).into()
}

/// Label color on an accent fill: dark ink on bright accents.
pub fn on_accent(p: Palette) -> Hsla {
    if p.accent.l > 0.6 {
        hsla(0., 0., 0.06, 1.)
    } else {
        hsla(0., 0., 1., 1.)
    }
}

/// Lighter by `by` (0..1 of lightness).
pub fn lift(c: Hsla, by: f32) -> Hsla {
    Hsla {
        l: (c.l + by).min(1.),
        ..c
    }
}
