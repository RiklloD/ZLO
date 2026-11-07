//! The rest of the shared page kit: GPUI's two button sizes, keyed
//! ready-built controls (button, icon button, refresh, setting row) for
//! views that key every control, text in a [`rune::TextStyle`], rules,
//! hover rows, tags, spinners and the floating card. Folded in from the
//! areas' local copies (`pages/c_widgets.rs`, `pages/usage/common_stub.rs`).

use rune::{Color, Cursor, El, Icon, TextStyle, col, el, row, text};
use rune_kit::{Button, ControlSize, IconButton, IconButtonVariant, Kit};

use super::ts;
use super::{Btn, Tone, button, header, label as sized_label};
use crate::design::{self, Palette};
use crate::state::State;

/// GPUI's button looks (the pages' primary, outline, ghost, danger).
pub type Look = Btn;

/// GPUI sizes: `.small()` is 24 pt, `.xsmall()` 20 pt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Small,
    XSmall,
}

fn control_size(size: Size) -> ControlSize {
    match size {
        Size::Small => ControlSize::Regular,
        Size::XSmall => ControlSize::Small,
    }
}

/// A GPUI `Button::new(..).label(..)` at a GPUI size.
pub fn button_sized(label: impl Into<String>, look: Look, size: Size) -> Button {
    let label: String = label.into();
    button(&label, look).control_size(control_size(size))
}

/// A GPUI icon-only ghost button at a GPUI size, with a tooltip that also
/// names it.
pub fn icon_button_sized(glyph: &'static Icon, help: &str, size: Size) -> IconButton {
    IconButton::new(glyph)
        .variant(IconButtonVariant::Ghost)
        .control_size(control_size(size))
        .label(help.to_string())
        .help(help.to_string())
}

/// GPUI `refresh_button` at `.small()`: the ghost refresh icon.
pub fn refresh_icon() -> IconButton {
    icon_button_sized(&crate::icons::REFRESH_CW, "Refresh", Size::Small)
}

/// A small text button (GPUI `.small()`), keyed and built, with an optional
/// icon. `loading` shows the kit's loading look and drops the press.
pub fn press_button(
    k: &Kit,
    key: &str,
    title: &str,
    look: Look,
    glyph: Option<&'static Icon>,
    loading: bool,
    f: impl Fn(&mut State) + 'static,
) -> El {
    let mut b = super::loading(button(title, look), loading);
    if let Some(i) = glyph {
        b = b.icon(i);
    }
    if !loading {
        b = b.on_press(f);
    }
    b.build(k).key(key.to_string())
}

/// A small ghost icon button with a tooltip, keyed and built.
pub fn press_icon(
    k: &Kit,
    key: &str,
    glyph: &'static Icon,
    help: &str,
    f: impl Fn(&mut State) + 'static,
) -> El {
    super::icon_button(glyph, help)
        .on_press(f)
        .build(k)
        .key(key.to_string())
}

/// [`press_icon`] that cannot be pressed.
pub fn press_icon_disabled(k: &Kit, key: &str, glyph: &'static Icon, help: &str) -> El {
    super::icon_button(glyph, help)
        .disabled(true)
        .build(k)
        .key(key.to_string())
}

/// GPUI `refresh_button`, keyed and built.
pub fn keyed_refresh(k: &Kit, key: &str, f: impl Fn(&mut State) + 'static) -> El {
    super::refresh_button(k, f).key(key.to_string())
}

/// Text in a style and colour.
pub fn styled(s: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(s.into()).font(style).color(color)
}

/// 12 px muted text that wraps (GPUI operator `muted`).
pub fn muted(p: &Palette, msg: impl Into<String>) -> El {
    styled(msg, ts::xs(), p.text_muted).wrap()
}

/// [`header`] with several actions, 8 px apart.
pub fn header_with(p: &Palette, title: &str, actions: Vec<El>) -> El {
    header(p, title, row().gap(8.).center_y().children(actions))
}

/// A section label (GPUI `div().pt_6().pb_2().font_semibold()`).
pub fn section_title(p: &Palette, title: &str, top: f32) -> El {
    styled(title, ts::body_semibold(), p.text).pt(top).pb(8.)
}

/// One setting under a caller's key (see [`super::setting_row`]).
pub fn keyed_setting_row(
    p: &Palette,
    key: &str,
    title: &str,
    description: Option<&str>,
    control: El,
) -> El {
    let mut left =
        col()
            .grow(1.0)
            .min_w(160.)
            .gap(1.)
            .child(sized_label(title, super::SM, 500., p.text));
    if let Some(d) = description {
        left = left.child(
            text(d)
                .font(super::font(super::XS, 400.).lh(crate::design::styles::rem(18.)))
                .color(p.text_muted)
                .wrap(),
        );
    }
    row()
        .key(key.to_string())
        .w_full()
        .min_h(60.)
        .px(16.)
        .py(12.)
        .gap(16.)
        .center_y()
        .child(left)
        .child(col().shrink().child(control))
}

/// A 1 px rule in the border colour, softened (between rows).
pub fn hairline(p: &Palette) -> El {
    el().h(1.)
        .fill(p.border.alpha(p.border.a * 0.6))
        .a11y_hidden()
}

/// A full-strength 1 px rule, GPUI's `border_t_1`.
pub fn rule(p: &Palette) -> El {
    el().w_full().h(1.).fill(p.border).a11y_hidden()
}

/// gpui-kit `Tag::*().small()`: a small tinted pill with a rim.
pub fn outlined_tag(p: &Palette, tone: Tone, label: impl Into<String>) -> El {
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
        .child(styled(label, ts::xs(), ink).single_line())
}

/// GPUI `design::kit::row` as the pickers draw it: a hoverable list row,
/// washed when `active`.
pub fn hover_row(p: &Palette, key: String, active: bool) -> El {
    row()
        .key(key)
        .w_full()
        .min_h(design::ROW_HEIGHT - 4.)
        .gap(8.)
        .px(8.)
        .center_y()
        .radius(design::ROW_RADIUS)
        .fill(if active { p.active } else { Color::TRANSPARENT })
        .hover(design::kit::hover_wash(p))
        .cursor(Cursor::Pointer)
}

/// GPUI `design::kit::popover`'s floating card around `content`.
pub fn floating(k: &Kit, content: El) -> El {
    rune_kit::popover::raised(k, content, design::PANEL_RADIUS)
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
        .child(styled(msg, ts::xs(), p.text))
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
