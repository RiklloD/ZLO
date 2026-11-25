//! The live turn's two lines, t3code's: "Working for 1m 3s" under the
//! prompt (the working row, a hairline under it) and the thinking line that
//! closes the turn while nothing else shows the run at work.

use std::time::Instant;

use rune::{El, LiveText, col, el, row, text};
use rune_kit::Kit;

use super::flow::{SPACE_SM, rise, type_at};
use crate::state::State;

/// The working row: "Working for" and the run's elapsed time, which the
/// poll repaints in place every second (`clock`), over a hairline.
pub fn render_working(s: &State, stopping: bool, since: Option<Instant>, clock: &LiveText) -> El {
    let p = &s.palette;
    let secs = since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    // The poll keeps the label current; a line built before its first tick
    // (or for a new run) starts it here. Equal text wakes nothing.
    clock.set(format_elapsed(secs));
    let font = type_at(s, 13., 20., 400.);
    let lead = if stopping {
        "Stopping after"
    } else {
        "Working for"
    };
    col()
        .key("working")
        .w_full()
        .transition(rise(4.))
        .child(
            row()
                .h(24.)
                .gap(4.)
                .center_y()
                .px(2.)
                .child(text(lead).font(font).color(p.text_muted))
                // Live text: each tick repaints the label in a box sized for
                // the widest it gets before the line is built again
                // (`clock_view`).
                // Rune refreshes the accessibility tree when live text
                // changes, so VoiceOver reads the current time.
                .child(
                    text(clock_placeholder(secs))
                        .live_text(clock)
                        .key("working-timer")
                        .font(font)
                        .color(p.text_muted),
                ),
        )
        .child(el().w_full().h(1.).mt(4.).fill(p.border))
}

/// The thinking line that closes the live turn: a breathing glyph and
/// "Thinking" ("Stopping" once Stop was pressed).
pub fn render_pending(s: &State, stopping: bool, _k: &Kit) -> El {
    let p = &s.palette;
    let font = type_at(s, 13., 20., 400.);
    row()
        .key("thinking")
        .gap(6.)
        .center_y()
        .h(24.)
        .transition(rise(4.))
        .child(
            el().w(20.)
                .h(24.)
                .center()
                .child(crate::design::kit::busy_glyph(
                    "thinking-glyph",
                    &crate::icons::BOT,
                    true,
                    14.,
                    p.text_muted,
                )),
        )
        .child(
            text(if stopping { "Stopping" } else { "Thinking" })
                .font(font)
                .color(p.text_muted)
                .cycle(crate::design::kit::loops::breathe(
                    1.6,
                    0.,
                    &crate::design::kit::loops::BREATH_55,
                )),
        )
        .mt(SPACE_SM / 4.)
}

/// What the working row shows besides its time changes with this: the
/// label's box ([`clock_placeholder`]) widens past the hour. Between two
/// changes, ticks only repaint the live label.
pub fn clock_view(secs: u64) -> bool {
    secs >= 3_600
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
