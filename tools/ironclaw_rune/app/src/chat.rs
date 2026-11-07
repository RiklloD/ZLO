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

// ---------------------------------------------------------------------------
// Voice and SnapShot hooks (area F calls these; the owners fill them in).
// ---------------------------------------------------------------------------

/// The composer field voice dictates into, if that composer exists now.
/// `Chat(id)` is the open thread's composer when `id` is open; `Draft` and
/// `Quick` are area B's new-thread and quick composers.
// PORT-STUB(area A): return the open chat's composer `TextEdit` for
// `Composer::Chat(id)` when `s.chat` is that thread; route `Draft`/`Quick`
// to area B's draft and quick composer (GPUI `Workspace::voice_composer`).
pub fn voice_composer<'a>(
    _s: &'a State,
    _composer: &crate::voice::Composer,
) -> Option<&'a rune::TextEdit> {
    None
}

/// [`voice_composer`], mutable: voice inserts committed words at the caret
/// and shows the still-changing tail as the field's preedit.
// PORT-STUB(area A): same routing as `voice_composer`.
pub fn voice_composer_mut<'a>(
    _s: &'a mut State,
    _composer: &crate::voice::Composer,
) -> Option<&'a mut rune::TextEdit> {
    None
}

/// Send what `composer` holds, exactly as pressing Enter in it does (GPUI
/// emitted `InputEvent::PressEnter`). The send handler must call
/// `crate::voice::take_submission(s, composer)` synchronously and report
/// the outcome with `voice::submission_{accepted,queued,refused}`.
// PORT-STUB(area A): call the chat composer's send for `Chat(id)`; route
// `Draft`/`Quick` to area B's send.
pub fn voice_press_enter(_s: &mut State, _composer: &crate::voice::Composer, _tx: &mut Tx) {}

/// A SnapShot finished: stage it where the person is writing (GPUI
/// `Workspace::stage_snapshot` + `ChatView::stage_snapshot`). Leave Settings
/// for the threads first; on `Page::Chat` stage into the open chat with
/// `crate::snapshot::stage_into(&mut s.snapshot, &mut chat.staged, snap,
/// &limits)`; otherwise open the new-thread screen and stage there (area
/// B). An `Err` (or a failed `stage_into`) becomes the composer's error.
// PORT-STUB(area A, area B): routing above; until then the error is logged.
pub fn stage_snapshot(_s: &mut State, snap: Result<crate::snapshot::Snapshot, String>) {
    if let Err(e) = snap {
        tracing::warn!(error = %e, "SnapShot failed");
    }
}
