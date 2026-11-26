//! The app's shared widgets, painted from the active [`Style`] and the
//! palette. Every page and
//! the chat build their buttons, rows, chips and cards from here; rune-kit
//! components (Menu, Select, Popover, TextInput, Markdown, Switch, Tooltip)
//! serve everything else and follow the same palette through
//! `design::kit(&palette)`.
//!
//! The glass recipes (gradient lit from above, hairline rim, inset top
//! highlight, short drop) are adapted from Zeron's `glass.rs` (MIT).
//!
//! Motion: a hover is Rune's paint-only overlay (`El::hover` with
//! the style's `hover_motion`, no commit), hover ink is `group_color` under a
//! `hover_group`, and a selection change is a keyed element whose fill,
//! border or offset springs on the style's `slide`. A press is an overlay
//! plus a small shrink about the centre (`El::press_scale`, [`motion::PRESS`]
//! on controls, [`motion::PRESS_CARD`] on rows and cards; layout and hit
//! testing keep the resting size, Reduce Motion turns it off). Surfaces
//! round their corners on Apple's continuous curve (`continuous_corners`).
//!
//! # Public API (all take the root `&State` for palette and style)
//!
//! Surfaces:
//! - `struct Plate { fill, gradient, rim, shadows, hover }` + `Plate::apply(el) -> El`
//! - `neutral(st, p, lit) -> Plate`, `accent(st, p) -> Plate`,
//!   `floating(st, p) -> Plate`, `glass(st, p) -> Plate`
//! - `selected_wash(p)`, `draft_wash(p)`, `hover_wash(p) -> Color`
//! - `card(s) -> El` (a column: approval prompts, run notices)
//! - `popover(s, key, content) -> El` (floating card, arrives on the pop spring)
//!
//! Controls:
//! - `Button::new(key)` `.label() .icon(&Icon) .variant() .primary() .ghost()
//!   .outline() .size() .small() .selected() .disabled() .loading() .tooltip()
//!   .on_press(|s: &mut State| ..) .build(s) -> El`;

//! - `row(s, key, active) -> El` list/sidebar row; `row_ink(s, active) -> (Color, Color)`
//! - `pill_row(s, key, active) -> El` a row over a `slide_pill`
//! - `slide_pill(s, key, top: Option<f32>) -> Option<El>` gliding selection;
//!   `slide_pill_sized(s, key, top, height)` for rails whose rows are not
//!   `row_height` tall
//! - `segmented(s, key, labels, selected, disabled, on_pick) -> El` (gliding
//!   knob, segments sized to their labels: `segment_width(label, px)`)
//! - `chip(s, key, label, on) -> El` toggle chip (caller adds `.on_press`)
//! - `shortcut_badge(s, index) -> El` (pops in and out while ⌘ is held)
//! - `hover_bg(el, s) -> El` (a fading hover wash)
//! - `activity_line(s, key, icon, title, detail) -> El`
//! - `selectable(k, text) -> El` selectable, copyable plain text
//! - `mono_editor(&edit).label() .rows(min, max) .on_edit(..) .build(k) -> El`
//!   the multi-line mono editor (memory, SKILL.md)
//!
//! Glyphs that move (`kit::glyphs`):
//! - `chevron(key, open, Turn::{Disclose, Flip}, size, color) -> El` one
//!   chevron that rotates open and shut
//! - `copy_glyph(key, copied, size, color) -> El` copy icon ↔ check, with
//!   `copied(at)` (`glyphs::COPIED_FOR`)
//! - `busy_glyph(key, icon, busy, size, color) -> El` breathes while busy
//! - `glyph_button(k, key, variant, tip, glyph, on_press_tx) -> El` kit's
//!   small icon button around one of those; `icon_ink(k, variant)`
//!
//! Loops (`kit::loops`, frozen by reduced motion / the battery saver):
//! - `pulse_dots(key, color) -> El`, `gradient_spinner(key, cell) -> El`,
//!   `shimmer_text(s, key, text, font) -> El`, `status_dot(key, color, live) -> El`,
//!   `claw_mark(s) -> El`
//!
//! Text sizes: [`XS`] (12) and [`SM`] (14) at the 16 pt rem, scaled by
//! `prefs.ui_font_size` through [`Style::text`].
//!
//! Markdown is `rune_kit::Markdown` (streaming-aware, keeps its parse per
//! key); fields, switches, badges and menus are rune-kit's own.

pub mod compact;
mod editor;
pub mod glyphs;
pub(crate) mod loops;
#[cfg(test)]
mod motion_tests;
mod segmented;

pub use editor::mono_editor;
pub use glyphs::{
    Turn, busy_glyph, chevron, copied, copy_glyph, glyph_button, glyph_button_sized, icon_ink,
};
pub use segmented::segmented;

pub use loops::{Spinner, claw_mark, gradient_spinner, pulse_dots, shimmer_text, status_dot};

use std::rc::Rc;

use rune::tok::TextStyle;
use rune::{Color, Cursor, El, Icon, Shadow, Transition, Tx, col, el, icon, text};

use super::motion::{self, mix};
use super::styles::Style;
use super::{Palette, lift};
use crate::state::State;

/// The kit rune-kit's `Checkbox` builds from: an unchecked box keeps a
/// visible hairline (the palette's strong border) on any row fill, where
/// the kit's weak separator all but vanishes on a grouped row. Only the
/// checkbox takes it; inputs and dividers keep the kit's own.
pub fn checkbox_kit(p: &Palette, k: &rune_kit::Kit) -> rune_kit::Kit {
    let mut c = k.clone();
    c.c.border_weak = p.border_strong;
    c
}

/// `text-xs` and `text-sm` at the 16 pt rem.
pub const XS: f32 = 12.;
pub const SM: f32 = 14.;
const MEDIUM: f32 = 500.;

fn white(a: f32) -> Color {
    Color::WHITE.alpha(a)
}

fn black(a: f32) -> Color {
    Color::BLACK.alpha(a)
}

fn shadow(color: Color, y: f32, blur: f32, spread: f32, inset: bool) -> Shadow {
    Shadow {
        x: 0.,
        y,
        blur,
        spread,
        color,
        inset,
    }
}

/// A painted surface: fill (with an optional top-lit gradient over it), rim,
/// shadows and the overlay drawn while hovered.
#[derive(Clone, Debug)]
pub struct Plate {
    pub fill: Color,
    pub gradient: Option<(Color, Color)>,
    pub rim: Color,
    pub shadows: Vec<Shadow>,
    pub hover: Color,
}

impl Plate {
    /// Paint the plate on `el` (which sets its own radius), its corners on
    /// Apple's continuous curve.
    pub fn apply(self, mut el: El) -> El {
        el = el
            .fill(self.fill)
            .border(1., self.rim)
            .hover(self.hover)
            .continuous_corners();
        if let Some((top, bottom)) = self.gradient {
            el = el.gradient(top, bottom);
        }
        for s in self.shadows {
            el = el.box_shadow(s);
        }
        el
    }
}

/// The neutral raised plate (secondary buttons, chips, cards). `lit` is how
/// raised it reads at rest (0..1); the hover brightens it.
/// One quiet recipe: a faint fill lit from above, a hairline rim and a soft
/// drop.
pub fn neutral(st: &Style, p: &Palette, lit: f32) -> Plate {
    let k = lit;
    if p.light {
        return Plate {
            fill: p.surface_card,
            gradient: None,
            rim: black(0.07),
            shadows: vec![
                shadow(black(0.04 * k * st.elevation), 1., 2., 0., false),
                shadow(black(0.04 * k * st.elevation), 4., 12., -4., false),
            ],
            hover: mix(p.surface_card, p.surface, 0.35).alpha(0.6),
        };
    }
    Plate {
        fill: Color::TRANSPARENT,
        gradient: Some((white(0.055 + 0.035 * k), white(0.035 + 0.03 * k))),
        rim: white(0.07 + 0.05 * k),
        shadows: vec![
            shadow(white(0.05 * k), 1., 0., 0., true),
            shadow(black(0.25 * k * st.elevation), 1., 3., 0., false),
        ],
        hover: white(0.03),
    }
}

/// The primary plate (primary buttons): the text colour as a flat fill,
/// no gradient, rim or halo. Monochrome, as the composer's Send: the accent
/// marks state, not actions.
pub fn accent(_st: &Style, p: &Palette) -> Plate {
    Plate {
        fill: p.text,
        gradient: None,
        rim: Color::TRANSPARENT,
        shadows: Vec::new(),
        hover: if p.light { white(0.12) } else { black(0.12) },
    }
}

/// Floating surfaces: popovers, menus, the composer.
pub fn floating(st: &Style, p: &Palette) -> Plate {
    let e = st.elevation;
    let fill = if st.vibrancy {
        p.surface_overlay.alpha(0.9)
    } else {
        p.surface_overlay
    };
    let mut shadows = vec![
        shadow(
            black((if p.light { 0.1 } else { 0.4 }) * e),
            12.,
            32.,
            -8.,
            false,
        ),
        shadow(
            black((if p.light { 0.05 } else { 0.2 }) * e),
            2.,
            6.,
            0.,
            false,
        ),
    ];
    if !p.light {
        shadows.push(shadow(white(0.06), 1., 0., 0., true));
    }
    Plate {
        fill,
        gradient: None,
        rim: if p.light { black(0.08) } else { white(0.09) },
        shadows,
        hover: Color::TRANSPARENT,
    }
}

/// A glass card: the theme's base plane let through at ~75 %, a hairline rim
/// and a soft drop, for light menus that float over the wallpaper (the
/// session card). Add `.backdrop_blur(σ)` to blur what lies under it (the
/// fill is dense enough to read without one).
pub fn glass(st: &Style, p: &Palette) -> Plate {
    let e = st.elevation;
    Plate {
        fill: p.bg.alpha(if p.light { 0.72 } else { 0.78 }),
        gradient: None,
        rim: if p.light { white(0.55) } else { white(0.08) },
        shadows: vec![
            shadow(
                black((if p.light { 0.08 } else { 0.32 }) * e),
                10.,
                28.,
                -10.,
                false,
            ),
            shadow(
                black((if p.light { 0.04 } else { 0.16 }) * e),
                1.,
                3.,
                0.,
                false,
            ),
        ],
        hover: Color::TRANSPARENT,
    }
}

/// The selected row: a light tint of the theme's ink, 7 % on dark and
/// 5.5 % on light, so the selection reads without turning into a slab.
pub fn selected_wash(p: &Palette) -> Color {
    if p.light { black(0.055) } else { white(0.07) }
}

/// A row holding an unsent draft: a faint wash of the theme's accent
/// (t3code's warm tint), under the hover and selection washes.
pub fn draft_wash(p: &Palette) -> Color {
    p.accent.alpha(if p.light { 0.07 } else { 0.09 })
}

/// Row hover: one step under the selection.
pub fn hover_wash(p: &Palette) -> Color {
    if p.light { black(0.035) } else { white(0.04) }
}

fn font(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    Primary,
    Secondary,
    Outline,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    XSmall,
    Small,
    Medium,
    Large,
}

type Press = Rc<dyn Fn(&mut State, &mut Tx)>;

/// The app's button: the hover
/// fades in the wash, `selected` morphs the wash in, Primary carries the
/// monochrome primary plate, `loading` swaps the icon for breathing dots.
pub struct Button {
    key: String,
    label: Option<String>,
    icon: Option<&'static Icon>,
    variant: Variant,
    size: Size,
    selected: bool,
    disabled: bool,
    loading: bool,
    tooltip: Option<String>,
    on_press: Option<Press>,
}

impl Button {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: None,
            icon: None,
            variant: Variant::Secondary,
            size: Size::Medium,
            selected: false,
            disabled: false,
            loading: false,
            tooltip: None,
            on_press: None,
        }
    }
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn icon(mut self, icon: &'static Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn variant(mut self, v: Variant) -> Self {
        self.variant = v;
        self
    }
    pub fn primary(self) -> Self {
        self.variant(Variant::Primary)
    }
    pub fn ghost(self) -> Self {
        self.variant(Variant::Ghost)
    }
    pub fn outline(self) -> Self {
        self.variant(Variant::Outline)
    }
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }
    pub fn small(self) -> Self {
        self.size(Size::Small)
    }
    pub fn selected(mut self, on: bool) -> Self {
        self.selected = on;
        self
    }
    pub fn disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        self
    }
    pub fn loading(mut self, on: bool) -> Self {
        self.loading = on;
        self
    }
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        self.tooltip = Some(text.into());
        self
    }
    pub fn on_press(mut self, f: impl Fn(&mut State) + 'static) -> Self {
        self.on_press = Some(Rc::new(move |s, _| f(s)));
        self
    }
    /// [`Self::on_press`] whose handler also gets the transaction (to
    /// `motion::color_morph(tx)`, copy, open a URL).
    pub fn on_press_tx(mut self, f: impl Fn(&mut State, &mut Tx) + 'static) -> Self {
        self.on_press = Some(Rc::new(f));
        self
    }

    pub fn build(self, s: &State) -> El {
        let st = Style::of(&s.prefs);
        let p = &s.palette;
        let inert = self.disabled || self.loading;
        let icon_only = self.label.is_none();
        let height = match self.size {
            Size::XSmall => 24.,
            Size::Small => 30.,
            Size::Medium => 34.,
            Size::Large => 38.,
        };
        let pad = match self.size {
            Size::XSmall => 7.,
            Size::Small => 9.,
            Size::Medium | Size::Large => 12.,
        };
        let text_px = st.text(match self.size {
            Size::XSmall | Size::Small => XS,
            Size::Medium | Size::Large => SM,
        });
        // Ink at rest and while hovered.
        let (ink, hover_ink) = match self.variant {
            Variant::Primary => (p.bg, p.bg),
            Variant::Danger => (p.danger, p.danger),
            _ if self.selected => (p.text, p.text),
            _ => (p.text_muted, p.text),
        };

        let mut b = rune::row()
            .key(self.key.clone())
            .role("button")
            .h(height)
            .gap(6.)
            .center_y()
            .radius(st.control_radius)
            .motion(st.slide)
            .justify_center();
        b = if icon_only { b.w(height) } else { b.px(pad) };
        if let Some(a) = self.label.clone() {
            b = b.label(a);
        }

        b = match self.variant {
            Variant::Primary => accent(&st, p).apply(b),
            Variant::Secondary => {
                let plate = neutral(&st, p, 0.6);
                let rim = if self.selected {
                    p.accent.alpha(0.6)
                } else {
                    plate.rim
                };
                plate.apply(b).border(1., rim)
            }
            Variant::Outline => b
                .border(
                    1.,
                    if self.selected {
                        p.accent
                    } else {
                        p.border_strong
                    },
                )
                .fill(if self.selected {
                    p.active
                } else {
                    Color::TRANSPARENT
                })
                .hover(p.hover),
            Variant::Ghost => b
                .fill(if self.selected {
                    p.active
                } else {
                    Color::TRANSPARENT
                })
                .hover(p.hover),
            Variant::Danger => b
                .border(1., p.danger.alpha(0.3))
                .fill(p.danger.alpha(0.1))
                .hover(p.danger.alpha(0.08)),
        };

        if self.loading {
            b = b.child(pulse_dots(&format!("{}-dots", self.key), ink));
        } else if let Some(i) = self.icon {
            let size = match self.size {
                Size::Large => 20.,
                Size::Medium => 16.,
                Size::Small => 14.,
                Size::XSmall => 12.,
            };
            b = b.child(icon(i, size).color(ink).group_color(hover_ink));
        }
        if let Some(label) = self.label {
            b = b.child(
                text(label)
                    .font(font(text_px, MEDIUM))
                    .color(ink)
                    .group_color(hover_ink)
                    .single_line(),
            );
        }
        if let Some(tip) = self.tooltip {
            b = b.help(tip);
        }
        if inert {
            if self.disabled {
                b = b.dim(0.45);
            }
            return b;
        }
        b = b
            .cursor(Cursor::Pointer)
            .hover_group()
            .hover_motion(st.hover, st.hover)
            .press(if p.light { black(0.05) } else { white(0.05) })
            .press_scale(motion::PRESS);
        match self.on_press {
            Some(f) => b.on_press_tx(move |s: &mut State, tx: &mut Tx| f(s, tx)),
            None => b,
        }
    }
}

/// A row's text colours (rest, hovered): its children set them, since Rune
/// text does not inherit colour.
pub fn row_ink(s: &State, active: bool) -> (Color, Color) {
    let p = &s.palette;
    if active {
        (p.text, p.text)
    } else {
        (p.text.alpha(0.8), p.text)
    }
}

/// A sidebar or list row with a fading hover wash and a selection that
/// morphs in (the wash alone marks it, no ring or tick, as in Zeron). Give
/// its text [`row_ink`] with `.group_color(..)`; the caller adds `.on_press`.
/// It sinks a little while pressed ([`motion::PRESS_CARD`]).
pub fn row(s: &State, key: &str, active: bool) -> El {
    let st = Style::of(&s.prefs);
    let p = &s.palette;
    rune::row()
        .key(key.to_string())
        .w_full()
        .min_h(st.row_height)
        .gap(8.)
        .px(8.)
        .center_y()
        .radius(8.)
        .fill(if active {
            selected_wash(p)
        } else {
            Color::TRANSPARENT
        })
        .hover(if active {
            Color::TRANSPARENT
        } else {
            hover_wash(p)
        })
        .hover_motion(st.hover, st.hover)
        .hover_group()
        .cursor(Cursor::Pointer)
        .press_scale(motion::PRESS_CARD)
        .motion(st.slide)
}

/// A row placed over a [`slide_pill`]: hover wash only, the pill carries the
/// selection.
pub fn pill_row(s: &State, key: &str, active: bool) -> El {
    let st = Style::of(&s.prefs);
    let p = &s.palette;
    rune::row()
        .key(key.to_string())
        .w_full()
        .h(st.row_height)
        .gap(8.)
        .px(8.)
        .center_y()
        .radius(8.)
        .hover(if active {
            Color::TRANSPARENT
        } else {
            hover_wash(p)
        })
        .hover_motion(st.hover, st.hover)
        .hover_group()
        .cursor(Cursor::Pointer)
}

/// A sliding pill behind a vertical list of rows. Put it as the first child
/// of the rows' container; `top` is the selected row's offset in points. It
/// glides between rows instead of jumping, and `None` fades it out where it
/// was (its exit transition).
pub fn slide_pill(s: &State, key: &str, top: Option<f32>) -> Option<El> {
    slide_pill_sized(s, key, top, Style::of(&s.prefs).row_height)
}

/// [`slide_pill`] for a vertical rail whose rows are `height` points tall
/// (a settings nav, a model picker's provider rail, a ⌘K list): the pill
/// takes the row's height and glides between offsets. Its container needs
/// no layout of its own beyond holding the rows (the pill is pinned
/// left/right and out of flow); rows over it use [`pill_row`] (or any row
/// without a selected fill).
///
/// ```ignore
/// let top = selected.map(|ix| ix as f32 * (ROW_H + GAP));
/// rail = rail.children(kit::slide_pill_sized(s, "nav-pill", top, ROW_H));
/// ```
pub fn slide_pill_sized(s: &State, key: &str, top: Option<f32>, height: f32) -> Option<El> {
    let top = top?;
    Some(
        el().key(key.to_string())
            .pin(Some(0.), Some(top), Some(0.), None)
            .h(height)
            .radius(8.)
            .continuous_corners()
            .fill(selected_wash(&s.palette))
            .a11y_hidden()
            .motion(motion::PILL)
            .transition(Transition::opacity()),
    )
}

/// A toggle chip (filters, tags) with animated on/off; it shrinks while
/// pressed ([`motion::PRESS`]). The caller adds `.on_press`.
pub fn chip(s: &State, key: &str, label: &str, on: bool) -> El {
    let st = Style::of(&s.prefs);
    let p = &s.palette;
    rune::row()
        .key(key.to_string())
        .role("button")
        .label(label.to_string())
        .px(8.)
        .py(2.)
        .radius(999.)
        .border(1., if on { p.accent.alpha(0.8) } else { p.border })
        .fill(if on {
            p.accent_wash
        } else {
            Color::TRANSPARENT
        })
        .hover(p.hover)
        .press(p.hover)
        .press_scale(motion::PRESS)
        .hover_motion(st.hover, st.hover)
        .hover_group()
        .cursor(Cursor::Pointer)
        .motion(st.slide)
        .child(
            text(label.to_string())
                .font(font(st.text(XS), 400.))
                .color(if on { p.text } else { p.text_muted })
                .group_color(p.text),
        )
}

/// t3code's `⌘N` keycap, shown on rows while ⌘ is held. It is keyed
/// `shortcut-badge` (one per row) and pops in as ⌘ goes down and out as it
/// comes up ([`motion::glyph_pop`]) instead of flashing.
pub fn shortcut_badge(s: &State, index: usize) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    el().key("shortcut-badge")
        .transition(motion::glyph_pop())
        .px(5.)
        .py(1.)
        .radius(4.)
        .border(1., p.border_strong)
        .fill(p.hover)
        .child(
            text(format!("⌘{}", index + 1))
                .font(font(st.text(XS), 400.))
                .color(p.text_muted),
        )
}

/// A floating popover card that arrives on the style's spring, falling
/// 6 points into place. Put it in the view's overlay `layer`, anchored with
/// `El::anchored`, or use `rune_kit::Popover` and style its content with
/// [`floating`].
pub fn popover(s: &State, key: &str, content: El) -> El {
    let st = Style::of(&s.prefs);
    let card = floating(&st, &s.palette)
        .apply(
            content
                .key(key.to_string())
                .radius(st.panel_radius.clamp(6., 12.)),
        )
        .transition(motion::pop(-6.));
    // Translucent only with the window's vibrancy: then blur what shows
    // through, as a macOS menu does.
    if st.vibrancy {
        card.backdrop_blur(16.)
    } else {
        card
    }
}

/// A raised card used for approval prompts and run notices, in the active
/// style's material: a column with 8 pt gaps and 12 pt padding.
pub fn card(s: &State) -> El {
    let st = Style::of(&s.prefs);
    let mut plate = neutral(&st, &s.palette, 0.5);
    plate.hover = Color::TRANSPARENT;
    plate.apply(
        col()
            .gap(8.)
            .padding(12.)
            .radius(st.panel_radius.clamp(4., 12.)),
    )
}

/// Plain text the pointer can select and ⌘C can copy.
pub fn selectable(k: &rune_kit::Kit, content: impl Into<String>) -> El {
    text(content.into())
        .selectable(true)
        .selection_color(k.selection())
}

/// One line of run activity (thinking, tool call, skill activation): the
/// icon sits in a small tile, the detail folds to three lines. `key` is the row's, so both lines stay selectable
/// while it streams.
pub fn activity_line(
    s: &State,
    key: &str,
    glyph: &'static Icon,
    title: &str,
    detail: Option<&str>,
) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let k = super::kit(p);
    let tile = el()
        .size(20., 20.)
        .center()
        .radius(st.control_radius.min(6.))
        .fill(p.hover)
        .border(1., p.border)
        .child(icon(glyph, 12.).color(p.text_muted));
    let mut lines = col()
        .key(format!("{key}-lines"))
        .grow(1.)
        .min_w(0.)
        .pt(3.)
        .gap(2.)
        .child(
            selectable(&k, title)
                .key(format!("{key}-title"))
                .font(font(st.text(XS), MEDIUM))
                .color(p.text),
        );
    if let Some(detail) = detail {
        // Folded to one paragraph so the three-line clamp holds.
        let folded = detail.split_whitespace().collect::<Vec<_>>().join(" ");
        lines = lines.child(
            selectable(&k, folded)
                .key(format!("{key}-detail"))
                .font(font(st.text(XS), 400.))
                .color(p.text_muted)
                .wrap()
                .max_lines(3),
        );
    }
    rune::row()
        .key(key.to_string())
        .gap(8.)
        .items_start()
        .child(tile)
        .child(lines)
}
