//! Small building blocks every side tab shares. Area G's `design/kit.rs` may
//! replace these at integration.

use crate::design::kit::Spinner;
use crate::design::motion;
use rune::tok::TextStyle;
use std::cell::RefCell;
use std::collections::HashMap;

use rune::{
    Color, El, GrowthSpec, Icon, ScrollPos, ScrollSource, ScrollSpec, Tx, col, el, icon, row, text,
};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, RollingText,
};

use crate::state::State;

/// The 10, 12 and 14 pt text sizes, scaled to Appearance →
/// UI font size (`styles::rem`); code ([`MONO_XS`]) keeps its size.
fn at(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(crate::design::styles::rem(size), weight)
}
pub fn tiny() -> TextStyle {
    at(10., 400.)
}
pub fn micro_semi() -> TextStyle {
    at(10.5, 600.)
}
pub fn xs() -> TextStyle {
    at(12., 400.)
}
pub fn xs_med() -> TextStyle {
    at(12., 500.)
}
pub fn xs_semi() -> TextStyle {
    at(12., 600.)
}
pub fn sm() -> TextStyle {
    at(13., 400.)
}
pub fn sm_med() -> TextStyle {
    at(13., 500.)
}
pub fn sm_semi() -> TextStyle {
    at(13.5, 600.)
}
pub const MONO_XS: TextStyle = TextStyle::new(12., 400.).mono();

/// A text in `style` and `color`, truncated to one line.
pub fn line(content: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(content.into()).font(style).color(color).truncate()
}

/// A wrapping text in `style` and `color`.
pub fn para(content: impl Into<String>, style: TextStyle, color: Color) -> El {
    text(content.into())
        .font(style)
        .color(color)
        .wrap()
        .w_full()
}

/// A muted line for empty lists; fades in and out as the list empties or
/// fills (`motion::fade`).
pub fn empty(s: &State, content: impl Into<String>) -> El {
    el().w_full()
        .transition(motion::fade())
        .padding(16.)
        .child(para(content, sm(), s.palette.text_muted))
}

/// A structured empty-state card with an accented icon badge, clear headline,
/// concise body copy, and an optional direct action button.
pub fn empty_card(
    s: &State,
    glyph: &'static Icon,
    title: impl Into<String>,
    subtitle: impl Into<String>,
    action: Option<El>,
) -> El {
    let p = &s.palette;
    let badge = el()
        .size(36., 36.)
        .radius(10.)
        .center()
        .fill(p.accent_wash)
        .border(1., p.accent.alpha(0.22))
        .child(icon(glyph, 18.).color(p.accent));
    let header = row().w_full().gap(12.).items_center().child(badge).child(
        col()
            .grow(1.)
            .min_w(0.)
            .gap(2.)
            .child(line(title, sm_semi(), p.text))
            .child(para(subtitle, xs(), p.text_muted)),
    );
    let mut card = col()
        .w_full()
        .padding(14.)
        .gap(12.)
        .radius(crate::design::PANEL_RADIUS)
        .continuous_corners()
        .fill(p.surface_card.alpha(if p.light { 0.85 } else { 0.65 }))
        .border(1., p.border)
        .child(header);
    if let Some(action) = action {
        card = card.child(action);
    }
    el().w_full()
        .px(12.)
        .pt(12.)
        .transition(motion::fade())
        .child(card)
}

/// Compact uppercase section label used inside side-pane launchpads.
pub fn section_label(s: &State, content: impl Into<String>) -> El {
    el().w_full().px(4.).pt(6.).pb(2.).child(line(
        content.into().to_uppercase(),
        micro_semi(),
        s.palette.text_faint,
    ))
}

/// Compact count pill for tab buttons and headers. Its tint morphs as it
/// turns active, and a changed count rolls to the new figure
/// (`rune_kit::RollingText`, keyed `count`): pass counts that change on
/// events, never on a clock.
pub fn count_pill(s: &State, count: usize, active: bool, accent: bool) -> El {
    let p = &s.palette;
    let (bg, fg) = if accent {
        (p.accent_wash, p.accent)
    } else if active {
        (
            if p.light {
                Color::BLACK.alpha(0.08)
            } else {
                Color::WHITE.alpha(0.12)
            },
            p.text,
        )
    } else {
        (
            if p.light {
                Color::BLACK.alpha(0.05)
            } else {
                Color::WHITE.alpha(0.07)
            },
            p.text_muted,
        )
    };
    let label = if count > 99 {
        "99+".into()
    } else {
        count.to_string()
    };
    el().h(16.)
        .min_w(16.)
        .px(5.)
        .radius(8.)
        .center()
        .fill(bg)
        .motion(motion::SLIDE)
        .child(rolling(s, "count", label, count as f64, micro_semi(), fg))
}

/// A `RollingText` label in an explicit font and colour, so it needs no
/// per-call `Kit` (one is kept per thread for the builder's defaults). It
/// rolls on a calm spring with no overshoot; `value` sets the direction
/// (rising rolls up, falling down).
fn rolling(s: &State, key: &str, label: String, value: f64, font: TextStyle, color: Color) -> El {
    thread_local! {
        static KIT: std::cell::OnceCell<Kit> = const { std::cell::OnceCell::new() };
    }
    KIT.with(|kit| {
        let k = kit.get_or_init(|| crate::design::kit(&s.palette));
        RollingText::new(key, label)
            .font(font)
            .color(color)
            .value(value)
            .spring(motion::ROLL)
            .build(k)
    })
}

/// A request's error in danger red: wipes open from its top and fades
/// (`motion::reveal`) when it appears after a press, and out when cleared.
pub fn error_line(s: &State, content: impl Into<String>) -> El {
    el().w_full()
        .px(12.)
        .py(8.)
        .transition(motion::reveal())
        .child(para(content, xs(), s.palette.danger))
}

/// The tab's loading spinner; it fades out as the content replaces it
/// (`motion::fade`).
pub fn loading(k: &Kit) -> El {
    row()
        .w_full()
        .justify_center()
        .py(24.)
        .transition(motion::fade())
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
        .child(line(content, xs_med(), s.palette.text_faint))
}

thread_local! {
    /// Where each tab's scrolling column was last scrolled to, by its key.
    /// Rune drops a viewport's offset when it unmounts (a tab switch, a dock
    /// closing), so the tab puts it back when it mounts again.
    static SCROLL_AT: RefCell<HashMap<String, f32>> = RefCell::new(HashMap::new());
}

/// Makes the scroll viewport keyed `key` keep its offset across unmounts:
/// it records every move (not the 0 a fresh mount reports before it is
/// scrolled) and scrolls back there, instantly, when it mounts again.
fn keeps_offset(viewport: El, key: String) -> El {
    let record = key.clone();
    viewport
        .on_scroll_offset(move |at: ScrollPos| {
            if at.source != ScrollSource::None {
                SCROLL_AT.with(|m| m.borrow_mut().insert(record.clone(), at.offset[1]));
            }
        })
        .on_mount(move |_: &mut State, tx: &mut Tx| {
            let y = SCROLL_AT.with(|m| m.borrow().get(&key).copied());
            if let Some(y) = y.filter(|y| *y > 0.) {
                tx.scroll_to_instant(key.clone(), 0., y);
            }
        })
}

/// A tab's scrolling column. Each tab keys its own, so each keeps its
/// scroll position ([`keeps_offset`]).
pub fn scroll(key: impl Into<String>, content: El) -> El {
    let key: String = key.into();
    let viewport = col()
        .key(key.clone())
        .w_full()
        .grow(1.)
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(col().w_full().pb(12.).child(content));
    keeps_offset(viewport, key)
}

/// [`scroll`] over a long list as Rune virtual rows: `count` rows, row `i`
/// keyed `key(s, i)` and built by `build(s, i)` only while it is near the
/// viewport. Rows carry their own spacing (no stack gap); the last one gets
/// the column's bottom room.
pub fn virtual_scroll(
    scroll_key: impl Into<String>,
    count: usize,
    estimate: f32,
    key: impl Fn(&State, usize) -> String + 'static,
    build: impl Fn(&State, usize) -> El + 'static,
) -> El {
    let scroll_key: String = scroll_key.into();
    let viewport = col()
        .key(scroll_key.clone())
        .w_full()
        .grow(1.)
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .key("rows")
                .w_full()
                .growth(GrowthSpec::default())
                .whole_rows()
                .virtual_rows(
                    count,
                    estimate,
                    move |s: &State, i| key(s, i).into(),
                    move |s: &State, i| {
                        let row = build(s, i);
                        if i + 1 == count {
                            col().w_full().pb(12.).child(row)
                        } else {
                            row
                        }
                    },
                ),
        );
    keeps_offset(viewport, scroll_key)
}

/// A column that fills the rest of a tab (`v_flex().flex_1().min_h_0()`).
pub fn fill_col() -> El {
    col().w_full().grow(1.).min_h(0.)
}

/// An extra-small ghost icon button with a tooltip.
pub fn ibtn(icon: &'static Icon, tip: &str) -> IconButton {
    // Muted ink, as the titlebar's toggles and tabs: the pane's own buttons
    // sit quieter than its content.
    IconButton::new(icon)
        .variant(IconButtonVariant::GhostMuted)
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

/// A ghost button shown selected: the active fill behind
/// it, which morphs in and out on the slide spring.
pub fn selected(s: &State, button: El, on: bool) -> El {
    el().radius(crate::design::CONTROL_RADIUS)
        .continuous_corners()
        .motion(motion::SLIDE)
        .fill(if on {
            s.palette.active
        } else {
            Color::TRANSPARENT
        })
        .child(button)
}

/// `kit::row`: a pressable row with the hover wash, which fades in and out;
/// a press washes and sinks it a little (`motion::PRESS_CARD`). Give its
/// text `.group_color(..)` to brighten with the hover.
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
        .press(s.palette.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .hover_group()
        .motion(motion::SLIDE)
        .press_scale(motion::PRESS_CARD)
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

/// Hand a file or folder on this Mac to Finder: open it in its app, or reveal
/// it. Rune's `Tx::open_url` refuses `file:` links, so this asks `open(1)`.
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

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
