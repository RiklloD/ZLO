//! Small building blocks every side tab shares (GPUI `side.rs`'s `empty`,
//! `error_line`, `loading`, `heading`, `scroll`, plus the ghost buttons and
//! hover rows of gpui-kit the tabs used). Area G's `design/kit.rs` may
//! replace these at integration.

use rune::tok::TextStyle;
use rune::{Color, El, Icon, ScrollSpec, col, el, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Spinner};

use crate::design::smooth_scroll::WheelGlide;
use crate::state::State;

/// gpui-kit's `text_size(10)`, `text_xs`, `text_sm`.
pub const TINY: TextStyle = TextStyle::new(10., 400.);
pub const XS: TextStyle = TextStyle::new(12., 400.);
pub const XS_MED: TextStyle = TextStyle::new(12., 500.);
pub const SM: TextStyle = TextStyle::new(13., 400.);
pub const SM_MED: TextStyle = TextStyle::new(13., 500.);
pub const MONO_XS: TextStyle = TextStyle::new(12., 400.).mono();

/// A text in `style` and `color`, truncated to one line.
pub fn line(content: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(content.into()).font(style).color(color).truncate()
}

/// A wrapping text in `style` and `color`.
pub fn para(content: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(content.into()).font(style).color(color).wrap()
}

/// A muted line for empty lists.
pub fn empty(s: &State, content: impl Into<String>) -> El {
    el().w_full()
        .padding(16.)
        .child(para(content, SM, s.palette.text_muted))
}

pub fn error_line(s: &State, content: impl Into<String>) -> El {
    el().w_full()
        .px(12.)
        .py(8.)
        .child(para(content, XS, s.palette.danger))
}

pub fn loading(k: &Kit) -> El {
    row()
        .w_full()
        .justify_center()
        .py(24.)
        .child(Spinner::new().size(16.).build(k))
}

/// A small group heading inside a tab; add trailing children to it.
pub fn heading(s: &State, content: impl Into<String>) -> El {
    row()
        .w_full()
        .px(12.)
        .pt(12.)
        .pb(4.)
        .gap(8.)
        .items_center()
        .child(line(content, XS_MED, s.palette.text_faint))
}

/// A tab's scrolling column. Each tab keys its own, so each keeps its
/// scroll position.
pub fn scroll(key: impl Into<String>, content: El) -> El {
    let key: String = key.into();
    col()
        .key(key.clone())
        .w_full()
        .grow(1.)
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .wheel_glide(key)
        .child(col().w_full().pb(12.).child(content))
}

/// A column that fills the rest of a tab (`v_flex().flex_1().min_h_0()`).
pub fn fill_col() -> El {
    col().w_full().grow(1.).min_h(0.)
}

/// gpui-kit's `Button::new(..).icon(..).ghost().xsmall()` with a tooltip.
pub fn ibtn(icon: &'static Icon, tip: &str) -> IconButton {
    IconButton::new(icon)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(tip)
        .help(tip)
}

/// A small ghost text button.
pub fn btn(label: impl Into<String>) -> Button {
    Button::new(label)
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
}

/// A small outlined text button.
pub fn outline(label: impl Into<String>) -> Button {
    btn(label).variant(ButtonVariant::Outline)
}

/// A small primary text button.
pub fn primary(label: impl Into<String>) -> Button {
    btn(label).variant(ButtonVariant::Contrast)
}

/// Busy buttons show the kit's loading look and take no presses.
pub fn busy(b: Button, loading: bool) -> Button {
    if loading {
        b.variant(ButtonVariant::Loading).disabled(true)
    } else {
        b
    }
}

/// gpui-kit's `.selected(true)` on a ghost button: the active fill behind it.
pub fn selected(s: &State, button: El, on: bool) -> El {
    el().radius(crate::design::CONTROL_RADIUS)
        .fill(if on {
            s.palette.active
        } else {
            Color::TRANSPARENT
        })
        .child(button)
}

/// `kit::row`: a pressable row with the hover wash.
pub fn hover_row(s: &State, key: impl Into<String>) -> El {
    row()
        .key(key)
        .w_full()
        .min_h(28.)
        .px(8.)
        .gap(8.)
        .items_center()
        .radius(crate::design::ROW_RADIUS)
        .hover(s.palette.hover)
        .cursor(rune::Cursor::Pointer)
}

/// A round dot.
pub fn dot(color: Color, size: f32) -> El {
    el().w(size).h(size).radius(size / 2.).fill(color)
}

/// A spacer that takes the row's free width.
pub fn grow() -> El {
    el().grow(1.)
}

/// Hand a file or folder on this Mac to Finder: open it in its app, or
/// reveal it (GPUI `cx.open_with_system`, `cx.reveal_path`). Rune's
/// `Tx::open_url` refuses `file:` links, so this asks `open(1)`.
pub fn open_path(path: &std::path::Path, reveal: bool) {
    let mut cmd = std::process::Command::new("/usr/bin/open");
    if reveal {
        cmd.arg("-R");
    }
    if let Err(e) = cmd.arg(path).spawn() {
        tracing::warn!(error = %e, path = %path.display(), "could not hand the file to Finder");
    }
}

/// An empty diff for `DiffView` getters when the diff they point at is gone
/// (Rune's getters must return a reference).
pub fn empty_diff() -> &'static rune_kit::diff::DiffState {
    thread_local! {
        static EMPTY: &'static rune_kit::diff::DiffState =
            Box::leak(Box::new(rune_kit::diff::DiffState::new(Vec::new())));
    }
    EMPTY.with(|e| *e)
}

/// A throwaway diff for `DiffView`'s mutable getter in the same case. Its
/// handlers only fire while the diff is shown, so this is never reached in
/// practice; the leak is one empty diff.
pub fn spare_diff() -> &'static mut rune_kit::diff::DiffState {
    Box::leak(Box::new(rune_kit::diff::DiffState::new(Vec::new())))
}
