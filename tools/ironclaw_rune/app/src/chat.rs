//! The open thread: transcript, composer, run state and the right pane.
//!
//! FOUNDATION STUB. The chat port (PLAN.md, areas A, H, S) replaces this with
//! the full port of `chat.rs` and `chat/*`. Keep the public surface below
//! (`ChatState::thread_id`, `open`, `view`, `poll`); add to it freely.

pub mod computer;
pub mod goals;
pub mod model_picker;
pub mod orchestration;
pub mod side;

use rune::{Cx, El, Tx, col, text};
use rune_kit::Kit;

use crate::state::State;

/// One open thread. Areas H and S own the sub-states below; area A owns the
/// rest of this struct.
pub struct ChatState {
    pub thread_id: String,
    pub side: side::SideState,
    pub model_picker: model_picker::ModelPickerState,
    pub goals: goals::GoalsState,
    pub orchestration: orchestration::OrchestrationState,
    pub computer: computer::ComputerState,
}

impl ChatState {
    pub fn new(thread_id: String) -> Self {
        Self {
            thread_id,
            side: Default::default(),
            model_picker: Default::default(),
            goals: Default::default(),
            orchestration: Default::default(),
            computer: Default::default(),
        }
    }
}

/// Open `thread_id` (replacing the open chat). Called by the workspace.
pub fn open(s: &mut State, thread_id: String) {
    s.chat = Some(ChatState::new(thread_id));
}

pub fn poll(_s: &mut State, _tx: &mut Tx) -> bool {
    false
}

/// The main column for `Page::Chat` and `Page::Draft`.
pub fn view(s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let label = match &s.chat {
        Some(c) => format!("Thread {}", c.thread_id),
        None => "No thread open".into(),
    };
    col().w_full().h_full().center().child(text(label).color(p.text_muted))
}

// PORT-STUB(area A): GPUI `chat::approval_mode::changed`: the global
// auto-approve switch changed; approval pills re-read it.
pub fn approval_mode_changed(_s: &mut State) {}
