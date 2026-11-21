//! A pane's list → detail drill-down (Outputs and Files → a file's
//! preview, History → a commit): the two levels are keyed siblings, so the
//! one the person opens comes in while the other leaves. The list lives on
//! the left and the detail on the right, so the motion reads as direction
//! without being told it: opening slides the detail in from the right as
//! the list steps out to the left; Back plays it in reverse.
//!
//! Short and calm: a 24 pt nudge and a fade (a full-width push travels too
//! far in a narrow pane and reads as a page turn), exits quicker than
//! entrances. Keyed by level, so a refresh, a poll or a re-render of either
//! level never replays it; under Reduce Motion Rune turns it into its plain
//! fade. It plays on the person's own navigation (a click commits with
//! motion); a level changed by a job lands as Rune lands any job.

use rune::{El, Spring, SpringPair, Transition};

use super::ui;

/// How far a level travels as it comes and goes.
const NUDGE: f32 = 24.;

/// Which level of a drill-down an element is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Level {
    List,
    Detail,
}

impl Level {
    fn side(self) -> f32 {
        match self {
            Level::List => -1.,
            Level::Detail => 1.,
        }
    }

    fn part(self) -> &'static str {
        match self {
            Level::List => "list",
            Level::Detail => "detail",
        }
    }
}

/// A level's transition: in from its own side, out back to it.
pub(super) fn transition(level: Level) -> Transition {
    Transition::offset(NUDGE * level.side(), 0.)
        .combined(Transition::opacity())
        .springs(SpringPair::with_exit_ratio(Spring::new(0.26, 0.0), 0.6))
}

/// `content` as `level` of the drill-down keyed `key` (one per pane).
pub(super) fn level(key: &str, level: Level, content: El) -> El {
    ui::fill_col()
        .key(format!("{key}-{}", level.part()))
        .transition(transition(level))
        .child(content)
}
