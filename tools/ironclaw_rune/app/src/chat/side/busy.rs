//! The panes' small icon buttons while their request is in flight: the
//! pressed button's glyph breathes (`kit::busy_glyph`, 15 fps, only while
//! busy, still under Reduce Motion) and takes no press; its siblings for
//! the same item dim as before. At rest they are the panes' usual muted
//! ghost buttons and shrink a hair while pressed.

use rune::El;
use rune_kit::{IconButtonVariant, Kit};

use super::ui;
use crate::design::{kit, motion};
use crate::state::State;

/// Where a button's request stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Busy {
    Idle,
    /// Another request for the same item runs: dimmed, no press.
    Blocked,
    /// This button's own request runs: its glyph breathes.
    Running,
}

/// A pane icon button (`ui::ibtn`'s look) that shows its own request.
pub(super) fn icon_button(
    k: &Kit,
    key: String,
    glyph: &'static rune::Icon,
    tip: &str,
    busy: Busy,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    match busy {
        Busy::Idle => ui::ibtn(glyph, tip)
            .on_press(on_press)
            .build(k)
            .key(key)
            .press_scale(motion::PRESS),
        Busy::Blocked => ui::ibtn(glyph, tip).disabled(true).build(k).key(key),
        Busy::Running => {
            let ink = kit::icon_ink(k, IconButtonVariant::GhostMuted);
            kit::glyph_button(
                k,
                &key,
                IconButtonVariant::GhostMuted,
                tip,
                kit::busy_glyph("busy", glyph, true, 16., ink),
                |_, _| {},
            )
        }
    }
}

/// A pane's refresh button: breathes while its list reloads.
pub(super) fn refresh(
    k: &Kit,
    key: &str,
    label: &str,
    loading: bool,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    let busy = if loading { Busy::Running } else { Busy::Idle };
    icon_button(
        k,
        key.to_string(),
        &crate::icons::REFRESH_CW,
        "Refresh",
        busy,
        on_press,
    )
    .label(label.to_string())
}
