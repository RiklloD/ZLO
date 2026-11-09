//! Operator → Inspector: pick a conversation and one of its runs, then see
//! model calls, tool executions, activity, token stats, the exact prompt,
//! and the thread's log lines. "Follow live" re-reads the snapshot on every
//! update. GPUI `pages/operator/inspector.rs`; rendering in
//! `inspector/view.rs`.
//!
//! The gateway keeps diagnostics in memory for a few recent runs per
//! session, so older runs show "no diagnostics".

mod view;

use chrono::{DateTime, Utc};
use zlo_client::operator::inspector::ToolExecutionDiagnostic;
use zlo_client::operator::{InspectorPrompt, InspectorRun, InspectorSignal, InspectorStream};
use zlo_client::product_wire::{RebornLogEntry, RebornOperatorLogsQuery};
use zlo_client::wire::ThreadSummary;

use crate::jobs::{self, Generation};
use crate::state::State;

pub use view::view;

const THREADS: u32 = 30;
const TIMELINE: u32 = 200;
const THREAD_LOGS: u32 = 50;

pub struct RunRef {
    pub id: String,
    pub at: Option<DateTime<Utc>>,
}

/// One tool's detail, read on demand (`inspector_tool`).
pub enum ToolDetail {
    Loading,
    Loaded(Box<ToolExecutionDiagnostic>),
    Missing,
    Failed(String),
}

#[derive(Default)]
pub struct InspectorPanel {
    pub threads: Vec<ThreadSummary>,
    pub thread: Option<String>,
    pub runs: Vec<RunRef>,
    pub run: Option<String>,
    /// `Some(None)`: the gateway kept no diagnostics for this run.
    pub snapshot: Option<Option<InspectorRun>>,
    pub prompt: Option<Result<Option<InspectorPrompt>, String>>,
    pub tool: Option<(String, ToolDetail)>,
    pub logs: Option<Vec<RebornLogEntry>>,
    pub fetching: bool,
    pub dirty: bool,
    /// The live update stream; dropping it frees the gateway's slot.
    pub live: Option<InspectorStream>,
    /// Bumped per stream so a stopped stream's last signals are dropped.
    pub live_gen: Generation,
    pub live_note: Option<String>,
    pub error: Option<String>,
}

fn st(s: &mut State) -> Option<&mut InspectorPanel> {
    s.pages.operator.inspector.as_mut()
}

fn fail(s: &mut State, e: impl ToString) {
    if let Some(p) = st(s) {
        p.error = Some(e.to_string());
    }
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "inspector",
        |c| async move { c.list_threads(THREADS, None).await },
        |s, result, _| match result {
            Ok(page) => {
                if let Some(p) = st(s) {
                    p.threads = page.threads;
                }
            }
            Err(e) => fail(s, e),
        },
    );
    if let Some(thread) = st(s).and_then(|p| p.thread.clone()) {
        load_runs(s, thread);
    }
    fetch_snapshot(s);
}

fn select_thread(s: &mut State, thread: String) {
    let Some(p) = st(s) else { return };
    stop_live(p);
    p.thread = Some(thread.clone());
    p.run = None;
    p.runs.clear();
    clear_run(p);
    p.logs = None;
    load_runs(s, thread);
}

/// Runs come from the thread timeline; newest first.
fn load_runs(s: &mut State, thread: String) {
    let logs_thread = thread.clone();
    let want = thread.clone();
    jobs::run(
        s,
        "inspector",
        move |c| async move { c.timeline(&thread, TIMELINE).await },
        move |s, result, _| match result {
            Ok(page) => {
                let Some(p) = st(s).filter(|p| p.thread.as_deref() == Some(want.as_str())) else {
                    return;
                };
                p.runs = runs_of(
                    page.messages
                        .iter()
                        .map(|m| (m.turn_run_id.clone(), m.created_at)),
                );
                if p.run.is_none()
                    && let Some(first) = p.runs.first().map(|r| r.id.clone())
                {
                    select_run(s, first);
                }
            }
            Err(e) => fail(s, e),
        },
    );
    let query = RebornOperatorLogsQuery {
        limit: Some(THREAD_LOGS),
        thread_id: Some(logs_thread.clone()),
        ..Default::default()
    };
    jobs::run(
        s,
        "inspector",
        move |c| async move { c.thread_logs(&query).await },
        move |s, result, _| match result {
            Ok(page) => {
                if let Some(p) = st(s).filter(|p| p.thread.as_deref() == Some(logs_thread.as_str()))
                {
                    p.logs = Some(page.entries);
                }
            }
            Err(e) => fail(s, e),
        },
    );
}

fn clear_run(p: &mut InspectorPanel) {
    p.snapshot = None;
    p.prompt = None;
    p.tool = None;
}

fn select_run(s: &mut State, run: String) {
    let Some(p) = st(s) else { return };
    stop_live(p);
    p.run = Some(run);
    clear_run(p);
    fetch_snapshot(s);
}

/// One snapshot read at a time; updates during a read mark it dirty.
fn fetch_snapshot(s: &mut State) {
    let Some(p) = st(s) else { return };
    let (Some(thread), Some(run)) = (p.thread.clone(), p.run.clone()) else {
        return;
    };
    if p.fetching {
        p.dirty = true;
        return;
    }
    p.fetching = true;
    let (want_thread, want_run) = (thread.clone(), run.clone());
    jobs::run(
        s,
        "inspector",
        move |c| async move { c.inspector_snapshot(&thread, &run).await },
        move |s, result, _| {
            let Some(p) = st(s) else { return };
            p.fetching = false;
            // Drop a late answer for a run that is no longer selected.
            if p.thread.as_deref() == Some(want_thread.as_str())
                && p.run.as_deref() == Some(want_run.as_str())
            {
                match result {
                    Ok(snapshot) => {
                        p.snapshot = Some(snapshot);
                        p.error = None;
                    }
                    Err(e) => p.error = Some(e.to_string()),
                }
            }
            if std::mem::take(&mut p.dirty) {
                fetch_snapshot(s);
            }
        },
    );
}

fn load_prompt(s: &mut State) {
    let Some(p) = st(s) else { return };
    let (Some(thread), Some(run)) = (p.thread.clone(), p.run.clone()) else {
        return;
    };
    let want_run = run.clone();
    jobs::run(
        s,
        "inspector",
        move |c| async move { c.inspector_prompt(&thread, &run).await },
        move |s, result, _| {
            if let Some(p) = st(s).filter(|p| p.run.as_deref() == Some(want_run.as_str())) {
                p.prompt = Some(result.map_err(|e| e.to_string()));
            }
        },
    );
}

fn toggle_tool(s: &mut State, activity_id: String) {
    let Some(p) = st(s) else { return };
    if p.tool.as_ref().is_some_and(|(id, _)| *id == activity_id) {
        p.tool = None;
        return;
    }
    let (Some(thread), Some(run)) = (p.thread.clone(), p.run.clone()) else {
        return;
    };
    p.tool = Some((activity_id.clone(), ToolDetail::Loading));
    let id = activity_id.clone();
    jobs::run(
        s,
        "inspector",
        move |c| async move { c.inspector_tool(&thread, &run, &id).await },
        move |s, result, _| {
            let detail = match result {
                Ok(Some(tool)) => ToolDetail::Loaded(Box::new(tool)),
                Ok(None) => ToolDetail::Missing,
                Err(e) => ToolDetail::Failed(e.to_string()),
            };
            if let Some((open, slot)) = st(s).and_then(|p| p.tool.as_mut())
                && *open == activity_id
            {
                *slot = detail;
            }
        },
    );
}

fn set_live(s: &mut State, on: bool) {
    let Some(p) = st(s) else { return };
    if !on {
        stop_live(p);
        return;
    }
    let (Some(thread), Some(run)) = (p.thread.clone(), p.run.clone()) else {
        return;
    };
    let Some(backend) = s.rt.backend.clone() else {
        fail(s, "not connected to the gateway");
        return;
    };
    let (stream, mut rx) = backend.inspector_updates(&thread, &run);
    let handle = s.rt.handle.clone();
    let Some(p) = st(s) else { return };
    let generation = p.live_gen.bump();
    // The pump ends when the stream is dropped (its sender goes away).
    drop(backend.spawn(move |_| async move {
        while let Some(first) = rx.recv().await {
            // A burst of updates becomes one snapshot read.
            let mut last = first;
            while let Ok(next) = rx.try_recv() {
                last = next;
            }
            handle.update(move |s, _| on_signal(s, generation, last));
        }
    }));
    p.live_note = Some("Connecting".into());
    p.live = Some(stream);
}

fn on_signal(s: &mut State, generation: Generation, signal: InspectorSignal) {
    let Some(p) = st(s).filter(|p| p.live.is_some() && p.live_gen == generation) else {
        return;
    };
    match signal {
        InspectorSignal::Live => p.live_note = Some("Following live".into()),
        InspectorSignal::Changed => fetch_snapshot(s),
        InspectorSignal::Closed(reason) => {
            p.live = None;
            p.live_note = (!reason.is_empty()).then(|| format!("Live updates stopped: {reason}"));
        }
    }
}

fn stop_live(p: &mut InspectorPanel) {
    // Dropping the stream frees the gateway's stream slot.
    p.live = None;
    p.live_note = None;
    p.live_gen.bump();
}

/// Distinct run ids in timeline order, newest first, each with its first time.
fn runs_of(messages: impl Iterator<Item = (Option<String>, Option<DateTime<Utc>>)>) -> Vec<RunRef> {
    let mut runs: Vec<RunRef> = Vec::new();
    for (id, at) in messages {
        let Some(id) = id else { continue };
        match runs.iter_mut().find(|r| r.id == id) {
            Some(run) => run.at = run.at.or(at),
            None => runs.push(RunRef { id, at }),
        }
    }
    runs.reverse();
    runs
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::runs_of;

    #[test]
    fn runs_are_distinct_and_newest_first() {
        let t = |s: &str| s.parse().ok();
        let runs = runs_of(
            [
                (Some("a".to_string()), t("2026-10-23T10:00:00Z")),
                (None, None),
                (Some("a".to_string()), t("2026-10-23T10:00:05Z")),
                (Some("b".to_string()), t("2026-10-23T11:00:00Z")),
            ]
            .into_iter(),
        );
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"]);
        assert_eq!(runs[1].at, t("2026-10-23T10:00:00Z"));
    }
}
