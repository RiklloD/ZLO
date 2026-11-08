//! When each thread's current run started, for the "Weaving… 2m 33s" line
//! and the sidebar's Working timer. Lives for the app run, outside any chat
//! state, so leaving a thread and coming back keeps counting. (GPUI
//! `run_clock.rs`; the global lives in `WorkspaceState::run_clock`.)
//!
//! The backend's turn start (inbox `working_since`) wins when it has one;
//! otherwise the first chat that saw the run remembers it, keyed by run id
//! so a stale start never leaks into the next run.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use zlo_client::inbox::InboxRow;

use crate::state::State;

#[derive(Default)]
pub struct RunClock {
    /// Thread -> turn start, from the last inbox read.
    backend: HashMap<String, Instant>,
    /// Thread -> (run id, start) seen by a chat.
    seen: HashMap<String, (Option<String>, Instant)>,
}

fn instant_at(unix_ms: i64) -> Instant {
    let ago = (Utc::now().timestamp_millis() - unix_ms).max(0) as u64;
    Instant::now()
        .checked_sub(Duration::from_millis(ago))
        .unwrap_or_else(Instant::now)
}

/// Record the turn starts the inbox just returned.
pub fn set_rows(s: &mut State, rows: &[InboxRow]) {
    s.workspace.run_clock.backend = rows
        .iter()
        .filter_map(|r| Some((r.thread_id.clone(), instant_at(r.working_since?))))
        .collect();
}

/// The start of `thread_id`'s live run `run`. `sent_ms` is when the
/// prompt went out, used when this app run never saw the start itself.
pub fn start(s: &mut State, thread_id: &str, run: Option<&str>, sent_ms: Option<i64>) -> Instant {
    let clock = &mut s.workspace.run_clock;
    if let Some(at) = clock.backend.get(thread_id) {
        return *at;
    }
    match clock.seen.get_mut(thread_id) {
        Some((seen_run, at)) if seen_run.as_deref() == run => *at,
        // The run id arrived after the send: same run.
        Some((seen_run, at)) if seen_run.is_none() => {
            *seen_run = run.map(str::to_string);
            *at
        }
        known => {
            // A new run while this app watched the thread starts now; one
            // found already running (after a relaunch) started at its prompt.
            let at = match (known, sent_ms) {
                (None, Some(ms)) => instant_at(ms),
                _ => Instant::now(),
            };
            clock
                .seen
                .insert(thread_id.to_string(), (run.map(str::to_string), at));
            at
        }
    }
}

/// Forget a run that ended.
pub fn end(s: &mut State, thread_id: &str) {
    let clock = &mut s.workspace.run_clock;
    clock.seen.remove(thread_id);
    clock.backend.remove(thread_id);
}

/// Seconds `thread_id` has been working, when known.
pub fn elapsed(s: &State, thread_id: &str) -> Option<u64> {
    let clock = &s.workspace.run_clock;
    clock
        .backend
        .get(thread_id)
        .or_else(|| clock.seen.get(thread_id).map(|(_, at)| at))
        .map(|at| at.elapsed().as_secs())
}
