//! The open thread: transcript, composer, run state and the right pane.
//!
//! FOUNDATION STUB. The chat port (PLAN.md, areas A, H, S) replaces this with
//! the full port of `chat.rs` and `chat/*`. Keep the public surface below
//! (`ChatState::thread_id`, `open`, `view`, `poll`); add to it freely.

pub mod computer;
pub(crate) mod extras_ui;
pub mod goals;
pub mod model_picker;
pub mod orchestration;
pub mod side;

use rune::{Cx, El, Tx, col, row, text};
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
    // PORT-STUB(area A): keep these three calls in the real `open`.
    model_picker::start(s);
    orchestration::opened(s);
    computer::start(s);
}

pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    // PORT-STUB(area A): keep this call in the real `poll`.
    computer::poll(s, tx)
}

/// The main column for `Page::Chat` and `Page::Draft`.
pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let Some(c) = &s.chat else {
        return col().w_full().h_full().center().child(text("No thread open").color(p.text_muted));
    };
    extras_preview(s, c, cx, k, layer)
}

/// PORT-STUB(area A): a stand-in chat column that places the chat extras
/// (area H) where the real chat does, so their scenes render: the
/// transcript's tail and the screen share over the transcript, the
/// orchestration strip above the composer, the model pills in it. Area A
/// replaces this with the real transcript and composer and keeps the calls.
fn extras_preview(s: &State, c: &ChatState, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let width = s.prefs.conversation_width.clamp(480., 1200.);
    let mut transcript = col()
        .key("transcript")
        .grow(1.)
        .min_h(0.)
        .w_full()
        .px(16.)
        .pb(16.)
        .justify_end()
        .items_center()
        .child(
            col()
                .w_full()
                .max_w(width)
                .gap(16.)
                .child(text(format!("Thread {}", c.thread_id)).color(p.text_muted))
                .children(orchestration::tail(s, k)),
        );
    if let Some(share) = computer::view(s, cx, k, layer) {
        transcript = transcript.child(share);
    }
    let (fill, rim) = extras_ui::composer_surface(p);
    let composer = row()
        .key("composer")
        .w_full()
        .max_w(width)
        .h(49.)
        .px(8.)
        .center_y()
        .radius(crate::design::BUBBLE_RADIUS)
        .fill(fill)
        .border(1., rim)
        .child(text("Ask anything").color(p.text_faint).grow(1.).pl(8.))
        .child(model_picker::pills(s, cx, k, layer));
    col()
        .w_full()
        .h_full()
        .min_h(0.)
        .child(transcript)
        .children(orchestration::composer(s, k, None))
        .child(row().w_full().px(16.).pt(8.).pb(16.).justify_center().child(composer))
}

// PORT-STUB(area A): hooks the chat extras (area H) call. Area A replaces
// these bodies with the real chat behaviour; keep the names and signatures.

/// Show `message` in the chat's error banner (GPUI `ChatView::set_error`).
pub fn set_error(_s: &mut State, message: String) {
    tracing::warn!(%message, "chat error");
}

/// Re-open the thread's event stream (its runtime changed).
pub fn resubscribe(_s: &mut State) {}

/// Re-read the open thread's timeline.
pub fn reload_timeline(_s: &mut State) {}

/// The run streaming in the open chat (IronClaw's loop), if any.
pub fn active_run(_s: &State) -> Option<String> {
    None
}

/// The open chat is a side chat embedded in a pane.
pub fn embedded(_s: &State) -> bool {
    false
}

/// The composer's text, and a way to put text back into it.
pub fn composer_text(_s: &State) -> String {
    String::new()
}
pub fn set_composer_text(_s: &mut State, _text: String) {}

/// A fork's source conversation, shown above its own messages.
pub fn set_fork_prefix(
    _s: &mut State,
    _prefix: Vec<ironclaw_gpui_client::wire::TimelineMessage>,
) {
}

/// Forks merged back into this thread, shown where they arrived.
pub fn set_merges(_s: &mut State, _merges: Vec<ironclaw_gpui_client::orchestration::MergedFork>) {}

/// Stop the open chat's turn (the computer-use Stop does this too).
pub fn stop_run(_s: &mut State) {}
