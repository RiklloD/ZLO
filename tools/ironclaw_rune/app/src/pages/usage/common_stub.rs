//! Area E's local copies of the GPUI `pages/mod.rs` helpers (header, group
//! header, rows card, setting row, page body, empty state, error line,
//! refresh button) and a few `design/kit.rs` pieces, used by the profiles,
//! memory and usage pages.
//!
//! PORT-STUB(area D): use pages::common. Area D owns the shared page
//! helpers in `pages/common.rs`; once it lands, the integrator replaces
//! every `common_stub::` use with it and deletes this file. Lives under
//! `usage` only because area E may not edit `pages/mod.rs`.

use rune::{Color, Cursor, El, ScrollSpec, TextStyle, col, el, icon, row, text};
use rune_kit::{ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::design::{self, Palette};
use crate::state::State;

/// Zeron's settings column (`widgets::page_column`).
pub const PAGE_MAX_WIDTH: f32 = 760.;

// GPUI's rem is 16 px and the body text 14 px: text_xs 12, text_sm 14,
// text_lg 18.
pub const XS: TextStyle = TextStyle::new(12.0, 400.0);
pub const XS_SEMIBOLD: TextStyle = TextStyle::new(12.0, 600.0);
pub const SM: TextStyle = TextStyle::new(14.0, 400.0);
pub const SM_MEDIUM: TextStyle = TextStyle::new(14.0, 500.0);
pub const BODY: TextStyle = TextStyle::new(14.0, 400.0);
pub const BODY_SEMIBOLD: TextStyle = TextStyle::new(14.0, 600.0);
pub const LG_SEMIBOLD: TextStyle = TextStyle::new(18.0, 600.0);
pub const TITLE: TextStyle = TextStyle::new(20.0, 500.0);

/// Text in a style and colour.
pub fn label(s: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(s.into()).font(style).color(color)
}

/// Title row with page-level actions on the right: Zeron's 20 px medium
/// page title, inset 8 px like its section labels.
pub fn header(p: &Palette, title: &str, actions: El) -> El {
    row()
        .key("page-header")
        .w_full()
        .justify_between()
        .center_y()
        .px(8.0)
        .pb(8.0)
        .child(label(title, TITLE, p.text))
        .child(row().gap(8.0).center_y().child(actions))
}

/// A settings group's label, Zeron's `section_label`: 16 px semibold,
/// inset 8 px, 32 px above the group, with an optional line of explanation.
pub fn group_header(p: &Palette, title: &str, note: Option<&str>) -> El {
    let mut c = col()
        .w_full()
        .pt(32.0)
        .pb(8.0)
        .px(8.0)
        .gap(2.0)
        .child(label(title, TextStyle::new(16.0, 600.0).lh(22.0), p.text));
    if let Some(note) = note {
        c = c.child(label(note, XS, p.text_muted).wrap());
    }
    c
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
    let mut card = col().w_full().radius(12.0).fill(block_fill(p)).clip_rounded(12.0);
    for (ix, r) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(hairline(p).mx(16.0).key(format!("rule-{ix}")));
        }
        card = card.child(r);
    }
    card
}

/// A 1 px rule in the border colour.
pub fn hairline(p: &Palette) -> El {
    el().h(1.0).fill(p.border.alpha(p.border.a * 0.6)).a11y_hidden()
}

/// A full-strength 1 px rule, GPUI's `border_t_1`.
pub fn rule(p: &Palette) -> El {
    el().w_full().h(1.0).fill(p.border).a11y_hidden()
}

/// One setting, Zeron's `card_row`: a 14 px medium title and a 12 px muted
/// description on the left, the control on the right, at least 60 px tall.
pub fn setting_row(p: &Palette, key: &str, title: &str, description: Option<String>, control: El) -> El {
    let mut left = col()
        .grow(1.0)
        .min_w(160.0)
        .gap(1.0)
        .child(label(title, SM_MEDIUM, p.text));
    if let Some(d) = description {
        left = left.child(label(d, XS.lh(18.0), p.text_muted).wrap());
    }
    row()
        .key(key.to_string())
        .w_full()
        .min_h(60.0)
        .px(16.0)
        .py(12.0)
        .gap(16.0)
        .center_y()
        .child(left)
        .child(el().child(control))
}

/// Centered, width-capped page body that scrolls.
pub fn page_body(key: &str, content: El) -> El {
    col().key(key.to_string()).w_full().h_full().min_h(0.0).child(
        col()
            .key("scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec { stick_bottom: false })
            .child(
                row().w_full().justify_center().px(40.0).pt(28.0).pb(48.0).child(
                    col().w_full().max_w(PAGE_MAX_WIDTH).child(content),
                ),
            ),
    )
}

pub fn empty_state(p: &Palette, msg: &str) -> El {
    row()
        .w_full()
        .py(48.0)
        .justify_center()
        .child(label(msg, SM, p.text_muted).wrap().align(rune::Align::Center))
}

pub fn error_line(p: &Palette, msg: &str) -> El {
    el().w_full().pb(12.0).child(label(msg, SM, p.danger).wrap())
}

/// GPUI `refresh_button`: a small ghost icon button, "Refresh".
pub fn refresh_button(k: &Kit, key: &str, f: impl Fn(&mut State) + 'static) -> El {
    IconButton::new(&crate::icons::REFRESH_CW)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label("Refresh")
        .help("Refresh")
        .on_press(f)
        .build(k)
        .key(key.to_string())
}

/// GPUI `Button` variants: `primary`, `outline`, `ghost`, `danger`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Primary,
    Outline,
    Ghost,
    Danger,
}

/// A small text button (GPUI `Button::new(id).label(..).small()`), with an
/// optional icon. `loading` shows kit's loading look and drops the press.
pub fn button(
    k: &Kit,
    key: &str,
    title: &str,
    look: Look,
    glyph: Option<&'static rune::Icon>,
    loading: bool,
    f: impl Fn(&mut State) + 'static,
) -> El {
    let variant = if loading {
        ButtonVariant::Loading
    } else {
        match look {
            // PORT-STUB(area G): the accent-filled primary look belongs to design/kit.rs.
            Look::Primary => ButtonVariant::Contrast,
            Look::Outline => ButtonVariant::Outline,
            Look::Ghost => ButtonVariant::Ghost,
            Look::Danger => ButtonVariant::Danger,
        }
    };
    let mut b = rune_kit::Button::new(title).variant(variant).control_size(ControlSize::Regular);
    if let Some(i) = glyph {
        b = b.icon(i);
    }
    b.on_press(f).build(k).key(key.to_string())
}

/// A small icon-only ghost button with a tooltip.
pub fn icon_button(k: &Kit, key: &str, glyph: &'static rune::Icon, help: &str, f: impl Fn(&mut State) + 'static) -> El {
    IconButton::new(glyph)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(help)
        .help(help)
        .on_press(f)
        .build(k)
        .key(key.to_string())
}

/// A disabled icon button.
pub fn icon_button_disabled(k: &Kit, key: &str, glyph: &'static rune::Icon, help: &str) -> El {
    IconButton::new(glyph)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(help)
        .disabled(true)
        .build(k)
        .key(key.to_string())
}

/// GPUI `design::kit::hover_wash`. PORT-STUB(area G): use design::kit.
pub fn hover_wash(p: &Palette) -> Color {
    if p.light {
        Color::BLACK.alpha(0.035)
    } else {
        Color::WHITE.alpha(0.04)
    }
}

/// GPUI `design::kit::row`: a hoverable list row, washed when `active`.
/// PORT-STUB(area G): use design::kit.
pub fn kit_row(p: &Palette, key: String, active: bool) -> El {
    row()
        .key(key)
        .w_full()
        .min_h(design::ROW_HEIGHT - 4.0)
        .gap(8.0)
        .px(8.0)
        .center_y()
        .radius(design::ROW_RADIUS)
        .fill(if active { p.active } else { Color::TRANSPARENT })
        .hover(hover_wash(p))
        .cursor(Cursor::Pointer)
}

/// GPUI `design::kit::popover`'s floating card. PORT-STUB(area G).
pub fn floating(k: &Kit, content: El) -> El {
    rune_kit::popover::raised(k, content, design::PANEL_RADIUS)
}

/// A chevron icon in a colour.
pub fn glyph(i: &'static rune::Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color).a11y_hidden()
}

pub(crate) use crate::chat::model_picker::pretty_model;

