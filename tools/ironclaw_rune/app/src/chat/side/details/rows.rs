//! The card's row vocabulary and its surface, so every section draws the
//! same way: one icon column, one label edge, meta right-aligned in the UI
//! font with tabular digits, one hover and keyboard fill.
//!
//! - [`base`]: a row's frame; [`glyph`], [`label`], [`meta`], [`counts`].
//! - [`pressable`]: the shared hover (eased), press and keyboard-focus fill,
//!   and a stop in the card's ↑/↓ focus group.
//! - [`split`]: a row with an action and a chevron menu; → opens the menu,
//!   ← closes it.
//! - [`surface`]: the opaque floating card with its scale-and-fade from the
//!   toggle's corner.

use rune::tok::TextStyle;
use rune::{Color, Cursor, El, Key, SpringPair, Transition, Vec2, el, icon, row};

use super::super::ui::{self, sm, sm_med, xs};
use super::{Dropdown, ROW_H, close_menus, open_dropdown};
use crate::design::styles::Style;
use crate::state::State;

/// Rows' inner padding, so glyphs sit on one column.
pub(super) const PAD: f32 = 8.;
const GLYPH: f32 = 16.;
const RADIUS: f32 = 7.;

pub(super) fn base(key: impl Into<String>) -> El {
    row()
        .key(key)
        .w_full()
        .h(ROW_H)
        .px(PAD)
        .gap(10.)
        .items_center()
        .radius(RADIUS)
}

pub(super) fn glyph(s: &State, g: &'static rune::Icon) -> El {
    icon(g, GLYPH).color(s.palette.text_muted)
}

/// A row's label: regular weight, so the header and the section labels
/// carry the hierarchy; `live` false greys it.
pub(super) fn label(s: &State, content: impl Into<String>, live: bool) -> El {
    let p = &s.palette;
    ui::line(content, sm(), if live { p.text } else { p.text_faint })
        .grow(1.)
        .min_w(0.)
}

/// The header's emphasised line.
pub(super) fn title(s: &State, content: impl Into<String>) -> El {
    ui::line(content, sm_med(), s.palette.text).min_w(0.)
}

/// Meta in the UI font with tabular digits, so counts line up.
pub(super) fn digits() -> TextStyle {
    TextStyle {
        features: "'tnum' 1",
        ..xs()
    }
}

pub(super) fn meta(s: &State, content: impl Into<String>) -> El {
    ui::line(content, digits(), s.palette.text_faint)
}

/// `+N −N`, each coloured only when it is not zero; "No changes" when both
/// are.
pub(super) fn counts(s: &State, additions: usize, deletions: usize) -> El {
    let p = &s.palette;
    if additions == 0 && deletions == 0 {
        return meta(s, "No changes").key("counts");
    }
    let n = |n: usize, sign: &str, color: Color| {
        ui::line(
            format!("{sign}{n}"),
            digits(),
            if n > 0 { color } else { p.text_faint },
        )
    };
    row()
        .key("counts")
        .gap(6.)
        .items_center()
        .child(n(additions, "+", p.success))
        .child(n(deletions, "\u{2212}", p.danger))
}

/// A row that acts: the eased hover wash, a press wash, and the same wash
/// while the keyboard focuses it (no ring: the fill shows focus).
pub(super) fn pressable(s: &State, e: El) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    e.role("button")
        .cursor(Cursor::Pointer)
        .hover(p.hover)
        .press(p.active)
        .hover_motion(st.hover, st.hover)
        .press_scale(crate::design::motion::PRESS_CARD)
        .focusable(true)
        .focus_fill(p.hover)
        .no_focus_ring()
}

/// ← and → over a row with a menu: → opens it, ← closes it.
pub(super) fn arrow_keys(e: El, which: Dropdown) -> El {
    e.on_key(move |s: &mut State, ev, _| match ev.key {
        Key::Right if !ev.mods.cmd => {
            open_dropdown(s, which);
            true
        }
        Key::Left if !ev.mods.cmd => {
            close_menus(s);
            true
        }
        _ => false,
    })
}

/// A row split into its action and the chevron that opens its menu: one
/// row with a hairline between the two, the chevron inside the row's edge.
pub(super) fn split(s: &State, key: &str, primary: El, which: Dropdown, open: bool) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let chevron = el()
        .key(format!("{key}-chevron"))
        .role("button")
        .label("More options")
        .w(26.)
        .h(ROW_H)
        .center()
        .radius(RADIUS)
        .cursor(Cursor::Pointer)
        .hover(p.hover)
        .press(p.active)
        .hover_motion(st.hover, st.hover)
        .fill(if open { p.active } else { Color::TRANSPARENT })
        .focusable(false)
        .on_press(move |s: &mut State| super::toggle_dropdown(s, which))
        // Turns over as its menu opens and back as it shuts.
        .child(crate::design::kit::chevron(
            "glyph",
            open,
            crate::design::kit::Turn::Flip,
            14.,
            if open { p.text } else { p.text_faint },
        ));
    arrow_keys(
        row()
            .key(key.to_string())
            .w_full()
            .h(ROW_H)
            .items_center()
            .gap(2.)
            .child(primary)
            .child(el().w(1.).h(14.).fill(p.border).a11y_hidden())
            .child(chevron),
        which,
    )
}

/// The card's plane: the popover material at full opacity, so the
/// conversation (or a wallpaper) never shows through, with the hairline rim
/// and soft shadow of ZLO's other floating surfaces. It grows out of the
/// toggle's corner (top right) and shrinks back into it.
pub(super) fn surface(s: &State, card: El) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let mut plate = crate::design::kit::floating(&st, p);
    plate.fill = p.surface_overlay;
    plate.hover = Color::TRANSPARENT;
    plate
        .apply(card.radius(st.panel_radius.clamp(10., 12.)))
        .transition(
            Transition::scale_anchor(0.95, Vec2::new(1., 0.))
                .springs(SpringPair::with_exit_ratio(crate::design::styles::POP, 0.6)),
        )
}
