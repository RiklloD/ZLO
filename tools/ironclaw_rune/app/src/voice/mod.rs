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

// PORT-STUB(area F): start voice (push-to-talk hotkey, wake word) at launch
// (GPUI `voice::start`).
pub fn start(_s: &mut State) {}

// PORT-STUB(area F): the composer's voice controls (GPUI
// `voice::ui::composer_controls`) for `target`; see
// `workspace::voice_composer`.
pub fn composer_controls(_s: &State, _k: &Kit, _target: crate::workspace::ComposerTarget) -> El {
    rune::el()
}
