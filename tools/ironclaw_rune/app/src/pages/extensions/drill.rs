//! A settings page's list → detail drill-down (Plugins → a plugin's page,
//! Profiles → a profile), the page-sized twin of the side panes'
//! `chat/side/drill.rs`: the two levels are keyed siblings, so the one the
//! person opens comes in while the other leaves. The list sits on the left
//! and the detail on the right, so opening nudges the detail in from the
//! right as the list steps out to the left; Back plays it in reverse.
//!
//! Short and calm: a 24 pt nudge and a fade (a full-width push travels too
//! far and reads as a page turn), exits quicker than entrances. Keyed by
//! level, so a refresh or a re-render of either level never replays it;
//! under Reduce Motion Rune turns it into its plain fade. It plays on the
//! person's own navigation (a click commits with motion); a level changed by
//! a job lands at once unless that reply asks for motion.
//!
//! Shared by the Plugins and Profiles pages; it and the side panes' copy
//! should move to `design/` together (one helper for both sizes).

use rune::{El, Spring, SpringPair, Transition, col};

/// How far a level travels as it comes and goes.
const NUDGE: f32 = 24.;

/// Which level of a drill-down an element is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Level {
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
fn transition(level: Level) -> Transition {
    Transition::offset(NUDGE * level.side(), 0.)
        .combined(Transition::opacity())
        .springs(SpringPair::with_exit_ratio(Spring::new(0.26, 0.0), 0.6))
}

/// The key `level` of drill-down `key` carries (`{key}-list`, `{key}-detail`).
pub(crate) fn level_key(key: &str, level: Level) -> String {
    format!("{key}-{}", level.part())
}

/// A page's body as `level` of the drill-down keyed `key`, inside the
/// page-filling frame both levels share.
pub(crate) fn page(key: &str, level: Level, body: El) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .h_full()
        .min_h(0.)
        .child(
            col()
                .key(level_key(key, level))
                .w_full()
                .h_full()
                .min_h(0.)
                .transition(transition(level))
                .child(body),
        )
}
