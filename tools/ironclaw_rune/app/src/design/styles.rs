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
//!
//! GPUI flipped the window's backdrop live (`set_translucent`/`apply_window`).
//! Rune fixes a window's transparency when it creates the window
//! (`WindowOptions::transparent`), so `main` asks for a transparent window
//! when `prefs.translucent` is on at launch and records it here
//! ([`set_window_translucent`]). The style is translucent only while both
//! hold: switching the pref on mid-session waits for the next launch
//! (Appearance says so), and switching it off turns the fills opaque at once.
//! Headless renders and tests, which open no window, follow the pref.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use rune::{Curve, Spring, SystemMaterial, TextStyle};

use super::motion::TAILWIND;
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
    /// Sliding selection indicators and toggles.
    pub slide: Spring,
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
            slide: Spring::new(0.30, 0.0),
        }
    }

    /// The style the preferences ask for, within what the window can show.
    pub fn of(prefs: &Prefs) -> Self {
        let mut s = Self::new(prefs.translucent && window_translucent().unwrap_or(true));
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

/// The UI type scale, `font_size / 16` (GPUI's rem against its 16 pt
/// base), as `f32` bits. Set from the preferences at the top of every view
/// build ([`set_type_scale`]), read by the shared text helpers
/// ([`rem`], [`scaled`]) so pages, compact controls, the side pane and
/// rune-kit's components follow Appearance → UI font size together, as the
/// transcript does through [`Style::text`]. Code keeps its fixed sizes.
static TYPE_SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

/// Record the type scale the preferences ask for. Call once per view build,
/// before anything draws text.
pub fn set_type_scale(prefs: &Prefs) {
    let k = Style::of(prefs).font_size / 16.;
    TYPE_SCALE.store(k.to_bits(), Ordering::Relaxed);
}

/// The current type scale (1 at the default 16 pt base).
pub fn type_scale() -> f32 {
    f32::from_bits(TYPE_SCALE.load(Ordering::Relaxed))
}

/// A text size given at the 16 pt base, at the current scale: whole points
/// when scaled, as [`Style::text`] rounds, and untouched at the default.
pub fn rem(px_at_16: f32) -> f32 {
    let k = type_scale();
    if (k - 1.).abs() < f32::EPSILON {
        px_at_16
    } else {
        (px_at_16 * k).round()
    }
}

/// `style` at the current type scale: its size and an explicit line height.
pub fn scaled(style: TextStyle) -> TextStyle {
    TextStyle {
        size: rem(style.size),
        line_height: if style.line_height > 0. {
            rem(style.line_height)
        } else {
            0.
        },
        ..style
    }
}

/// Whether the open window was created transparent; unset without a window.
static WINDOW_TRANSLUCENT: OnceLock<bool> = OnceLock::new();

/// Record the transparency `main` created the window with (once, at launch).
pub fn set_window_translucent(on: bool) {
    if WINDOW_TRANSLUCENT.set(on).is_err() {
        tracing::debug!("window translucency already recorded");
    }
}

/// The transparency the window was created with, or `None` when there is no
/// window (headless renders, tests).
pub fn window_translucent() -> Option<bool> {
    WINDOW_TRANSLUCENT.get().copied()
}

/// The pref is on but the window opened opaque: it shows after a relaunch.
/// (Off always applies at once: the fills turn opaque.)
pub fn translucency_pending(prefs: &Prefs) -> bool {
    prefs.translucent && window_translucent() == Some(false)
}
