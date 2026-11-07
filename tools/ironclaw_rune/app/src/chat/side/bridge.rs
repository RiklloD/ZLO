//! What the side pane needs from the rest of the chat (areas A and H) and
//! the workspace (area B). In the GPUI app these were fields and methods of
//! the one `ChatView`; here each need is one function over the open chat
//! (`s.chat`), so the side pane's files call only these.

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::orchestration::{Lineage, TaskView};
use ironclaw_gpui_client::transcript::{CliBody, Row};
use rune::El;
use rune_kit::Kit;

use crate::chat::{self, ChatState};
use crate::state::State;

fn open(s: &State) -> Option<&ChatState> {
    s.chat.as_ref()
}

/// The open chat's thread id.
fn open_id(s: &State) -> Option<String> {
    open(s).map(|c| c.thread_id.clone())
}

/// The open chat's transcript rows, oldest first (GPUI `ChatView::rows`).
pub fn rows(s: &State) -> &[Row] {
    open(s).map_or(&[], |c| c.rows.as_slice())
}

/// A run of the open chat is going (GPUI `ChatView::running_run().is_some()`).
pub fn running(s: &State) -> bool {
    open(s).is_some_and(|c| c.running_run().is_some())
}

/// Stop the open chat's running run (GPUI `ChatView::stop`).
pub fn stop(s: &mut State) {
    if let Some(id) = open_id(s) {
        chat::stop_run(s, &id);
    }
}

/// Answer an approval or question gate (GPUI `ChatView::resolve_gate`).
pub fn resolve_gate(s: &mut State, run_id: String, gate_ref: String, decision: GateDecision) {
    let Some(id) = open_id(s) else { return };
    chat::with_tx(s, &id, move |s, id, tx| {
        chat::actions::resolve_gate(s, id, run_id, gate_ref, decision, tx)
    });
}

/// Open the "No, do X instead" editor for a gate (GPUI `ChatView::start_reject`).
pub fn start_reject(s: &mut State, run_id: String, gate_ref: String) {
    let Some(id) = open(s).map(|c| c.thread_id.clone()) else { return };
    chat::reject_feedback::start_reject(s, &id, run_id, gate_ref);
}

/// The reject editor under a gate card, when it was opened for `gate_ref`
/// (GPUI `reject_feedback::render_reject_editor`).
pub fn reject_editor(s: &State, k: &Kit, gate_ref: &str) -> Option<El> {
    chat::reject_feedback::render_reject_editor(open(s)?, gate_ref, k)
}

/// Show an error in the open chat's error strip (GPUI `ChatView::set_error`).
pub fn set_error(s: &mut State, message: String) {
    chat::open_error(s, message);
}

/// Insert `text` at the composer's cursor and focus it (GPUI
/// `composer.update(|s| { s.focus(); s.insert(text) })`).
pub fn composer_insert(s: &mut State, text: &str) {
    let Some(c) = s.chat.as_mut() else { return };
    c.composer.focused = true;
    c.composer.insert(text);
    let id = c.thread_id.clone();
    chat::composer::on_change(s, &id);
}

/// The composer's draft is empty (decides the gap before inserted text).
pub fn composer_empty(s: &State) -> bool {
    open(s).is_none_or(|c| c.composer.text.is_empty())
}

/// Attach files on this Mac to the next message (GPUI `ChatView::attach`).
pub fn attach(s: &mut State, paths: Vec<std::path::PathBuf>) {
    let Some(id) = open(s).map(|c| c.thread_id.clone()) else { return };
    chat::actions::attach(s, &id, paths);
}

/// The thread's runtime, `Some("codex")` on Codex threads (GPUI `ChatView::thread_runtime`).
pub fn thread_runtime(s: &State) -> Option<&str> {
    open(s)?.orchestration.queue.runtime.as_deref()
}

/// The latest turn diff a CLI agent reported (GPUI `transcript.cli_turn_diff()`).
pub fn cli_turn_diff(s: &State) -> Option<&CliBody> {
    open(s)?.transcript.cli_turn_diff()
}

/// The latest finished CLI review (GPUI `transcript.cli_review()`).
pub fn cli_review(s: &State) -> Option<&CliBody> {
    open(s)?.transcript.cli_review()
}

/// Re-read the timeline after a runtime action (GPUI `ChatView::load_timeline`).
pub fn load_timeline(s: &mut State) {
    chat::reload_timeline(s);
}

/// The model the composer will use, for the session card (GPUI `picker.model_label()`).
pub fn model_label(s: &State) -> String {
    open(s).map_or_else(|| "Default model".into(), |c| c.model_picker.model_label())
}

/// The profile the chat runs under: (display name, effective folder, git
/// branch), for the session card (GPUI `approval.profile()`).
pub fn profile(s: &State) -> Option<(String, String, Option<String>)> {
    let info = open(s)?.approval.profile.as_ref()?;
    Some((
        info.settings.display_name.clone(),
        info.effective_folder.clone(),
        info.git_branch.clone(),
    ))
}

/// The orchestrator's tasks for this thread (GPUI `ChatView::orch.tasks`).
pub fn tasks(s: &State) -> &[TaskView] {
    open(s).map_or(&[], |c| c.orchestration.tasks.as_slice())
}

/// The thread's lineage: forks and where it was forked from (GPUI `orch.lineage`).
pub fn lineage(s: &State) -> Option<&Lineage> {
    open(s).map(|c| &c.orchestration.lineage)
}

/// Cancel a delegated task (GPUI `ChatView::cancel_task`).
pub fn cancel_task(s: &mut State, task_id: String) {
    if let Some(id) = open_id(s) {
        chat::orchestration::cancel_task(s, &id, task_id);
    }
}

/// Fork the open conversation (GPUI `ChatView::fork(None)`).
pub fn fork(s: &mut State) {
    if let Some(id) = open_id(s) {
        chat::orchestration::fork(s, &id, None);
    }
}

/// Re-read the orchestration state now (GPUI `ChatView::refresh_orchestration`).
pub fn refresh_orchestration(s: &mut State) {
    if let Some(id) = open_id(s) {
        chat::orchestration::refresh(s, &id);
    }
}

/// The open chat's computer-use share has a session, and whether its
/// screen shows (GPUI `computer.has_session()`, `!computer.hidden()`).
pub fn computer_share(s: &State) -> Option<bool> {
    let id = open_id(s)?;
    chat::computer::has_session(s, &id).then(|| !chat::computer::hidden(s, &id))
}

/// Show or hide the open chat's computer-use screen share (GPUI
/// `share.toggle_hidden`).
pub fn toggle_computer(s: &mut State) {
    if let Some(id) = open_id(s) {
        chat::computer::toggle_hidden(s, &id);
    }
}

/// A thread's title from the shared title cache (GPUI `titles::get`).
pub fn title(s: &State, thread_id: &str) -> Option<String> {
    crate::titles::get(s, thread_id)
}

/// Fetch titles for these threads into the cache (GPUI `titles::ensure`).
pub fn ensure_titles(s: &mut State, ids: Vec<String>) {
    crate::titles::ensure(s, ids);
}

/// The open side chat has a running run (GPUI `side_chat_running`).
pub fn side_chat_running(s: &State) -> bool {
    open(s)
        .and_then(|c| c.side.chat.as_ref())
        .is_some_and(|c| c.state.running_run().is_some())
}
