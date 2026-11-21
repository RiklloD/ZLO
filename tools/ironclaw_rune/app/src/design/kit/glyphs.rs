//! Small glyphs that move instead of swapping: the rotating disclosure
//! chevron, the copy icon that morphs into a check, a glyph that breathes
//! while its action runs, and the icon button that carries any of them.
//!
//! Each is a keyed element whose change rides Rune: a rotation springs on
//! `rotate_motion`, a swapped icon pops out while its replacement pops in
//! ([`motion::glyph_pop`]), a busy glyph breathes on a [`HALF_FPS`] cycle.
//! Reduce Motion lands the rotation, fades the swap and holds the breath.

use std::f32::consts::{FRAC_PI_2, PI};

use rune::{Color, El, Icon, Tx, el, icon};
use rune_kit::{ControlSize, IconButton, IconButtonVariant, Kit};

use super::loops::{BREATH_35, breathe};
use crate::design::motion;
use crate::icons;
use crate::state::State;

/// Which way a [`chevron`] turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// A disclosure: points right while closed, down while open (an
    /// accordion, a fold, a tree row).
    Disclose,
    /// A dropdown: points down while closed, up while open (a picker, a
    /// select, a menu trigger).
    Flip,
}

/// One chevron glyph that turns between closed and open instead of
/// swapping two icons, on the style's slide spring (`motion::SLIDE`).
/// `key` must be stable for the row it sits in (`format!("{id}-chevron")`):
/// the rotation only springs when the same keyed glyph changes.
///
/// ```ignore
/// row.child(kit::chevron(&format!("{id}-chev"), open, Turn::Disclose, 12., p.text_muted))
/// ```
pub fn chevron(key: &str, open: bool, turn: Turn, size: f32, color: Color) -> El {
    let angle = match (turn, open) {
        (Turn::Disclose, false) => -FRAC_PI_2,
        (Turn::Disclose, true) | (Turn::Flip, false) => 0.,
        (Turn::Flip, true) => PI,
    };
    icon(&icons::CHEVRON_DOWN, size)
        .key(key.to_string())
        .color(color)
        .a11y_hidden()
        .rotate_motion(motion::SLIDE)
        .rotate(angle)
}

/// How long a copy button shows its check (the chat's message copy uses
/// the same).
pub const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1400);

/// Whether a copy made at `at` still shows its check ([`COPIED_FOR`]).
pub fn copied(at: Option<std::time::Instant>) -> bool {
    at.is_some_and(|t| t.elapsed() < COPIED_FOR)
}

/// The copy glyph that becomes a check once something was copied: the copy
/// icon pops out as the check pops in (and back). The caller owns the state:
/// note when it copied (an `Instant` in its slot), pass
/// `copied = at.elapsed() < kit::COPIED_FOR`, and ask for a wake at the
/// deadline (`tx.wake_after(COPIED_FOR)`) so the check reverts; call
/// `tx.animated()` in that wake if the revert should pop too. `size` is the
/// icon size; `key` is the glyph's (unique among its siblings).
///
/// ```ignore
/// kit::copy_glyph(&format!("{id}-copy"), copied, 14., p.text_muted)
/// ```
pub fn copy_glyph(key: &str, copied: bool, size: f32, color: Color) -> El {
    let (glyph, part) = if copied {
        (&icons::CHECK, "done")
    } else {
        (&icons::COPY, "copy")
    };
    el().key(key.to_string())
        .size(size, size)
        .a11y_hidden()
        .child(
            icon(glyph, size)
                .key(part)
                .color(color)
                .absolute(0., 0.)
                .transition(motion::glyph_pop()),
        )
}

/// A glyph that breathes while `busy` (a refresh icon while its list
/// reloads, a sync arrow while it syncs) and rests fully lit otherwise. It
/// breathes on the small loaders' 15 fps clock, only while busy and on
/// screen, and holds still under Reduce Motion and the battery saver.
/// Rune's rotation loops draw at the display's rate, so this breathes rather
/// than spins.
///
/// ```ignore
/// kit::busy_glyph("refresh-glyph", &icons::REFRESH_CW, loading, 16., ink)
/// ```
pub fn busy_glyph(key: &str, glyph: &'static Icon, busy: bool, size: f32, color: Color) -> El {
    let mark = icon(glyph, size)
        .key(key.to_string())
        .color(color)
        .a11y_hidden();
    if busy {
        mark.cycle(breathe(1.2, 0., &BREATH_35))
    } else {
        mark
    }
}

/// An icon with nothing in it: [`glyph_button`] keeps the kit button's
/// metrics and paints its own glyph over this one.
static BLANK: Icon = Icon {
    name: "zlo-blank",
    view: 24.0,
    body: "",
};

/// The ink a kit icon button of `variant` draws its icon in.
pub fn icon_ink(k: &Kit, variant: IconButtonVariant) -> Color {
    match variant {
        IconButtonVariant::GhostMuted => k.c.icon_muted,
        IconButtonVariant::Contrast | IconButtonVariant::Submit => k.c.submit_icon,
        _ => k.c.text_base,
    }
}

/// rune-kit's small icon button (same size, wash, focus ring, tooltip and
/// accessible name `tip`) around a glyph of the caller's (a [`copy_glyph`],
/// a [`busy_glyph`], a [`chevron`]), which kit's own button cannot animate.
/// Draw the glyph at 16 pt in [`icon_ink`]`(k, variant)`. It shrinks while
/// pressed (`motion::PRESS`).
///
/// ```ignore
/// let ink = kit::icon_ink(k, IconButtonVariant::GhostMuted);
/// kit::glyph_button(k, "copy-sha", IconButtonVariant::GhostMuted, "Copy",
///     kit::copy_glyph("g", copied, 16., ink), |s, tx| { tx.copy(sha.clone()); .. })
/// ```
pub fn glyph_button(
    k: &Kit,
    key: &str,
    variant: IconButtonVariant,
    tip: &str,
    glyph: El,
    on_press: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    IconButton::new(&BLANK)
        .variant(variant)
        .control_size(ControlSize::Small)
        .label(tip)
        .help(tip)
        .on_press_tx(on_press)
        .build(k)
        .key(key.to_string())
        .press_scale(motion::PRESS)
        .child(
            el().key("glyph")
                .pin(Some(0.), Some(0.), Some(0.), Some(0.))
                .center()
                .inert()
                .a11y_hidden()
                .child(glyph),
        )
}
