//! Computer use: live screen, steps and approvals (GPUI `chat/computer.rs`). Area H.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct ComputerState {}

// ---- Called by the chat (area A). PORT-STUB(area H): replace with the port
// of `chat/computer.rs`; keep the signatures. Its Stop also stops the turn:
// call `crate::chat::actions::stop(s, id, tx)`.

use rune::El;
use rune_kit::Kit;

use crate::chat::ChatState;
use crate::state::State;

/// PORT-STUB(area H): the chat opened: watch the thread's screen share.
pub fn opened(_s: &mut State, _id: &str) {}

/// PORT-STUB(area H): the computer-use share above the composer.
pub fn view(_s: &State, _chat: &ChatState, _cx: &mut rune::Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> Option<El> {
    None
}
