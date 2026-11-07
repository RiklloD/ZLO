//! The app's shared widgets, painted from the active [`Style`] and the
//! palette (GPUI `design/kit.rs` and the helpers of `ui.rs`). Every page and
//! the chat build their buttons, rows, chips and cards from here; rune-kit
//! components (Menu, Select, Popover, TextInput, Markdown, Switch, Tooltip)
//! serve everything else and follow the same palette through
//! `design::kit(&palette)`.
//!
//! The glass recipes (gradient lit from above, hairline rim, inset top
//! highlight, short drop) are adapted from Zeron's `glass.rs` (MIT).
//!
//! Motion: GPUI faded hover washes and morphed selections through its own
//! tween store. Here a hover is Rune's paint-only overlay (`El::hover` with
//! the style's `hover_motion`, no commit), hover ink is `group_color` under a
//! `hover_group`, and a selection change is a keyed element whose fill,
//! border or offset springs on the style's `slide`. GPUI's one-pixel press
//! sink has no Rune equivalent (layout offsets do not follow the pointer);
//! a press overlay stands in.
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
//! - `Button::new(key)` `.label() .icon(&Icon) .icon_size() .primary() .ghost()
//!   .outline() .danger() .small() .xsmall() .large() .selected() .pill_selected()
//!   .disabled() .loading() .full() .tooltip() .accessibility_label()
//!   .on_press(|s: &mut State| ..) .on_press_tx(..) .build(s) -> El`;

//! - `row(s, key, active) -> El` list/sidebar row; `row_ink(s, active) -> (Color, Color)`
//! - `pill_row(s, key, active) -> El` a row over a `slide_pill`
//! - `slide_pill(s, key, top: Option<f32>) -> Option<El>` gliding selection
//! - `segmented(s, key, labels, selected, disabled, on_pick) -> El`
//! - `chip(s, key, label, on) -> El` toggle chip (caller adds `.on_press`)
//! - `shortcut_badge(s, index) -> El`, `cmd_digit(&KeyEvent) -> Option<usize>`
//! - `hover_bg(el, s) -> El` (GPUI `smooth_bg`: a fading hover wash)
//! - `activity_line(s, key, icon, title, detail) -> El`
//! - `selectable(k, text) -> El` selectable, copyable plain text
//! - `mono_editor(&edit).label() .rows(min, max) .on_edit(..) .build(k) -> El`
//!   the multi-line mono editor (memory, SKILL.md)
//!
//! Loops (`kit::loops`, frozen by reduced motion / the battery saver):
//! - `pulse_dots(key, color) -> El`, `gradient_spinner(key, cell) -> El`,
//!   `shimmer_text(s, key, text, font) -> El`, `status_dot(key, color, live) -> El`,
//!   `claw_mark(s) -> El`
//!
//! Text sizes: [`XS`] (12) and [`SM`] (14) at the 16 pt rem, scaled by
//! `prefs.ui_font_size` through [`Style::text`].
//!
//! `ui.rs` mapping: `ui::Button` → [`Button`]; `ui::card` → [`card`];
//! `ui::activity_line` → [`activity_line`]; `ui::selectable` → [`selectable`];
//! `ui::markdown`/`kept_markdown` → `rune_kit::Markdown` (streaming-aware, keeps
//! its parse per key); `cached_view` → `El::cache()`; the gpui-kit re-exports
//! (Input, Switch, Tag, charts, menus) → `rune::TextEdit` + `rune_kit::{TextInput,
//! TextArea, Switch, Badge, Menu, Popover, ...}`.

pub mod compact;
mod editor;
mod loops;

pub use editor::mono_editor;

pub use loops::{claw_mark, gradient_spinner, pulse_dots, shimmer_text, status_dot};

use std::rc::Rc;

use rune::tok::TextStyle;
use rune::{Color, Cursor, El, Icon, Shadow, Transition, Tx, col, el, icon, text};

use super::motion::{self, mix};
use super::styles::Style;
use super::{Palette, lift};
use crate::state::State;

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
    pub fn apply(self, mut el: El) -> El {
        el = el.fill(self.fill).border(1., self.rim).hover(self.hover);
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
/// raised it reads at rest (0..1); the hover brightens it (GPUI's `h`).
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

/// The accent plate (primary buttons, the send button): a flat accent fill,
/// a lit top edge and a coloured halo.
pub fn accent(_st: &Style, p: &Palette) -> Plate {
    let base = p.accent;
    Plate {
        fill: base,
        gradient: Some((lift(base, 0.04), base)),
        rim: lift(base, 0.12).alpha(0.6),
        shadows: vec![
            shadow(white(0.22), 1., 0., 0., true),
            shadow(base.alpha(0.22), 2., 12., 0., false),
        ],
        hover: white(0.08),
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
/// session card). Add `.backdrop_blur(σ)` to blur what lies under it (GPUI
/// had no backdrop blur, so the fill stays dense enough on its own).
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

/// The app's button (GPUI `kit::Button`, same builder names): the hover
/// fades in the wash, `selected` morphs the wash in, Primary carries the
/// accent plate, `loading` swaps the icon for breathing dots.
pub struct Button {
    key: String,
    label: Option<String>,
    icon: Option<&'static Icon>,
    icon_px: Option<f32>,
    text_px: Option<f32>,
    variant: Variant,
    size: Size,
    selected: bool,
    /// The selection wash is drawn by a gliding pill behind the button;
    /// `selected` only sets the ink.
    pill: bool,
    disabled: bool,
    loading: bool,
    full: bool,
    tooltip: Option<String>,
    aria: Option<String>,
    on_press: Option<Press>,
}

// The builder mirrors GPUI's `kit::Button` whole (the gallery scene shows
// every variant); not every setter has a caller yet.
#[allow(dead_code)]
impl Button {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: None,
            icon: None,
            icon_px: None,
            text_px: None,
            variant: Variant::Secondary,
            size: Size::Medium,
            selected: false,
            pill: false,
            disabled: false,
            loading: false,
            full: false,
            tooltip: None,
            aria: None,
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
    /// Glyph size over the size preset's, for icons that draw edge to edge.
    pub fn icon_size(mut self, size: f32) -> Self {
        self.icon_px = Some(size);
        self
    }
    /// Label size over the size preset's.
    pub fn text_size(mut self, size: f32) -> Self {
        self.text_px = Some(size);
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
    pub fn danger(self) -> Self {
        self.variant(Variant::Danger)
    }
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }
    pub fn small(self) -> Self {
        self.size(Size::Small)
    }
    pub fn xsmall(self) -> Self {
        self.size(Size::XSmall)
    }
    /// Icon-first header buttons: a 38 pt target with a 20 pt glyph.
    pub fn large(self) -> Self {
        self.size(Size::Large)
    }
    /// Selected ink over a pill the caller draws ([`slide_pill`]), no own wash.
    pub fn pill_selected(mut self, on: bool) -> Self {
        self.pill = true;
        self.selected(on)
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
    /// Stretch to the parent's width, content left-aligned.
    pub fn full(mut self) -> Self {
        self.full = true;
        self
    }
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        self.tooltip = Some(text.into());
        self
    }
    pub fn accessibility_label(mut self, text: impl Into<String>) -> Self {
        self.aria = Some(text.into());
        self
    }
    pub fn on_press(mut self, f: impl Fn(&mut State) + 'static) -> Self {
        self.on_press = Some(Rc::new(move |s, _| f(s)));
        self
    }
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
        let text_px = self.text_px.unwrap_or_else(|| {
            st.text(match self.size {
                Size::XSmall | Size::Small => XS,
                Size::Medium | Size::Large => SM,
            })
        });
        // Ink at rest and while hovered.
        let (ink, hover_ink) = match self.variant {
            Variant::Primary => (p.on_accent(), p.on_accent()),
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
            .motion(st.slide);
        b = if self.full {
            b.w_full()
        } else {
            b.justify_center()
        };
        b = if icon_only { b.w(height) } else { b.px(pad) };
        if let Some(a) = self.aria.clone().or_else(|| self.label.clone()) {
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
            Variant::Ghost => {
                let wash = self.selected && !self.pill;
                b.fill(if wash { p.active } else { Color::TRANSPARENT })
                    .hover(p.hover)
            }
            Variant::Danger => b
                .border(1., p.danger.alpha(0.3))
                .fill(p.danger.alpha(0.1))
                .hover(p.danger.alpha(0.08)),
        };

        if self.loading {
            b = b.child(pulse_dots(&format!("{}-dots", self.key), ink));
        } else if let Some(i) = self.icon {
            let size = self.icon_px.unwrap_or(match self.size {
                Size::Large => 20.,
                Size::Medium => 16.,
                Size::Small => 14.,
                Size::XSmall => 12.,
            });
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
            .press(if p.light { black(0.05) } else { white(0.05) });
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
    let st = Style::of(&s.prefs);
    let top = top?;
    Some(
        el().key(key.to_string())
            .pin(Some(0.), Some(top), Some(0.), None)
            .h(st.row_height)
            .radius(8.)
            .fill(selected_wash(&s.palette))
            .a11y_hidden()
            .motion(motion::PILL)
            .transition(Transition::opacity()),
    )
}

/// A segmented control whose highlight slides between equal-width segments
/// (Zeron's 150 ms tab slide) instead of jumping.
pub fn segmented(
    s: &State,
    key: &str,
    labels: &[&str],
    selected: usize,
    disabled: bool,
    on_pick: impl Fn(&mut State, usize) + Clone + 'static,
) -> El {
    const SEG_W: f32 = 54.;
    const SEG_H: f32 = 24.;
    const INSET: f32 = 2.;
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let knob_fill = if p.light {
        p.surface_raised
    } else {
        white(0.12)
    };
    let mut knob = el()
        .key(format!("{key}-knob"))
        .absolute(INSET + selected as f32 * SEG_W, INSET)
        .size(SEG_W, SEG_H)
        .radius(6.)
        .fill(knob_fill)
        .a11y_hidden()
        .motion(motion::TAB_SLIDE);
    if p.light {
        knob = knob.shadow(1., 2., black(0.05));
    }
    let mut bar = rune::row()
        .key(key.to_string())
        .role("radiogroup")
        .self_start()
        .padding(INSET)
        .radius(8.)
        .fill(hover_wash(p))
        .child(knob);
    for (ix, label) in labels.iter().enumerate() {
        let on = ix == selected;
        let mut seg = el()
            .key(format!("{key}-seg-{ix}"))
            .role("radio")
            .label(label.to_string())
            .size(SEG_W, SEG_H)
            .center()
            .child(
                text(label.to_string())
                    .font(font(st.text(XS), 400.))
                    .color(if on { p.text } else { p.text_muted })
                    .group_color(p.text),
            );
        if !disabled && !on {
            let pick = on_pick.clone();
            seg = seg
                .cursor(Cursor::Pointer)
                .hover_group()
                .on_press(move |s: &mut State| pick(s, ix));
        }
        bar = bar.child(seg);
    }
    if disabled {
        bar = bar.dim(0.5);
    }
    bar
}

/// A toggle chip (filters, tags) with animated on/off. The caller adds
/// `.on_press`.
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

/// t3code's `⌘N` keycap, shown on rows while ⌘ is held.
pub fn shortcut_badge(s: &State, index: usize) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    el().px(5.)
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
    floating(&st, &s.palette)
        .apply(
            content
                .key(key.to_string())
                .radius(st.panel_radius.clamp(6., 12.)),
        )
        .transition(motion::pop(-6.))
}

/// A raised card used for approval prompts and run notices, in the active
/// style's material (GPUI `ui::card`): a column with 8 pt gaps and 12 pt
/// padding.
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

/// Plain text the pointer can select and ⌘C can copy (GPUI `ui::selectable`).
pub fn selectable(k: &rune_kit::Kit, content: impl Into<String>) -> El {
    text(content.into())
        .selectable(true)
        .selection_color(k.selection())
}

/// One line of run activity (thinking, tool call, skill activation): the
/// icon sits in a small tile, the detail folds to three lines (GPUI
/// `ui::activity_line`). `key` is the row's, so both lines stay selectable
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
