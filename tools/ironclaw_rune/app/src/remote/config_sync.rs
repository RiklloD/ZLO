//! Configuration sync: This Mac's settings, profiles, prompts and
//! approvals carried to every other connected computer, so a thread there
//! runs with what the person set up here.
//!
//! The app is the carrier (`zlo_client::config_sync::sync_once`): it reads
//! This Mac's backend and writes the other one over connections it already
//! holds. This Mac is always the source; a computer is a target while it
//! is connected and its switch is on (on by default, `Prefs::
//! config_sync_off` lists the ones turned off).
//!
//! No timer (README rule 4). A target is checked when it connects, when
//! the window comes to the front, when the person leaves a page (a setting
//! may have changed), and on "Sync now". A check is two small reads; data
//! moves only for what differs. Plan:
//! `docs/internal/plans/2026-11-26-zlo-config-sync.md`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use zlo_client::config_sync::{Side, SyncMemo, SyncOutcome, SyncReport, sync_once};
use zlo_client::environments::THIS_MAC;
use zlo_client::{Backend, ClientError};

use crate::state::{Conn, Page, State};

/// The window coming back checks again after this long.
const ACTIVATION_GAP: Duration = Duration::from_secs(60);
/// Leaving a page checks again after this long.
const PAGE_GAP: Duration = Duration::from_secs(10);

/// What the last pass to one computer came to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Not checked on this connection yet.
    #[default]
    Pending,
    /// Everything already matched.
    Matches,
    Synced(SyncReport),
    /// That side's IronClaw is older than configuration sync.
    NeedsUpdate(Side),
    Failed(String),
}

#[derive(Default)]
struct Row {
    phase: Phase,
    /// The pass in flight; a reply from any other pass is dropped.
    running: Option<u64>,
    checked_at: Option<Instant>,
    /// Checked again on the next [`sync`].
    due: bool,
    /// Domains that differ by design and need no carrying
    /// (`zlo_client::config_sync::SyncMemo`).
    memo: SyncMemo,
    /// Config sections waiting for that computer's IronClaw to restart.
    /// Kept across later passes; forgotten when its connection goes.
    restart: Vec<String>,
}

/// `State::remote.config_sync`: one row per connected target.
#[derive(Default)]
pub struct ConfigSync {
    rows: HashMap<String, Row>,
    next_run: u64,
    last_page: Option<Page>,
}

impl ConfigSync {
    /// A row as a scene shows it.
    pub fn set_fixture(&mut self, key: &str, phase: Phase, restart: &[&str]) {
        self.rows.insert(
            key.to_string(),
            Row {
                phase,
                restart: restart.iter().map(|s| (*s).to_string()).collect(),
                ..Row::default()
            },
        );
    }
}

/// Whether `key` receives This Mac's configuration.
pub fn enabled(s: &State, key: &str) -> bool {
    !s.prefs.config_sync_off.iter().any(|k| k == key)
}

pub fn set_enabled(s: &mut State, key: &str, on: bool) {
    let key = key.to_string();
    let k = key.clone();
    s.update_prefs(move |p| {
        p.config_sync_off.retain(|x| x != &k);
        if !on {
            p.config_sync_off.push(k);
        }
    });
    if on {
        sync_now(s, &key);
    } else {
        s.remote.config_sync.rows.remove(&key);
    }
}

/// The person's "Sync now".
pub fn sync_now(s: &mut State, key: &str) {
    let row = s
        .remote
        .config_sync
        .rows
        .entry(key.to_string())
        .or_default();
    row.due = true;
    // Asked for by hand: everything is compared again.
    row.memo.clear();
    sync(s);
}

/// The window came to the front.
pub fn window_active(s: &mut State) {
    mark_due(s, ACTIVATION_GAP);
}

fn mark_due(s: &mut State, gap: Duration) {
    for row in s.remote.config_sync.rows.values_mut() {
        if row.checked_at.is_none_or(|at| at.elapsed() >= gap) {
            row.due = true;
        }
    }
}

/// Every ready connection: the focused computer, then the peers.
fn connections(s: &State) -> Vec<(String, Backend)> {
    let mut out = Vec::new();
    if matches!(s.conn, Conn::Ready)
        && let Some(backend) = s.rt.backend.as_ref()
    {
        out.push((s.onboarding.computer.key(), backend.clone()));
    }
    for (key, _, backend) in super::peers::stream_targets(s) {
        out.push((key, backend));
    }
    out
}

/// Start the passes that are due and forget computers that went away.
/// Cheap when nothing is due; called from the app's poll.
pub fn sync(s: &mut State) {
    if s.fixture {
        return;
    }
    if s.remote.config_sync.last_page != Some(s.page) {
        s.remote.config_sync.last_page = Some(s.page);
        mark_due(s, PAGE_GAP);
    }
    let mut connections = connections(s);
    let source = connections
        .iter()
        .position(|(key, _)| key == THIS_MAC)
        .map(|at| connections.swap_remove(at).1);
    let connected: HashSet<&str> = connections.iter().map(|(key, _)| key.as_str()).collect();
    // A computer whose connection went is checked afresh when it is back,
    // and by then it has restarted or not: the note goes with it.
    s.remote
        .config_sync
        .rows
        .retain(|key, _| connected.contains(key.as_str()));
    let Some(source) = source else {
        return;
    };
    for (key, target) in &connections {
        if !enabled(s, key) {
            continue;
        }
        let state = &mut s.remote.config_sync;
        let fresh = !state.rows.contains_key(key);
        let row = state.rows.entry(key.clone()).or_default();
        if row.running.is_some() || !(fresh || row.due) {
            continue;
        }
        state.next_run += 1;
        let run = state.next_run;
        row.running = Some(run);
        row.due = false;
        let memo = row.memo.clone();
        let delivery = s.rt.delivery();
        let from = source.client().clone();
        let key = key.clone();
        // Only clients travel: no backend is dropped off the UI thread.
        target.spawn(move |to| async move {
            let result = sync_once(&from, &to, &BTreeSet::new(), memo).await;
            delivery.deliver(move |s, _| finished(s, &key, run, result));
        });
    }
}

fn finished(
    s: &mut State,
    key: &str,
    run: u64,
    result: Result<(SyncOutcome, SyncMemo), ClientError>,
) -> bool {
    crate::energy::hit(crate::energy::C::UpdateJob);
    if s.fixture {
        return false;
    }
    let Some(row) = s.remote.config_sync.rows.get_mut(key) else {
        return false;
    };
    if row.running != Some(run) {
        return false;
    }
    row.running = None;
    row.checked_at = Some(Instant::now());
    let result = result.map(|(outcome, memo)| {
        row.memo = memo;
        outcome
    });
    row.phase = match result {
        Ok(SyncOutcome::UpToDate) => Phase::Matches,
        Ok(SyncOutcome::Applied(report)) => {
            for section in &report.restart_required {
                if !row.restart.contains(section) {
                    row.restart.push(section.clone());
                }
            }
            Phase::Synced(report)
        }
        Ok(SyncOutcome::Unsupported(side)) => Phase::NeedsUpdate(side),
        Err(e) => {
            tracing::debug!(computer = %key, error = %e, "configuration sync failed");
            Phase::Failed(super::first_line(&e.to_string()))
        }
    };
    true
}

/// How the Computers page words one computer's line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Quiet,
    Good,
    Warn,
    Bad,
}

/// One computer's line on the Computers page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub tone: Tone,
    pub on: bool,
    /// "Sync now" is offered.
    pub can_sync: bool,
}

/// The line under a saved computer, or `None` when there is nothing to
/// say (a Light install has no settings of its own to send; a computer
/// that is not connected and has sync on).
pub fn line(s: &State, key: &str, label: &str) -> Option<Line> {
    if s.remote.app_only || key == THIS_MAC {
        return None;
    }
    let on = enabled(s, key);
    if !on {
        return Some(Line {
            text: "Settings sync from This Mac is off".into(),
            tone: Tone::Quiet,
            on,
            can_sync: false,
        });
    }
    if !matches!(super::state_of(s, key), super::EnvState::Connected) {
        return None;
    }
    let source_ready =
        s.fixture || matches!(super::state_of(s, THIS_MAC), super::EnvState::Connected);
    if !source_ready {
        return Some(Line {
            text: "Settings sync needs IronClaw running on this Mac".into(),
            tone: Tone::Quiet,
            on,
            can_sync: false,
        });
    }
    let row = s.remote.config_sync.rows.get(key);
    let running = row.is_some_and(|r| r.running.is_some());
    let restart = row.map(|r| r.restart.as_slice()).unwrap_or_default();
    let (text, tone) = if running {
        ("Syncing settings from This Mac…".to_string(), Tone::Quiet)
    } else {
        match row.map(|r| &r.phase) {
            None | Some(Phase::Pending) => {
                ("Settings sync from This Mac is on".into(), Tone::Quiet)
            }
            Some(Phase::NeedsUpdate(Side::Target)) => (
                format!("Update IronClaw on {label} to sync settings"),
                Tone::Warn,
            ),
            Some(Phase::NeedsUpdate(Side::Source)) => (
                "Update IronClaw on this Mac to sync settings".into(),
                Tone::Warn,
            ),
            Some(Phase::Failed(why)) => (format!("Settings sync failed: {why}"), Tone::Bad),
            Some(Phase::Synced(report)) if !report.skipped.is_empty() => {
                let (domain, why) = &report.skipped[0];
                (
                    format!("Settings synced, except {}: {why}", domain_name(*domain)),
                    Tone::Warn,
                )
            }
            Some(Phase::Matches | Phase::Synced(_)) if !restart.is_empty() => (
                format!(
                    "Settings synced. Restart IronClaw on {label} to finish ({})",
                    restart.join(", ")
                ),
                Tone::Warn,
            ),
            Some(Phase::Matches | Phase::Synced(_)) => {
                ("Settings match This Mac".into(), Tone::Good)
            }
        }
    };
    Some(Line {
        text,
        tone,
        on,
        can_sync: !running,
    })
}

fn domain_name(domain: zlo_client::config_sync::ConfigSyncDomain) -> String {
    serde_json::to_value(domain)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_else(|| "one part".into())
}

#[cfg(test)]
#[path = "config_sync_tests.rs"]
mod tests;
