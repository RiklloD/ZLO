//! Shared page helpers (GPUI `pages/mod.rs` helpers plus the few `ui.rs` and
//! `design/kit.rs` pieces every page uses): the page column, its title row,
//! settings groups and rows, cards, tags and the small controls the pages
//! share. Areas C, D and E build their pages from these; keep the API small
//! and stable. [`ts`] holds the text styles, [`controls`] the sized and
//! keyed controls and the smaller pieces.

pub mod controls;
pub mod ts;

use rune::{Color, El, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl,
    Switch,
};

use crate::design::smooth_scroll::WheelGlide;
use crate::design::Palette;
use crate::state::State;

/// Zeron's settings column (`widgets::page_column`).
pub const PAGE_MAX_WIDTH: f32 = 760.;

/// Geist at `size` px and `weight` (400 regular, 500 medium, 600 semibold).
pub fn font(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}

/// Text sizes of the GPUI app's rem scale (16 px base).
pub const XS: f32 = 12.;
pub const SM: f32 = 14.;
pub const BASE: f32 = 16.;
pub const XL: f32 = 20.;

/// A line of text in one size, weight and colour.
pub fn label(s: impl Into<String>, size: f32, weight: f32, color: Color) -> El {
    text(s).font(font(size, weight)).color(color)
}

/// Like [`label`], wrapping (GPUI `whitespace_normal`).
pub fn para(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s).font(font(size, 400.)).color(color).wrap()
}

/// Title row with page-level actions on the right: Zeron's 20 px medium
/// page title, inset 8 px like its section labels.
pub fn header(p: &Palette, title: &str, actions: El) -> El {
    row()
        .key("page-header")
        .w_full()
        .justify_between()
        .center_y()
        .px(8.)
        .pb(8.)
        .child(label(title, XL, 500., p.text))
        .child(actions)
}

/// A row of header actions, 8 px apart (GPUI `h_flex().gap_2()`).
pub fn actions(items: impl IntoIterator<Item = El>) -> El {
    row().gap(4.).center_y().children(items)
}

/// A settings group's label, Zeron's `section_label`: 16 px semibold text,
/// inset 8 px, 32 px above the group, with an optional line of explanation.
pub fn group_header(p: &Palette, title: &str, note: Option<&str>) -> El {
    let mut d = col()
        .key(format!("group-{title}"))
        .w_full()
        .pt(32.)
        .pb(8.)
        .px(8.)
        .gap(2.)
        .child(label(title, BASE, 600., p.text).font(font(BASE, 600.).lh(22.)));
    if let Some(note) = note {
        d = d.child(para(note, XS, p.text_muted));
    }
    d
}

/// Zeron's block fill: a faint wash of the theme's ink, no outline.
pub fn block_fill(p: &Palette) -> Color {
    if p.light {
        Color::BLACK.alpha(0.045)
    } else {
        Color::WHITE.alpha(0.045)
    }
}

/// Related settings in one block: 12 px radius, the wash fill, hairlines
/// between rows inset 16 px.
pub fn rows_card(p: &Palette, rows: impl IntoIterator<Item = El>) -> El {
    let mut card = col()
        .w_full()
        .radius(12.)
        .fill(block_fill(p))
        .clip_rounded(12.);
    for (ix, r) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(
                el().key(format!("rule-{ix}"))
                    .mx(16.)
                    .h(1.)
                    .fill(p.border.alpha(p.border.a * 0.6))
                    .a11y_hidden(),
            );
        }
        card = card.child(r);
    }
    card
}

/// One setting, Zeron's `card_row`: a 14 px medium title and a 12 px muted
/// description on the left, the control on the right, at least 60 px tall.
pub fn setting_row(p: &Palette, title: &str, description: Option<&str>, control: El) -> El {
    controls::keyed_setting_row(p, &format!("setting-{title}"), title, description, control)
}

/// Centered, width-capped page body that scrolls.
pub fn page_body(key: &str, content: El) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        }).wheel_glide(key)
        .child(
            row()
                .w_full()
                .justify_center()
                .px(40.)
                .pt(28.)
                .pb(48.)
                .child(col().w_full().max_w(PAGE_MAX_WIDTH).child(content)),
        )
}

/// A quiet centered line for empty lists and placeholders.
pub fn empty_state(p: &Palette, msg: impl Into<String>) -> El {
    row()
        .w_full()
        .py(48.)
        .justify_center()
        .child(para(msg, SM, p.text_muted).align(rune::Align::Center))
}

/// A request's error in danger red.
pub fn error_line(p: &Palette, msg: &str) -> El {
    col().w_full().pb(12.).child(para(msg, SM, p.danger))
}

/// A status line (success, warning) under the header.
pub fn notice_line(msg: &str, color: Color) -> El {
    col().w_full().pb(12.).child(para(msg, SM, color))
}

/// The ghost refresh icon every page header carries.
pub fn refresh_button(k: &Kit, on_press: impl Fn(&mut State) + 'static) -> El {
    icon_button(&crate::icons::REFRESH_CW, "Refresh")
        .on_press(on_press)
        .build(k)
}

/// GPUI `ui::card`: the neutral raised plate (a faint lit fill, a hairline
/// rim), 12 px padding, rows 8 px apart.
pub fn card(p: &Palette) -> El {
    let (fill, rim) = if p.light {
        (p.surface_card, Color::BLACK.alpha(0.07))
    } else {
        (Color::WHITE.alpha(0.06), Color::WHITE.alpha(0.095))
    };
    col()
        .w_full()
        .gap(8.)
        .padding(12.)
        .radius(crate::design::PANEL_RADIUS)
        .fill(fill)
        .border(1., rim)
}

/// What a tag says: GPUI `Tag::success()`, `warning()`, `secondary()`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Danger,
    Secondary,
}

/// A small status tag.
pub fn tag(p: &Palette, s: &str, tone: Tone) -> El {
    let ink = match tone {
        Tone::Success => p.success,
        Tone::Warning => p.warning,
        Tone::Danger => p.danger,
        Tone::Secondary => p.text_muted,
    };
    row()
        .h(20.)
        .px(6.)
        .center_y()
        .radius(crate::design::CONTROL_RADIUS)
        .fill(ink.alpha(if tone == Tone::Secondary { 0.12 } else { 0.15 }))
        .child(label(s, XS, 500., ink))
}

/// GPUI button flavours.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// GPUI's default button: the neutral raised plate.
    Secondary,
    Primary,
    Ghost,
    Outline,
    Danger,
}

/// A small labelled button (GPUI `Button::new(..).label(..).small()`);
/// chain `on_press`, `disabled`, `icon` and `build(k)`.
pub fn button(s: &str, kind: Btn) -> Button {
    Button::new(s)
        .variant(match kind {
            Btn::Secondary => ButtonVariant::Neutral,
            Btn::Primary => ButtonVariant::Submit,
            Btn::Ghost => ButtonVariant::Ghost,
            Btn::Outline => ButtonVariant::Outline,
            Btn::Danger => ButtonVariant::Danger,
        })
        .control_size(ControlSize::Regular)
}

/// GPUI `.xsmall()`.
pub fn button_xs(s: &str, kind: Btn) -> Button {
    button(s, kind).control_size(ControlSize::Small)
}

/// GPUI `Button::loading(on)`: the kit's loading look, not pressable.
pub fn loading(b: Button, on: bool) -> Button {
    if on {
        b.variant(ButtonVariant::Loading).disabled(true)
    } else {
        b
    }
}

/// A primary button while `on`, else ghost (the pages' choice chips).
pub fn choice(s: &str, on: bool) -> Button {
    button(s, if on { Btn::Primary } else { Btn::Ghost })
}

/// A ghost icon button with a tooltip that also names it.
pub fn icon_button(i: &'static rune::Icon, tip: &str) -> IconButton {
    IconButton::new(i)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .help(tip)
        .label(tip)
}

/// A switch with an accessible name (no visible label).
pub fn switch(
    k: &Kit,
    checked: bool,
    disabled: bool,
    name: &str,
    on_toggle: impl Fn(&mut State) + 'static,
) -> El {
    let mut sw = Switch::new(checked)
        .label(name)
        .hide_label()
        .control_size(ControlSize::Small);
    if disabled {
        sw = sw.disabled(true);
    } else {
        sw = sw.on_toggle(on_toggle);
    }
    sw.build(k)
}

/// GPUI `kit::segmented`: equal segments, one picked.
pub fn segmented(
    k: &Kit,
    labels: &[&str],
    selected: usize,
    disabled: bool,
    on_pick: impl Fn(&mut State, usize, &mut Tx) + Clone + 'static,
) -> El {
    // GPUI drew 54 px segments; the kit's font needs more, so each fits its widest label.
    let mut seg = SegmentedControl::new(labels, Some(selected)).fit();
    let seg = if disabled {
        for i in 0..labels.len() {
            seg = seg.segment_state(i, rune_kit::State::Disabled);
        }
        seg.build(k)
    } else {
        seg.on_pick(on_pick).build(k)
    };
    // A row keeps it at its own width inside stretching columns.
    row().child(seg)
}

/// An icon at `size` in `color`.
pub fn glyph(i: &'static rune::Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color).a11y_hidden()
}
