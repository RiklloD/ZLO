//! The goals strip (GPUI `chat/goals.rs`). Area H.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct GoalsState {}

// ---- Called by the chat (area A). PORT-STUB(area H): replace with the port
// of `chat/goals.rs`; keep the signatures.

use crate::state::State;

/// PORT-STUB(area H): a parsed `/goal` line (GPUI `goals::GoalCommand`).
pub enum GoalCommand {
    Invalid(String),
}

/// PORT-STUB(area H): `goal_command`, the `/goal …` line as a command.
pub fn goal_command(_text: &str) -> Option<GoalCommand> {
    None
}

/// PORT-STUB(area H): `ChatView::goal_command`: run it (clearing the
/// composer), or show why it is invalid.
pub fn run_goal_command(s: &mut State, id: &str, command: GoalCommand) {
    match command {
        GoalCommand::Invalid(message) => crate::chat::report_error(s, id, message),
    }
}
