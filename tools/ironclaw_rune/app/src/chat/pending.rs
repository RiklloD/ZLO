//! The working line under a live run (GPUI `chat/flow.rs` `render_pending`
//! and the flavour words of `design/motion.rs`): a pulsing glyph, a verb
//! that rolls every 7 s and the elapsed seconds.

use std::time::Instant;

use rune::{El, col, el, row};
use rune_kit::{Kit, RollingText};

use super::flow::{SPACE_SM, rise, type_at};
use crate::design::Palette;
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
/// the elapsed seconds, rolling as they tick.
pub fn render_pending(s: &State, stopping: bool, since: Option<Instant>, seed: u64, k: &Kit) -> El {
    let p = &s.palette;
    let secs = since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
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
        .child(
            RollingText::new("working-timer", format_elapsed(secs))
                .font(type_at(s, 11., 14., 400.))
                .color(p.text_faint)
                .build(k)
                .mt(1.),
        )
}

/// The GPUI gradient spinner's three tints, as a 2×2 grid of cells that
/// pulse in turn.
fn working_glyph(p: &Palette) -> El {
    const TINTS: [u32; 3] = [0xB6D3EF, 0xEDB185, 0xF888A0];
    let cell = 5.;
    let gap = cell / 2.;
    let dot = |i: usize| {
        let c = crate::design::rgb(TINTS[i % 3]);
        el().size(cell, cell).radius(1.5).fill(c).looping(rune::LoopSpec {
            period: 1.2,
            delay: i as f64 * 0.3,
            kind: rune::LoopKind::Opacity { low: 0.35, high: 1. },
        })
    };
    let _ = p;
    col().key("working-glyph")
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
