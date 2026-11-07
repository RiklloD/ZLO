//! What the side pane needs from the rest of the chat (areas A and H) and
//! the workspace (area B). In the GPUI app these were fields and methods of
//! the one `ChatView`; here the chat's state belongs to other areas, so each
//! need is one function. Every body below is a stand-in until the owning
//! area's port lands: the integrator replaces it with the real call.
//!
//! Keep the signatures; the side pane's files call only these.

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::orchestration::{Lineage, TaskView};
use ironclaw_gpui_client::transcript::{CliBody, Row};
use rune::El;
use rune_kit::Kit;

use crate::state::State;

/// The open chat's transcript rows, oldest first (GPUI `ChatView::rows`).
// PORT-STUB(area A): return the open chat's live rows (scenes read fixtures).
pub fn rows(s: &State) -> &[Row] {
    match super::fixtures::data() {
        Some(d) if s.fixture => &d.rows,
        _ => &[],
    }
}

/// A run of the open chat is going (GPUI `ChatView::running_run().is_some()`).
// PORT-STUB(area A): true while the open chat has a running run.
pub fn running(s: &State) -> bool {
    s.fixture
        && rows(s)
            .iter()
            .any(|r| matches!(r, Row::Tool { status: ironclaw_gpui_client::transcript::ToolStatus::Running, .. }))
}

/// Stop the open chat's running run (GPUI `ChatView::stop`).
// PORT-STUB(area A): stop the running run.
pub fn stop(_s: &mut State) {}

/// Answer an approval or question gate (GPUI `ChatView::resolve_gate`).
// PORT-STUB(area A): resolve the gate through the chat's own path.
pub fn resolve_gate(_s: &mut State, _run_id: String, _gate_ref: String, _decision: GateDecision) {}

/// Open the "No, do X instead" editor for a gate (GPUI `ChatView::start_reject`).
// PORT-STUB(area A): start the reject-with-feedback editor for this gate.
pub fn start_reject(_s: &mut State, _run_id: String, _gate_ref: String) {}

/// The reject editor under a gate card, when it was opened for `gate_ref`
/// (GPUI `reject_feedback::render_reject_editor`).
// PORT-STUB(area A): render the reject editor for this gate, or None.
pub fn reject_editor(_s: &State, _k: &Kit, _gate_ref: &str) -> Option<El> {
    None
}

/// Show an error in the open chat's error strip (GPUI `ChatView::set_error`).
// PORT-STUB(area A): route to the chat's error strip.
pub fn set_error(_s: &mut State, message: String) {
    tracing::warn!(%message, "side pane error (chat error strip not wired yet)");
}

/// Insert `text` at the composer's cursor and focus it (GPUI
/// `composer.update(|s| { s.focus(); s.insert(text) })`).
// PORT-STUB(area A): insert into the open chat's composer.
pub fn composer_insert(_s: &mut State, _text: &str) {}

/// The composer's draft is empty (decides the gap before inserted text).
// PORT-STUB(area A): read the composer's text.
pub fn composer_empty(_s: &State) -> bool {
    true
}

/// Attach files on this Mac to the next message (GPUI `ChatView::attach`).
// PORT-STUB(area A): stage these files in the composer.
pub fn attach(_s: &mut State, _paths: Vec<std::path::PathBuf>) {}

/// The thread's runtime, `Some("codex")` on Codex threads (GPUI `ChatView::thread_runtime`).
// PORT-STUB(area A): the thread's runtime id.
pub fn thread_runtime(_s: &State) -> Option<&str> {
    None
}

/// The latest turn diff a CLI agent reported (GPUI `transcript.cli_turn_diff()`).
// PORT-STUB(area A): the open chat's `Transcript::cli_turn_diff`.
pub fn cli_turn_diff(_s: &State) -> Option<&CliBody> {
    None
}

/// The latest finished CLI review (GPUI `transcript.cli_review()`).
// PORT-STUB(area A): the open chat's `Transcript::cli_review`.
pub fn cli_review(_s: &State) -> Option<&CliBody> {
    None
}

/// Re-read the timeline after a runtime action (GPUI `ChatView::load_timeline`).
// PORT-STUB(area A): reload the open chat's timeline.
pub fn load_timeline(_s: &mut State) {}

/// The model the composer will use, for the session card (GPUI `picker.model_label()`).
// PORT-STUB(area H): the model picker's label.
pub fn model_label(_s: &State) -> String {
    "Default model".into()
}

/// The profile the chat runs under: (display name, effective folder, git
/// branch), for the session card (GPUI `approval.profile()`).
// PORT-STUB(area A): the approval-mode control's profile.
pub fn profile(_s: &State) -> Option<(String, String, Option<String>)> {
    None
}

/// The orchestrator's tasks for this thread (GPUI `ChatView::orch.tasks`).
// PORT-STUB(area H): `OrchestrationState` tasks (scenes read fixtures).
pub fn tasks(s: &State) -> &[TaskView] {
    match super::fixtures::data() {
        Some(d) if s.fixture => &d.tasks,
        _ => &[],
    }
}

/// The thread's lineage: forks and where it was forked from (GPUI `orch.lineage`).
// PORT-STUB(area H): `OrchestrationState` lineage.
pub fn lineage(_s: &State) -> Option<&Lineage> {
    None
}

/// Cancel a delegated task (GPUI `ChatView::cancel_task`).
// PORT-STUB(area H): cancel through the orchestration port.
pub fn cancel_task(_s: &mut State, _task_id: String) {}

/// Fork the open conversation (GPUI `ChatView::fork(None)`).
// PORT-STUB(area H): fork through the orchestration port.
pub fn fork(_s: &mut State) {}

/// Re-read the orchestration state now (GPUI `ChatView::refresh_orchestration`).
// PORT-STUB(area H): refresh the orchestration poll.
pub fn refresh_orchestration(_s: &mut State) {}

/// The computer-use share has a session, and whether its screen shows
/// (GPUI `computer.has_session()`, `!computer.hidden()`).
// PORT-STUB(area H): the computer-use share state.
pub fn computer_share(_s: &State) -> Option<bool> {
    None
}

/// Show or hide the computer-use screen share (GPUI `share.toggle_hidden`).
// PORT-STUB(area H): toggle the share's visibility.
pub fn toggle_computer(_s: &mut State) {}

/// A thread's title from the shared title cache (GPUI `titles::get`).
// PORT-STUB(area B): `titles::get`.
pub fn title(_s: &State, _thread_id: &str) -> Option<String> {
    None
}

/// Fetch titles for these threads into the cache (GPUI `titles::ensure`).
// PORT-STUB(area B): `titles::ensure`.
pub fn ensure_titles(_s: &mut State, _ids: Vec<String>) {}

/// The side chat open in the right pane, drawn as an embedded chat with no
/// docks of its own (GPUI `ChatView::new_side_chat_view`). Its sends,
/// links (`side::open_link`), "open as main chat" and run-ended events go
/// through the parent as before.
// PORT-STUB(area A): render an embedded ChatState for `thread_id`.
pub fn side_chat_view(s: &State, _k: &Kit, _layer: &mut Vec<El>, thread_id: &str) -> El {
    super::ui::empty(s, format!("Side chat {thread_id}"))
}

/// The open side chat has a running run (GPUI `side_chat_running`).
// PORT-STUB(area A): read the embedded chat's run state.
pub fn side_chat_running(_s: &State) -> bool {
    false
}
