//! Orchestration around the open thread: the subagent frame above the
//! composer, the context strip (delegated from, forked from with Merge back,
//! usage-limit countdown, paused after Stop, interrupted with Continue,
//! queued follow-ups), the queue panel and the Fork action. Port of GPUI
//! `chat/orchestration.rs`; the views are in `orchestration/view.rs`.
//!
//! Refresh is event-driven: on open, when the thread's stream reports a run
//! end or a timeline reload, and after every mutation here. While a turn
//! runs or a child task is live, a slow poll re-reads tasks and queue, and
//! only while the app window is in front. An idle thread schedules nothing.
//!
//! Every chat has its own: the open one and each side chat in its right
//! pane (GPUI: each `ChatView` owned its `orch`). Functions take the chat's
//! thread id and find it again with `chat::chat_for`; replies for a chat
//! that closed meanwhile are dropped.
//!
//! # Entry points (area A calls these)
//!
//! - `opened(s, id)`: after a chat starts.
//! - `refresh(s, id)`: when the stream reports a run end or a timeline reload.
//! - `ensure_poll(s, id)`: after a send starts a run.
//! - `queue_follow_up(s, id, text) -> bool`: the composer's ⌥Enter.
//! - `fork(s, id, at_run: Option<u32>)`, `cancel_task(s, id, task_id)`.
//! - `running_run(s, id)`, `thread_runtime(s, id)`, `tail_summary(s, id)`
//!   (the jump-to-end button).
//! - Views: `composer(s, chat, k, plan) -> Option<El>` above the composer
//!   (pass the plan strip in), `tail(s, chat, k) -> Option<El>` as the
//!   transcript's last row.

mod view;

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::orchestration::{ForkOptions, Lineage, QueueAction, QueueDoc, TaskView};
use rune::TextEdit;

use crate::jobs;
use crate::state::State;

pub use view::{composer, tail};

/// Poll period while something is moving. Also the granularity of elapsed
/// and countdown labels, which only repaint when data arrives.
const POLL: Duration = Duration::from_secs(4);

#[derive(Default)]
pub struct OrchestrationState {
    pub tasks: Vec<TaskView>,
    pub(crate) queue: QueueDoc,
    pub lineage: Lineage,
    lineage_loaded: bool,
    pub(crate) queue_open: bool,
    pub(crate) queue_input: Option<TextEdit>,
    /// A mutation is in flight; its buttons wait for it.
    pub(crate) busy: bool,
    /// Merge-back outcome, shown in the strip until the next action.
    pub(crate) note: Option<String>,
    loading: bool,
    /// Another refresh was asked for while one was in flight.
    stale: bool,
    polling: bool,
}

thread_local! {
    /// Child thread → the task that created it, for threads delegated from a
    /// thread opened in this session. The routes give a parent its children
    /// but not a child its parent, so this is how a child knows "delegated
    /// from" (GPUI's `Delegations` global). UI thread only.
    static DELEGATIONS: RefCell<HashMap<String, TaskView>> = RefCell::new(HashMap::new());
}

/// The task that delegated `thread_id`, if this session saw it.
pub(crate) fn delegation(thread_id: &str) -> Option<TaskView> {
    DELEGATIONS.with(|d| d.borrow().get(thread_id).cloned())
}

struct Snapshot {
    tasks: Result<Vec<TaskView>, ClientError>,
    queue: Result<QueueDoc, ClientError>,
    lineage: Option<Result<Lineage, ClientError>>,
}

/// The orchestration of the chat showing `thread_id` (main or side), only
/// while that chat is still open.
fn slot<'a>(s: &'a mut State, thread_id: &str) -> Option<&'a mut OrchestrationState> {
    crate::chat::chat_for(s, thread_id).map(|c| &mut c.orchestration)
}

/// The chat opened: read everything once.
pub fn opened(s: &mut State, id: &str) {
    refresh(s, id);
}

/// The runtime the thread runs on (`codex`, `claude-cli`, `ironclaw`), once
/// the orchestrator has seen it.
pub fn thread_runtime(s: &State, id: &str) -> Option<String> {
    crate::chat::chat_ref(s, id)?.orchestration.queue.runtime.clone()
}

/// Re-read tasks and queue (and lineage until loaded), and the goal.
/// Coalesces: a call during a refresh only marks it stale.
pub fn refresh(s: &mut State, id: &str) {
    crate::chat::goals::load(s, id);
    let Some(o) = slot(s, id) else { return };
    let thread_id = id.to_string();
    if o.loading {
        o.stale = true;
        return;
    }
    o.loading = true;
    // A merge waiting for the next message turns sent once it is.
    let want_lineage = !o.lineage_loaded || o.lineage.merges_in.iter().any(|m| m.status == "pending");
    let asked = thread_id.clone();
    jobs::run(
        s,
        "orchestration",
        move |c| async move {
            let (tasks, queue) = tokio::join!(c.thread_tasks(&thread_id), c.thread_queue(&thread_id));
            let lineage = match want_lineage {
                true => Some(c.thread_lineage(&thread_id).await),
                false => None,
            };
            Ok(Snapshot { tasks: tasks.map(|t| t.tasks), queue, lineage })
        },
        move |s, result: Result<Snapshot, ClientError>, _| match result {
            Ok(snapshot) => apply_snapshot(s, &asked, snapshot),
            Err(e) => {
                tracing::debug!(error = %e, "orchestration refresh failed");
                if let Some(o) = slot(s, &asked) {
                    o.loading = false;
                }
            }
        },
    );
}

fn apply_snapshot(s: &mut State, asked: &str, snap: Snapshot) {
    let embedded = crate::chat::embedded(s, asked);
    let Some(o) = slot(s, asked) else { return };
    let mut reload_timeline = false;
    let (mut load_prefix, mut load_merge_list) = (false, false);
    o.loading = false;
    // silent-ok: a thread the orchestrator has not seen yet (no message
    // sent) answers 404, as does an older gateway; both mean "nothing".
    match snap.tasks {
        Ok(tasks) => o.tasks = tasks,
        Err(e) if e.is_not_found() => o.tasks.clear(),
        Err(e) => tracing::debug!(error = %e, "thread tasks refresh failed"),
    }
    match snap.queue {
        Ok(queue) => {
            // An external turn shows up only here: reload its transcript
            // while it runs and once more when it ends.
            if queue.external_run().is_some() || o.queue.external_run().is_some() {
                reload_timeline = true;
            }
            o.queue = queue;
        }
        Err(e) if e.is_not_found() => o.queue = QueueDoc::default(),
        Err(e) => tracing::debug!(error = %e, "thread queue refresh failed"),
    }
    match snap.lineage {
        Some(Ok(lineage)) => {
            let forked = lineage.forked_from.is_some();
            let merged = !lineage.merges_in.is_empty();
            o.lineage = lineage;
            o.lineage_loaded = true;
            // A side chat shows only its own messages; its parent is beside it.
            load_prefix = forked && !embedded;
            load_merge_list = merged;
        }
        Some(Err(e)) => tracing::debug!(error = %e, "thread lineage refresh failed"),
        None => {}
    }
    DELEGATIONS.with(|d| {
        let mut d = d.borrow_mut();
        if let Some(task) = &o.lineage.delegated_from {
            d.insert(asked.to_string(), task.clone());
        }
        for task in &o.tasks {
            if let Some(child) = &task.child_thread_id {
                d.insert(child.clone(), task.clone());
            }
        }
    });
    let mut related: Vec<String> = Vec::new();
    if let Some(parent) = delegation(asked).map(|t| t.parent_thread_id) {
        related.push(parent);
    }
    if let Some(from) = &o.lineage.forked_from {
        related.push(from.source_thread.clone());
    }
    let stale = std::mem::take(&mut o.stale);
    crate::workspace::ensure_titles(s, related);
    if load_prefix {
        load_fork_prefix(s, asked);
    }
    if load_merge_list {
        load_merges(s, asked);
    }
    if reload_timeline {
        crate::chat::load_timeline(s, asked);
    }
    if stale {
        refresh(s, asked);
    }
    ensure_poll(s, asked);
}

/// A fork shows its source conversation above its own messages.
fn load_fork_prefix(s: &mut State, id: &str) {
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.fork_prefix(&thread_id).await },
        move |s, result, _| match result {
            Ok(prefix) => crate::chat::set_fork_prefix(s, &asked, prefix),
            Err(e) => tracing::debug!(error = %e, "fork source load failed"),
        },
    );
}

/// Forks merged back into this thread show where they arrived.
fn load_merges(s: &mut State, id: &str) {
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.merged_in(&thread_id).await },
        move |s, result, _| match result {
            Ok(merges) => crate::chat::set_merges(s, &asked, merges),
            Err(e) => tracing::debug!(error = %e, "merged forks load failed"),
        },
    );
}

fn needs_poll(s: &State, id: &str) -> bool {
    let Some(c) = crate::chat::chat_ref(s, id) else { return false };
    let o = &c.orchestration;
    crate::chat::active_run(s, id).is_some()
        || o.queue.external_run().is_some()
        || o.tasks.iter().any(|t| t.status.is_live())
        || c.goals.check_in_flight()
}

/// Whether [`tail`] has anything to show for `id`'s chat: live subagents,
/// the goal (when its editor is closed) or queued follow-ups. The chat folds
/// a tail row into its transcript while this holds.
pub fn has_tail(s: &State, id: &str) -> bool {
    let Some(c) = crate::chat::chat_ref(s, id) else { return false };
    let o = &c.orchestration;
    o.tasks.iter().any(|t| t.status.is_live())
        || (!c.goals.editing() && c.goals.goal.is_some())
        || !o.queue.items.is_empty()
}

/// The running turn, streamed (IronClaw's loop) or polled (CLI runtime).
pub fn running_run(s: &State, id: &str) -> Option<String> {
    crate::chat::active_run(s, id)
        .or_else(|| crate::chat::chat_ref(s, id)?.orchestration.queue.external_run().map(str::to_string))
}

/// Start the slow poll if something is moving. It stops by itself.
pub fn ensure_poll(s: &mut State, id: &str) {
    let moving = needs_poll(s, id);
    let Some(o) = slot(s, id) else { return };
    if o.polling || !moving {
        return;
    }
    o.polling = true;
    schedule_tick(s, id.to_string());
}

fn schedule_tick(s: &State, thread_id: String) {
    jobs::after(s, POLL, move |s, _| {
        if slot(s, &thread_id).is_none() {
            return;
        }
        if !needs_poll(s, &thread_id) {
            if let Some(o) = slot(s, &thread_id) {
                o.polling = false;
            }
            return;
        }
        // In the background nothing is watching; the next stream event or a
        // later tick catches up.
        if crate::workspace::window_active(s) {
            refresh(s, &thread_id);
        }
        schedule_tick(s, thread_id);
    });
}

/// Queue `text` as a follow-up for after the current turn (the composer's
/// ⌥Enter). `false` while another queue change is in flight. When the
/// server refuses it, the text goes back into an empty composer so it is
/// never lost.
pub fn queue_follow_up(s: &mut State, id: &str, text: String) -> bool {
    let Some(o) = slot(s, id) else { return false };
    let thread_id = id.to_string();
    if o.busy {
        return false;
    }
    o.busy = true;
    o.note = None;
    let asked = thread_id.clone();
    let action = QueueAction::Add(text.clone());
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.mutate_queue(&thread_id, &action).await },
        move |s, result, _| {
            let Some(o) = slot(s, &asked) else { return };
            o.busy = false;
            match result {
                Ok(queue) => o.queue = queue,
                Err(e) => {
                    if crate::chat::composer_text(s, &asked).trim().is_empty() {
                        crate::chat::set_composer_text(s, &asked, text);
                    }
                    crate::chat::report_error(s, &asked, e.to_string());
                }
            }
            ensure_poll(s, &asked);
        },
    );
    true
}

pub(crate) fn queue_action(s: &mut State, id: &str, action: QueueAction) {
    let Some(o) = slot(s, id) else { return };
    let thread_id = id.to_string();
    if o.busy {
        return;
    }
    o.busy = true;
    o.note = None;
    let asked = thread_id.clone();
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.mutate_queue(&thread_id, &action).await },
        move |s, result, _| {
            let Some(o) = slot(s, &asked) else { return };
            o.busy = false;
            match result {
                Ok(queue) => o.queue = queue,
                Err(e) => crate::chat::report_error(s, &asked, e.to_string()),
            }
            ensure_poll(s, &asked);
        },
    );
}

pub fn cancel_task(s: &mut State, id: &str, task_id: String) {
    let thread_id = id.to_string();
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.cancel_task(&task_id).await },
        move |s, result, _| match result {
            Ok(view) => {
                if let Some(o) = slot(s, &thread_id)
                    && let Some(task) = o.tasks.iter_mut().find(|t| t.task_id == view.task_id)
                {
                    *task = view;
                }
            }
            Err(e) => crate::chat::report_error(s, &thread_id, e.to_string()),
        },
    );
}

/// Fork the thread into a new one. `at_run`: keep only the history up to
/// and including run `at_run` (1-based); `None` keeps everything.
pub fn fork(s: &mut State, id: &str, at_run: Option<u32>) {
    let Some(o) = slot(s, id) else { return };
    let thread_id = id.to_string();
    if o.busy {
        return;
    }
    o.busy = true;
    let asked = thread_id.clone();
    jobs::run(
        s,
        "orchestration",
        move |c| async move {
            let options = ForkOptions { run: at_run, ..ForkOptions::default() };
            c.fork_thread(&thread_id, &options).await
        },
        move |s, result, _| {
            // The fork opens as the main chat, from a side chat too (GPUI
            // `OpenThread` bubbles up to the window).
            if let Some(o) = slot(s, &asked) {
                o.busy = false;
            }
            match result {
                Ok(forked) => crate::workspace::open_thread(s, forked.fork_thread_id),
                Err(e) => crate::chat::report_error(s, &asked, e.to_string()),
            }
        },
    );
}

pub(crate) fn merge_back(s: &mut State, id: &str) {
    let Some(o) = slot(s, id) else { return };
    let thread_id = id.to_string();
    if o.busy {
        return;
    }
    o.busy = true;
    o.note = None;
    let asked = thread_id.clone();
    jobs::run(
        s,
        "orchestration",
        move |c| async move { c.merge_back(&thread_id, None).await },
        move |s, result, _| {
            let target = result
                .as_ref()
                .ok()
                .and_then(|m| m.target_thread_id.as_deref())
                .and_then(|id| crate::workspace::thread_title(s, id))
                .unwrap_or_else(|| "the original thread".into());
            let Some(o) = slot(s, &asked) else { return };
            o.busy = false;
            match result {
                Ok(merged) => {
                    o.note = Some(merged.note.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
                        format!("Merged into {target}. It arrives there as context once that thread is idle.")
                    }));
                    o.lineage_loaded = false;
                    refresh(s, &asked);
                }
                Err(e) => crate::chat::report_error(s, &asked, e.to_string()),
            }
        },
    );
}

pub(crate) fn toggle_queue(s: &mut State, id: &str) {
    let Some(o) = slot(s, id) else { return };
    o.queue_open = !o.queue_open;
    if o.queue_open && o.queue_input.is_none() {
        o.queue_input = Some(TextEdit::new(""));
    }
}

pub(crate) fn add_queued(s: &mut State, id: &str) {
    let Some(o) = slot(s, id) else { return };
    let Some(input) = o.queue_input.as_mut() else { return };
    let text = input.text.trim().to_string();
    if text.is_empty() || o.busy {
        return;
    }
    input.clear();
    queue_action(s, id, QueueAction::Add(text));
}

/// A short line for the jump-to-end button: "2 queued", "1 working".
pub fn tail_summary(s: &State, id: &str) -> Option<String> {
    let o = &crate::chat::chat_ref(s, id)?.orchestration;
    let queued = o.queue.items.len();
    let working = o.tasks.iter().filter(|t| t.status.is_live()).count();
    let mut parts = Vec::new();
    if working > 0 {
        parts.push(format!("{working} working"));
    }
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// "in 12 min", "in 2 h 5 min", "any moment now".
pub(crate) fn countdown(at_ms: i64, now_ms: i64) -> String {
    let secs = (at_ms - now_ms) / 1000;
    if secs <= 0 {
        return "any moment now".into();
    }
    format!("in {}", duration(secs))
}

/// "45 s", "3 min", "2 h 5 min".
pub(crate) fn duration(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// A forked thread with a live subagent, a held queue and two follow-ups,
/// for the headless scene.
pub fn fixture() -> OrchestrationState {
    let now = chrono::Utc::now().timestamp_millis();
    let task: Option<TaskView> = fixture_value(serde_json::json!({
        "taskId": "task-1",
        "title": "Port the side pane's git tab",
        "status": "running",
        "runtime": "codex",
        "parentThreadId": "t1",
        "childThreadId": "t9",
        "createdAt": now - 200_000,
        "startedAt": now - 185_000,
    }))
    ;
    let waiting: Option<TaskView> = fixture_value(serde_json::json!({
        "taskId": "task-2",
        "title": "Write the parity checklist",
        "status": "waiting",
        "runtime": "claude-cli",
        "parentThreadId": "t1",
        "createdAt": now - 900_000,
        "limitResetAt": now + 725_000,
    }))
    ;
    let queue: QueueDoc = fixture_value(serde_json::json!({
        "held": true,
        "holdReason": "stopped",
        "items": [
            {"id": "q1", "text": "Then run the headless scenes and compare the PNGs."},
            {"id": "q2", "text": "Commit on the area branch when it builds clean."},
        ],
        "runtime": "codex",
    }))
    .unwrap_or_default();
    let lineage: Lineage = fixture_value(serde_json::json!({
        "forkedFrom": {
            "id": "x1",
            "kind": "fork",
            "sourceThread": "t2",
            "status": "done",
            "createdAt": now - 3_600_000,
        },
    }))
    .unwrap_or_default();
    OrchestrationState {
        tasks: [task, waiting].into_iter().flatten().collect(),
        queue,
        lineage,
        lineage_loaded: true,
        queue_open: true,
        queue_input: Some(TextEdit::new("")),
        ..Default::default()
    }
}

fn fixture_value<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Option<T> {
    serde_json::from_value(v)
        .map_err(|e| tracing::warn!(error = %e, "orchestration fixture does not parse"))
        .ok()
}

/// Scene `orchestration`: a fork with live subagents, a held queue and its
/// panel open, plus the goal.
pub fn scene(s: &mut State) {
    crate::scenes::chat(s);
    s.workspace.titles.remember("t9", "Port the side pane");
    if let Some(c) = s.chat.as_mut() {
        c.orchestration = fixture();
        c.goals = crate::chat::goals::fixture();
    }
    crate::chat::fixtures::refold(s);
}

#[cfg(test)]
mod tests {
    use super::{Snapshot, apply_snapshot, countdown, duration, fixture, queue_follow_up};
    use crate::chat::chat_ref;
    use crate::chat::fixtures::{close_side_chat, test_state, with_side_chat};

    fn snapshot() -> Snapshot {
        Snapshot { tasks: Ok(Vec::new()), queue: Ok(fixture().queue), lineage: None }
    }

    fn queued(s: &crate::state::State, id: &str) -> usize {
        chat_ref(s, id).map_or(0, |c| c.orchestration.queue.items.len())
    }

    #[test]
    fn a_refresh_lands_in_its_own_chat_and_is_dropped_once_that_chat_closed() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        apply_snapshot(&mut s, "sc2", snapshot());
        assert_eq!(queued(&s, "sc2"), 2);
        assert_eq!(queued(&s, "t1"), 0, "the open chat's queue is its own");
        close_side_chat(&mut s);
        apply_snapshot(&mut s, "sc2", snapshot());
        assert_eq!(queued(&s, "t1"), 0);
    }

    #[test]
    fn a_side_chat_queues_into_its_own_thread() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        assert!(queue_follow_up(&mut s, "sc2", "after this".into()));
        assert!(chat_ref(&s, "sc2").is_some_and(|c| c.orchestration.busy));
        assert!(chat_ref(&s, "t1").is_some_and(|c| !c.orchestration.busy));
        // A closed chat has nothing to queue into.
        close_side_chat(&mut s);
        assert!(!queue_follow_up(&mut s, "sc2", "after this".into()));
    }

    #[test]
    fn labels_read_naturally() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(185), "3 min");
        assert_eq!(duration(7_500), "2 h 5 min");
        assert_eq!(countdown(1_000, 5_000), "any moment now");
        assert_eq!(countdown(725_000, 5_000), "in 12 min");
    }
}
