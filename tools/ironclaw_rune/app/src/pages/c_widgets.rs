//! Area C's local page helpers (Providers, Extensions, Operator, Skills,
//! Hooks): the GPUI `pages/mod.rs` helpers (`header`, `page_body`,
//! `error_line`, `empty_state`, `refresh_button`), `ui::card`, gpui-kit's
//! `Tag`, and the button sizes the GPUI pages use.
//!
//! PORT-STUB(area D): use pages::common (header, page_body, empty_state,
//! error_line, refresh_button) once it lands.
//! PORT-STUB(area G): use design::kit (card plate, tag) once it lands.

use rune::{Color, El, Icon, ScrollSpec, TextStyle, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};

use crate::design::smooth_scroll::WheelGlide;
use crate::design::{self, Palette};
use crate::icons;

/// Zeron's settings column (GPUI `pages::PAGE_MAX_WIDTH`).
pub const PAGE_MAX_WIDTH: f32 = 760.;

/// GPUI `text_xs` (12 px), `text_sm` (14 px), the body (14 px), `text_lg`
/// (18 px), `text_xl` (20 px).
pub const XS: TextStyle = TextStyle::new(12., 400.);
pub const XS_MEDIUM: TextStyle = TextStyle::new(12., 500.);
pub const SM: TextStyle = TextStyle::new(14., 400.);
pub const SM_MEDIUM: TextStyle = TextStyle::new(14., 500.);
pub const BODY_SEMIBOLD: TextStyle = TextStyle::new(14., 600.);
pub const MONO_XS: TextStyle = TextStyle::new(12., 400.).mono();
pub const TITLE: TextStyle = TextStyle::new(20., 500.);

/// GPUI `page_body`: a centered, width-capped column that scrolls.
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

/// GPUI `header`: the 20 px medium title with page actions on the right.
pub fn header(p: &Palette, title: &str, actions: Vec<El>) -> El {
    row()
        .key("page-header")
        .w_full()
        .justify_between()
        .center_y()
        .px(8.)
        .pb(8.)
        .child(text(title.to_string()).font(TITLE).color(p.text))
        .child(row().gap(8.).center_y().children(actions))
}

/// A section label (GPUI `div().pt_6().pb_2().font_semibold()`).
pub fn section_title(p: &Palette, title: &str, top: f32) -> El {
    text(title.to_string())
        .font(BODY_SEMIBOLD)
        .color(p.text)
        .pt(top)
        .pb(8.)
}

/// GPUI `empty_state`: centered muted text, 48 px of room above and below.
pub fn empty_state(p: &Palette, msg: &str) -> El {
    row()
        .w_full()
        .py(48.)
        .justify_center()
        .child(text(msg.to_string()).font(SM).color(p.text_muted).wrap())
}

/// GPUI `error_line`: danger text, 12 px under it.
pub fn error_line(p: &Palette, msg: &str) -> El {
    text(msg.to_string())
        .font(SM)
        .color(p.danger)
        .wrap()
        .pb(12.)
}

/// A success notice (Providers, Operator).
pub fn notice_line(p: &Palette, msg: &str, color: Color) -> El {
    let _ = p;
    text(msg.to_string()).font(SM).color(color).wrap().pb(12.)
}

/// 12 px muted text that wraps (GPUI operator `muted`).
pub fn muted(p: &Palette, msg: impl Into<String>) -> El {
    text(msg.into()).font(XS).color(p.text_muted).wrap()
}

/// The GPUI `card`: a neutral plate, 12 px padding, 8 px gap, 12 px radius.
pub fn card(p: &Palette) -> El {
    let (fill, rim) = if p.light {
        (p.surface_card, Color::BLACK.alpha(0.07))
    } else {
        (Color::WHITE.alpha(0.07), Color::WHITE.alpha(0.09))
    };
    col()
        .w_full()
        .gap(8.)
        .padding(12.)
        .radius(design::PANEL_RADIUS)
        .fill(fill)
        .border(1., rim)
}

/// A 1 px rule (GPUI `border_t_1` / `border_b_1` with `theme.border`).
pub fn rule(p: &Palette) -> El {
    el().w_full().h(1.).fill(p.border)
}

/// gpui-kit's `Tag` tones.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Danger,
    Secondary,
}

/// gpui-kit `Tag::*().small()`: a small tinted pill.
pub fn tag(p: &Palette, tone: Tone, label: impl Into<String>) -> El {
    let c = match tone {
        Tone::Success => p.success,
        Tone::Warning => p.warning,
        Tone::Danger => p.danger,
        Tone::Secondary => p.text_muted,
    };
    let (fill, ink) = match tone {
        Tone::Secondary => (p.hover, p.text_muted),
        _ => (c.alpha(0.14), c),
    };
    row()
        .h(20.)
        .px(6.)
        .center_y()
        .radius(4.)
        .fill(fill)
        .border(1., c.alpha(0.25))
        .child(text(label.into()).font(XS).color(ink).single_line())
}

/// GPUI button looks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Primary,
    Outline,
    Ghost,
    Danger,
}

/// GPUI sizes: `.small()` is 24 pt, `.xsmall()` 20 pt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Small,
    XSmall,
}

/// A GPUI `Button::new(..).label(..)` in rune-kit: look and size, plus
/// GPUI's `.loading(true)` (Rune's Loading look, no handler).
pub fn button(label: impl Into<String>, look: Look, size: Size) -> Button {
    Button::new(label)
        .variant(match look {
            Look::Primary => ButtonVariant::Submit,
            Look::Outline => ButtonVariant::Outline,
            Look::Ghost => ButtonVariant::Ghost,
            Look::Danger => ButtonVariant::Danger,
        })
        .control_size(match size {
            Size::Small => ControlSize::Regular,
            Size::XSmall => ControlSize::Small,
        })
}

/// GPUI `.loading(on)`: a busy button shows Rune's loading look and drops
/// its handler.
pub fn loading(b: Button, on: bool) -> Button {
    if on {
        b.variant(ButtonVariant::Loading).disabled(true)
    } else {
        b
    }
}

/// A GPUI icon-only ghost button with a tooltip.
pub fn icon_button(glyph: &'static Icon, help: &str, size: Size) -> IconButton {
    IconButton::new(glyph)
        .variant(IconButtonVariant::Ghost)
        .control_size(match size {
            Size::Small => ControlSize::Regular,
            Size::XSmall => ControlSize::Small,
        })
        .label(help.to_string())
        .help(help.to_string())
}

/// GPUI `refresh_button`: the ghost refresh icon with its tooltip.
pub fn refresh_button() -> IconButton {
    icon_button(&icons::REFRESH_CW, "Refresh", Size::Small)
}

/// A small spinner (GPUI `Spinner::new()`).
pub fn spinner(k: &Kit) -> El {
    rune_kit::Spinner::new().size(16.).build(k)
}

/// A spinner with a short line beside it ("Loading models").
pub fn spinner_line(p: &Palette, k: &Kit, msg: &str) -> El {
    row()
        .gap(8.)
        .center_y()
        .child(spinner(k))
        .child(text(msg.to_string()).font(XS).color(p.text))
}

/// A small icon in a colour.
pub fn glyph(i: &'static Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color)
}

/// Open a URL from a handler; failures go to the log.
pub fn open_url(tx: &mut rune::Tx, url: &str) {
    if let Err(e) = tx.open_url(url) {
        tracing::warn!(url, error = ?e, "could not open the URL");
    }
}

/// A GPUI `Input`: a text field that fills its row (rune-kit's default is
/// 280 pt wide).
pub fn input(edit: &rune::TextEdit) -> rune_kit::TextInput {
    rune_kit::TextInput::new(edit).width(None)
}
