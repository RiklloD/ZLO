//! The working line under a live run (GPUI `chat/flow.rs` `render_pending`
//! and the flavour words of `design/motion.rs`): a pulsing glyph, a verb
//! that rolls every 7 s and the elapsed seconds.

use std::time::Instant;

use rune::{El, LiveText, col, el, row, text};
use rune_kit::{Kit, RollingText};

use super::flow::{SPACE_SM, rise, type_at};
use crate::design::Palette;
use crate::design::kit::loops::{BREATH_35, breathe};
use crate::state::State;

/// The words that rotate on the working line, every
/// [`FLAVOUR_ROTATE_SECS`] seconds (GPUI `design/motion.rs`).
const FLAVOUR_WORDS: [&str; 21] = [
    "Clawing",
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];
const FLAVOUR_ROTATE_SECS: u64 = 7;

/// Zeron's working line: a pulsing glyph, a verb that rolls every 7 s and
/// the elapsed seconds.
pub fn render_pending(
    s: &State,
    stopping: bool,
    since: Option<Instant>,
    clock: &LiveText,
    seed: u64,
    k: &Kit,
) -> El {
    let p = &s.palette;
    let secs = since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    // The poll keeps the label current; a line built before its first tick
    // (or for a new run) starts it here. Equal text wakes nothing.
    clock.set(format_elapsed(secs));
    let word = if stopping {
        "Stopping".to_string()
    } else {
        flavour_word(seed, secs).to_string()
    };
    row()
        .key("working")
        .gap(SPACE_SM)
        .center_y()
        .h(24.)
        .transition(rise(4.))
        .child(working_glyph(p))
        .child(
            RollingText::new("working-word", format!("{word}…"))
                .font(type_at(s, 12., 16., 400.))
                .color(p.text_muted)
                .build(k),
        )
        // Plain text, not a `RollingText`: a roll every second kept the
        // window drawing at the display's full rate for most of each second
        // of a run, for a number nobody watches tick. Live text: each tick
        // repaints the label in a box sized for the widest it gets before
        // the line is built again (`clock_view`); nothing sits after it.
        .child(
            text(clock_placeholder(secs))
                .live_text(clock)
                .key("working-timer")
                .font(type_at(s, 11., 14., 400.))
                .color(p.text_faint)
                .mt(1.),
        )
}

/// A 2×2 grid of cells that pulse in turn, in the muted ink: the motion
/// says it is working, without three more colours on the page.
fn working_glyph(p: &Palette) -> El {
    let cell = 5.;
    let gap = cell / 2.;
    let dot = |i: usize| {
        let c = p.text_muted;
        el().size(cell, cell)
            .radius(1.5)
            .fill(c)
            .cycle(breathe(1.2, i as f32 * 0.3, &BREATH_35))
    };
    col()
        .key("working-glyph")
        .gap(gap)
        .child(row().gap(gap).child(dot(0)).child(dot(1)))
        .child(row().gap(gap).child(dot(3)).child(dot(2)))
}

pub fn flavour_word(seed: u64, elapsed_secs: u64) -> &'static str {
    let step = elapsed_secs / FLAVOUR_ROTATE_SECS;
    FLAVOUR_WORDS[(seed.wrapping_add(step) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// FNV-1a, a stable seed from a thread id.
pub fn seed(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// What the working line shows besides its seconds changes with this: the
/// word rolls every [`FLAVOUR_ROTATE_SECS`], and the label's box
/// ([`clock_placeholder`]) widens past the hour. Between two changes, ticks
/// only repaint the live label.
pub fn clock_view(secs: u64) -> (u64, bool) {
    (secs / FLAVOUR_ROTATE_SECS, secs >= 3_600)
}

/// Text at least as wide as any label until the next [`clock_view`]
/// change: under an hour the widest is "59m 59s", after it "99h 59m". The
/// UI faces' digits are proportional, so each has a spare digit's room.
fn clock_placeholder(secs: u64) -> &'static str {
    if secs < 3_600 {
        "000m 000s"
    } else {
        "0000h 000m"
    }
}

/// "12s", "3m 4s", "1h 2m": at most two units.
pub fn format_elapsed(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3_600, (secs % 3_600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_labels_use_two_units() {
        assert_eq!(format_elapsed(12), "12s");
        assert_eq!(format_elapsed(184), "3m 4s");
        assert_eq!(format_elapsed(3_720), "1h 2m");
    }
}
