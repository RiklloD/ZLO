//! "Import terminal sessions": Claude Code and Codex sessions run in a
//! terminal become threads here (in the inbox's Settled shelf), searchable
//! and resumable. One switch per source, the periodic rescan, and "Import
//! now" with the last run's counts. The backend runs the import as a job;
//! while it runs this panel polls the status once a second.
//!
//! [`schedule`] asks for the first-launch import when the app connects and a
//! rescan every [`RESCAN_EVERY`] after; the backend runs either only when
//! due, and reads only session files that changed. Port of GPUI
//! `pages/settings/session_import.rs`.

use std::time::Duration;

use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::session_import::{
    ImportTrigger, SessionImportCounts, SessionImportRequest, SessionImportStatus,
};
use rune::{El, col};
use rune_kit::Kit;

use crate::jobs::{self, Generation};
use crate::pages::common::{Btn, button, error_line, rows_card, setting_row, switch};
use crate::state::State;

/// How often the app asks for a background rescan.
pub const RESCAN_EVERY: Duration = Duration::from_secs(15 * 60);
const RUNNING_POLL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct SessionImportState {
    pub status: Option<SessionImportStatus>,
    pub saving: bool,
    pub error: Option<String>,
    /// The running poll; a newer one replaces it.
    poll: Generation,
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "session import",
        |c| async move { c.session_import_status().await },
        show,
    );
}

fn show(s: &mut State, result: Result<SessionImportStatus, ClientError>, _: &mut rune::Tx) {
    let this = &mut s.pages.settings.session_import;
    this.saving = false;
    match result {
        Ok(status) => {
            let running = status.running;
            this.status = Some(status);
            this.error = None;
            if running {
                poll_while_running(s);
            }
        }
        Err(e) if e.is_not_found() => {
            this.error = Some("This backend has no session import yet.".into());
        }
        Err(e) => this.error = Some(e.to_string()),
    }
}

fn poll_while_running(s: &mut State) {
    let generation = s.pages.settings.session_import.poll.bump();
    jobs::after(s, RUNNING_POLL, move |s, _| {
        if s.pages.settings.session_import.poll == generation {
            refresh(s);
        }
    });
}

fn send(s: &mut State, request: SessionImportRequest) {
    let this = &mut s.pages.settings.session_import;
    if this.saving {
        return;
    }
    this.saving = true;
    jobs::run(
        s,
        "session import",
        move |c| async move { c.session_import(&request).await },
        show,
    );
}

fn set_source(s: &mut State, source: String, on: bool) {
    send(
        s,
        SessionImportRequest {
            sources: [(source, on)].into(),
            ..SessionImportRequest::default()
        },
    );
}

/// "3 imported · 1 updated · 40 unchanged · 2 skipped" (zeros left out).
pub fn counts_line(c: &SessionImportCounts) -> String {
    let parts: Vec<String> = [
        (c.found, "found"),
        (c.imported, "imported"),
        (c.updated, "updated"),
        (c.unchanged, "unchanged"),
        (c.skipped, "skipped"),
        (c.failed, "unreadable"),
        (c.deferred, "left for the next run"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, label)| format!("{n} {label}"))
    .collect();
    if parts.is_empty() {
        "No sessions found".into()
    } else {
        parts.join(" · ")
    }
}

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let this = &s.pages.settings.session_import;
    let mut d = col().w_full();
    if let Some(e) = &this.error {
        d = d.child(error_line(p, e));
    }
    let Some(status) = &this.status else {
        return d;
    };
    let busy = this.saving || status.running;
    let mut rows: Vec<El> = status
        .sources
        .iter()
        .map(|source| {
            let (id, on) = (source.source.clone(), source.enabled);
            let toggle = switch(
                k,
                source.enabled,
                busy || !source.available,
                &format!("Import {} sessions", source.label),
                move |s: &mut State| set_source(s, id.clone(), !on),
            );
            let note = if !source.available {
                format!("{} is not installed on this Mac.", source.label)
            } else {
                match &source.counts {
                    Some(c) => format!("Last run: {}", counts_line(c)),
                    None => "Not imported yet.".into(),
                }
            };
            setting_row(p, &source.label, Some(&note), toggle)
                .key(format!("import-{}", source.source))
        })
        .collect();
    let background_on = status.background;
    let background = switch(
        k,
        status.background,
        busy,
        "Rescan for new terminal sessions",
        move |s: &mut State| {
            send(
                s,
                SessionImportRequest {
                    background: Some(!background_on),
                    ..SessionImportRequest::default()
                },
            )
        },
    );
    rows.push(setting_row(
        p,
        "Keep up to date",
        Some("Every 15 minutes while ZLO runs, look for new sessions and new messages. Files that did not change are not read."),
        background,
    ));
    let last = match (status.running, status.last_run_at) {
        (true, _) => "Importing…".to_string(),
        (false, Some(at)) => chrono::DateTime::from_timestamp_millis(at)
            .map(|t| format!("Last run {}", jobs::ago(t)))
            .unwrap_or_default(),
        (false, None) => "Never run".into(),
    };
    let now = button(
        if status.running {
            "Importing…"
        } else {
            "Import now"
        },
        Btn::Secondary,
    )
    .disabled(busy)
    .on_press(|s: &mut State| {
        send(
            s,
            SessionImportRequest {
                run: Some(ImportTrigger::Manual),
                ..SessionImportRequest::default()
            },
        )
    })
    .build(k);
    let note = match &status.last_error {
        Some(error) => format!("{last}. {error}"),
        None => {
            format!("{last}. Imported threads land in Settled and resume in the CLI's own session.")
        }
    };
    rows.push(setting_row(p, "Import terminal sessions", Some(&note), now));
    d.child(rows_card(p, rows))
}

/// Ask for the first-launch import now, then a rescan every
/// [`RESCAN_EVERY`]. Fire and forget: failures only reach the debug log.
/// The workspace calls this once the gateway connected.
pub fn schedule(s: &State) {
    let Some(backend) = s.rt.backend.clone() else {
        return;
    };
    let ticker = backend.clone();
    backend.spawn(move |_| async move {
        let mut trigger = ImportTrigger::Launch;
        loop {
            let job = ticker.spawn(move |c| async move { c.session_import_tick(trigger).await });
            if let Err(e) = jobs::flatten(job.await) {
                tracing::debug!(error = %e, "session import tick failed");
            }
            tokio::time::sleep(RESCAN_EVERY).await;
            trigger = ImportTrigger::Background;
        }
    });
}

pub fn fixture(s: &mut State) {
    let status: SessionImportStatus = match serde_json::from_value(serde_json::json!({
        "sources": [
            {"source": "claude_code", "label": "Claude Code", "enabled": true, "available": true,
             "counts": {"found": 42, "imported": 3, "updated": 1, "unchanged": 38}},
            {"source": "codex", "label": "Codex", "enabled": false, "available": false}
        ],
        "background": true,
        "running": false,
        "last_run_at": chrono::Utc::now().timestamp_millis() - 12 * 60 * 1000
    })) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "session import fixture");
            return;
        }
    };
    s.pages.settings.session_import.status = Some(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_as_one_line_without_zeros() {
        let c = SessionImportCounts {
            found: 5,
            imported: 3,
            skipped: 2,
            ..SessionImportCounts::default()
        };
        assert_eq!(counts_line(&c), "5 found · 3 imported · 2 skipped");
        assert_eq!(
            counts_line(&SessionImportCounts::default()),
            "No sessions found"
        );
    }
}
