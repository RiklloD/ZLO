//! Small shared pieces of the workspace's drawing: text in the GPUI sizes,
//! dots, ghost buttons, washes and the floating card. Area G's
//! `design/kit.rs` takes these over at integration where they overlap.

use rune::{Color, El, Icon, TextStyle, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::design::{Palette, motion};
use crate::state::State;

/// GPUI's `text_xs` / `text_sm` in points.
pub const XS: f32 = 12.;
pub const SM: f32 = 14.;

pub const REGULAR: f32 = 400.;
pub const MEDIUM: f32 = 500.;
pub const SEMIBOLD: f32 = 600.;

/// GPUI's default line height, a multiple of the size (`gpui::phi`).
const GPUI_LINE: f32 = 1.618;

/// The sidebar's own type, as GPUI drew it: sizes in fixed pixels (the UI
/// font size never scales them, unlike its rem-based `text_xs`/`text_sm`)
/// on GPUI's 1.618 line height, which gives the cards their air.
pub fn sidebar_style(size: f32, weight: f32) -> TextStyle {
    TextStyle {
        line_height: (size * GPUI_LINE).round(),
        ..TextStyle::new(size, weight)
    }
}

/// Sidebar text at `size` and `weight` ([`sidebar_style`]).
pub fn side_tw(s: impl Into<String>, size: f32, weight: f32, color: Color) -> El {
    text(s.into())
        .font(sidebar_style(size, weight))
        .color(color)
}

/// Sidebar text at `size` in the regular weight ([`sidebar_style`]).
pub fn side_t(s: impl Into<String>, size: f32, color: Color) -> El {
    side_tw(s, size, REGULAR, color)
}

/// Text at `size` points (at the 16 pt base, scaled to the UI font size)
/// in the UI face.
pub fn t(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s.into())
        .font(TextStyle::new(crate::design::styles::rem(size), REGULAR))
        .color(color)
}

/// Text at `size` and `weight`.
pub fn tw(s: impl Into<String>, size: f32, weight: f32, color: Color) -> El {
    text(s.into())
        .font(TextStyle::new(crate::design::styles::rem(size), weight))
        .color(color)
}

pub fn dot(color: Color, size: f32) -> El {
    el().size(size, size).radius(size / 2.).fill(color)
}

pub fn glyph(i: &'static Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color)
}

/// An icon-only ghost button with its tooltip and accessible name.
pub fn ghost_icon(
    k: &Kit,
    i: &'static Icon,
    help: &str,
    size: ControlSize,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    IconButton::new(i)
        .variant(IconButtonVariant::Ghost)
        .control_size(size)
        .label(help)
        .help(help)
        .on_press(on_press)
        .build(k)
}

/// [`ghost_icon`] whose accessible name differs from its tooltip.
pub fn ghost_icon_named(
    k: &Kit,
    i: &'static Icon,
    name: &str,
    help: &str,
    size: ControlSize,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    IconButton::new(i)
        .variant(IconButtonVariant::Ghost)
        .control_size(size)
        .label(name)
        .help(help)
        .on_press(on_press)
        .build(k)
}

/// A small ghost button with a label and an optional icon.
pub fn ghost_button(
    k: &Kit,
    label: &str,
    i: Option<&'static Icon>,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    let mut b = Button::new(label)
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small);
    if let Some(i) = i {
        b = b.icon(i);
    }
    b.on_press(on_press).build(k)
}

/// A small outlined button (GPUI's default `Button` at xsmall).
pub fn small_button(
    k: &Kit,
    label: &str,
    disabled: bool,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    Button::new(label)
        .variant(ButtonVariant::Outline)
        .control_size(ControlSize::Small)
        .disabled(disabled)
        .on_press(on_press)
        .build(k)
}

/// Row hover: one step under the selection.
pub fn hover_wash(p: &Palette) -> Color {
    if p.light {
        Color::BLACK.alpha(0.035)
    } else {
        Color::WHITE.alpha(0.04)
    }
}

/// The selected row's wash (Zeron: the wash alone marks it).
pub fn selected_wash(p: &Palette) -> Color {
    if p.light {
        Color::BLACK.alpha(0.055)
    } else {
        Color::WHITE.alpha(0.07)
    }
}

/// A row holding an unsent draft: a faint wash of the theme's accent.
pub fn draft_wash(p: &Palette) -> Color {
    p.accent.alpha(if p.light { 0.07 } else { 0.09 })
}

/// A filter chip: rounded, accent-washed when on. Its hover wash fades,
/// on/off morphs on the slide spring and it shrinks while pressed, as
/// `design::kit::chip` does (which it matches in motion; it keeps the
/// sidebar's 24 pt height and its own type scale, so it does not delegate).
pub fn chip(
    p: &Palette,
    key: String,
    label: &str,
    on: bool,
    f: impl Fn(&mut State) + 'static,
) -> El {
    row()
        .key(key)
        .h(24.)
        .px(8.)
        .center_y()
        .radius(12.)
        .border(1., if on { p.accent.alpha(0.5) } else { p.border })
        .fill(if on {
            p.accent_wash
        } else {
            Color::TRANSPARENT
        })
        .hover(p.hover)
        .press(p.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .hover_group()
        .motion(motion::SLIDE)
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(label.to_string())
        .child(t(label, XS, if on { p.text } else { p.text_muted }).group_color(p.text))
        .on_press(f)
        .press_scale(motion::PRESS)
}

/// `⌘N` in a key-cap, keyed `shortcut-badge` (one per row): it pops in
/// as ⌘ goes down and out as it comes up (`motion::glyph_pop`).
pub fn shortcut_badge(p: &Palette, index: usize) -> El {
    row()
        .key("shortcut-badge")
        .transition(motion::glyph_pop())
        .px(5.)
        .py(1.)
        .radius(4.)
        .border(1., p.border_strong)
        .fill(p.hover)
        .child(t(format!("⌘{}", index + 1), XS, p.text_muted))
}

/// The floating card of menus and popovers: overlay fill, a hairline and a
/// soft shadow, on continuous corners. It pops in, falling 6 pt into place,
/// and pops out faster (`motion::pop(-6.)`) where its commit animates;
/// inside `rune_kit::popover::anchored` the anchor's instant open and fade
/// win. A caller's own `.transition(..)` after this call replaces it.
pub fn floating(p: &Palette, content: El, radius: f32) -> El {
    content
        .radius(radius)
        .continuous_corners()
        .transition(motion::pop(-6.))
        .fill(p.surface_overlay)
        .border(1., p.border_strong)
        .shadow(
            12.,
            32.,
            Color::BLACK.alpha(if p.light { 0.1 } else { 0.4 }),
        )
}

/// A thin horizontal rule.
pub fn rule(p: &Palette) -> El {
    el().h(1.).w_full().fill(p.border)
}

/// A borderless multi-line field that grows from `min` to `max` lines
/// (GPUI's `Textarea::appearance(false)` with `auto_grow`). Bind it with
/// `.on_edit(..)`, `.on_submit(..)` and `.on_input_change(..)` on the
/// returned field before wrapping it with [`grow_box`].
pub fn plain_area(edit: &rune::TextEdit, size: f32, placeholder: &str, p: &Palette) -> El {
    let size = crate::design::styles::rem(size);
    rune::text_input(edit, TextStyle::new(size, REGULAR).lh(plain_line(size)))
        .key("input")
        .w_full()
        .color(p.text)
        .placeholder(placeholder.to_string(), p.text_faint)
        .caret(p.text, p.accent.alpha(0.3))
}

/// A [`plain_area`]'s line height at `size` (already in rem).
fn plain_line(size: f32) -> f32 {
    (size * 1.45).round()
}

/// [`grow_box`]'s `line` for a [`plain_area`] of `size` points.
pub fn plain_area_line(size: f32) -> f32 {
    plain_line(crate::design::styles::rem(size))
}

/// The scroll box that grows `field` (whose lines are `line` points tall)
/// between `min` and `max` lines, then scrolls inside it (the viewport keyed
/// `"scroll"`). It is `rune::auto_grow` without its edit animation: Rune's
/// helper marks the field `animate_edit_layout`, which turns every edit's
/// commit into an animated one, so a multiline paste or a select-all delete
/// sprang the composer and everything laid out around it. Here an edit
/// lands its layout in its own frame, as typing should. Every composer's
/// field (chat, new-thread, quick) grows in one of these.
pub fn grow_box(field: El, line: f32, min: usize, max: usize) -> El {
    let (min, max) = (min.max(1), max.max(min.max(1)));
    rune::col()
        .key("scroll")
        .w_full()
        .shrink()
        .min_h(min as f32 * line)
        .max_h(max as f32 * line)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .child(field)
}
