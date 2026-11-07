//! Orchestration: delegation tree, forks, side chats in the transcript (GPUI `chat/orchestration.rs`). Area H.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct OrchestrationState {}

// ---- Called by the chat (area A). PORT-STUB(area H): replace the bodies
// with the port of `chat/orchestration.rs`; keep the signatures. `id` is the
// chat's thread id; find the chat with `crate::chat::chat_for(s, id)`.

use rune::El;
use rune_kit::Kit;

use crate::chat::ChatState;
use crate::state::State;

/// PORT-STUB(area H): the queue's external run (a CLI runtime turn the
/// stream does not report), for `ChatState::running_run`.
pub fn external_run(_o: &OrchestrationState) -> Option<&str> {
    None
}

/// PORT-STUB(area H): `refresh_orchestration`: reload tasks, queue, lineage
/// and goal, and arm the slow poll.
pub fn refresh(_s: &mut State, _id: &str) {}

/// PORT-STUB(area H): `ensure_poll`: start the slow poll if something moves.
pub fn ensure_poll(_s: &mut State, _id: &str) {}

/// PORT-STUB(area H): whether the transcript ends with the tail row (live
/// subagents, the goal, queued turns): GPUI `render_tail(..).is_some()`.
pub fn has_tail(_s: &State, _id: &str) -> bool {
    false
}

/// PORT-STUB(area H): `fork`: a new thread holding the history up to run
/// `at_run` (1-based), opened when it exists.
pub fn fork(_s: &mut State, _id: &str, _at_run: Option<u32>) {}

/// PORT-STUB(area H): `queue_follow_up`: ⌥Enter's message waits in the
/// queue. `true` when it was queued (the composer then clears).
pub fn queue_follow_up(_s: &mut State, _id: &str, _text: String) -> bool {
    false
}

/// PORT-STUB(area H): `render_orchestration`, what stays on the composer:
/// the plan strip first (`crate::chat::plan_strip::view`), then the goal
/// editor, the queue's controls and the strip. Return the inner column; the
/// chat centres it in the reading column.
pub fn panel(s: &State, chat: &ChatState, _cx: &mut rune::Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> Option<El> {
    crate::chat::plan_strip::view(s, chat)
}

/// PORT-STUB(area H): `render_tail`, the transcript's last row.
pub fn tail(_s: &State, _chat: &ChatState, _k: &Kit, _layer: &mut Vec<El>) -> Option<El> {
    None
}

/// PORT-STUB(area H): `tail_summary`, "2 working · 1 queued" for the jump
/// button.
pub fn tail_summary(_chat: &ChatState) -> Option<String> {
    None
}
