//! Voice: push-to-talk, dictation into the composer, spoken replies, wake
//! word, and the Settings → Voice page.
//!
//! FOUNDATION STUB, replaced by the voice port (PLAN.md area F).

use rune::{Cx, El, Tx, col, text};
use rune_kit::Kit;

use crate::state::State;

#[derive(Default)]
pub struct VoiceState {}

pub fn poll(_s: &mut State, _tx: &mut Tx) -> bool {
    false
}

/// Settings → Voice became visible.
pub fn settings_opened(_s: &mut State) {}

/// Settings → Voice (GPUI `pages/settings/voice.rs`).
pub fn settings_view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    col().w_full().h_full().center().child(text("Voice").color(s.palette.text_muted))
}

// ---- Called by the chat (area A). PORT-STUB(area F): replace with the
// voice port; keep the signatures. A composer is named by its key
// (`ChatState::composer_key`, "composer:<thread>").

/// Talk mode's claim on one message (GPUI `voice::turn::Submission`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Submission(pub u64);

/// PORT-STUB(area F): the composer that handles an Enter takes talk mode's
/// claim on it, if talk mode pressed it there.
pub fn take_submission(_s: &mut State, _composer: &str) -> Option<Submission> {
    None
}

/// PORT-STUB(area F): the gateway took talk mode's message into `thread` as `run`.
pub fn submission_accepted(_s: &mut State, _sub: Submission, _thread: &str, _run: Option<String>, _composer: &str) {}

/// PORT-STUB(area F): talk mode's message waits in `thread`'s queue behind `behind`.
pub fn submission_queued(_s: &mut State, _sub: Submission, _thread: &str, _behind: Option<String>, _composer: &str) {}

/// PORT-STUB(area F): talk mode's message was not sent: say why, leave talk mode.
pub fn submission_refused(_s: &mut State, _sub: Submission, _why: &str) {}

/// PORT-STUB(area F): a chat's run ended with `rows` on screen; talk mode or
/// read-aloud may speak it.
pub fn run_ended(
    _s: &mut State,
    _thread: &str,
    _run: Option<&str>,
    _completed: bool,
    _rows: &[ironclaw_gpui_client::transcript::Row],
) {
}

/// PORT-STUB(area F): the composer's mic, talk button and voice state pill
/// (GPUI `voice::ui::composer_controls`).
pub fn composer_controls(_s: &State, _composer: &str, _thread: Option<&str>, _k: &Kit) -> El {
    rune::el()
}
