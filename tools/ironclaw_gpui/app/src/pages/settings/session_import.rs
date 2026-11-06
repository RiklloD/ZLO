//! "Import terminal sessions": Claude Code and Codex sessions run in a
//! terminal become threads here (in the inbox's Settled shelf), searchable
//! and resumable. One switch per source, the periodic rescan, and "Import
//! now" with the last run's counts. The backend runs the import as a job;
//! while it runs this panel polls the status once a second.
//!
//! [`schedule`] asks for the first-launch import when the app connects and a
//! rescan every [`RESCAN_EVERY`] after; the backend runs either only when
//! due, and reads only session files that changed.

use std::time::Duration;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::session_import::{
    ImportTrigger, SessionImportCounts, SessionImportRequest, SessionImportStatus,
};

use crate::jobs;
use crate::pages::{error_line, rows_card, setting_row};
use crate::ui::*;

/// How often the app asks for a background rescan.
pub const RESCAN_EVERY: Duration = Duration::from_secs(15 * 60);
const RUNNING_POLL: Duration = Duration::from_secs(1);

pub struct SessionImportPanel {
    backend: Backend,
    status: Option<SessionImportStatus>,
    saving: bool,
    error: Option<SharedString>,
    _poll: Option<Task<()>>,
}

impl SessionImportPanel {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            status: None,
            saving: false,
            error: None,
            _poll: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.session_import_status().await },
            |this, result, cx| this.show(result, cx),
        );
    }

    fn show(
        &mut self,
        result: Result<SessionImportStatus, ironclaw_gpui_client::ClientError>,
        cx: &mut Context<Self>,
    ) {
        self.saving = false;
        match result {
            Ok(status) => {
                let running = status.running;
                self.status = Some(status);
                self.error = None;
                if running {
                    self.poll_while_running(cx);
                }
            }
            Err(e) if e.is_not_found() => {
                self.error = Some("This backend has no session import yet.".into());
            }
            Err(e) => self.error = Some(e.to_string().into()),
        }
        cx.notify();
    }

    fn poll_while_running(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RUNNING_POLL).await;
            let _ = this.update(cx, |this, cx| this.refresh(cx));
        }));
    }

    fn send(&mut self, request: SessionImportRequest, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.session_import(&request).await },
            |this, result, cx| this.show(result, cx),
        );
    }

    fn set_source(&mut self, source: String, on: bool, cx: &mut Context<Self>) {
        self.send(
            SessionImportRequest {
                sources: [(source, on)].into(),
                ..SessionImportRequest::default()
            },
            cx,
        );
    }
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

impl Render for SessionImportPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(status) = self.status.clone() else {
            return v_flex().when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)));
        };
        let busy = self.saving || status.running;
        let mut rows: Vec<AnyElement> = status
            .sources
            .iter()
            .enumerate()
            .map(|(ix, source)| {
                let id = source.source.clone();
                let switch = Switch::new(("import-source", ix))
                    .checked(source.enabled)
                    .disabled(busy || !source.available)
                    .accessibility_label(format!("Import {} sessions", source.label))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_source(id.clone(), *checked, cx)
                    }));
                let note = if !source.available {
                    format!("{} is not installed on this Mac.", source.label)
                } else {
                    match &source.counts {
                        Some(c) => format!("Last run: {}", counts_line(c)),
                        None => "Not imported yet.".into(),
                    }
                };
                setting_row(source.label.clone(), Some(note.into()), switch, cx)
            })
            .collect();
        let background = Switch::new("import-background")
            .checked(status.background)
            .disabled(busy)
            .accessibility_label("Rescan for new terminal sessions")
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                this.send(
                    SessionImportRequest {
                        background: Some(*checked),
                        ..SessionImportRequest::default()
                    },
                    cx,
                )
            }));
        rows.push(setting_row(
            "Keep up to date",
            Some(
                "Every 15 minutes while ZLO runs, look for new sessions and new messages. Files that did not change are not read."
                    .into(),
            ),
            background,
            cx,
        ));
        let last = match (status.running, status.last_run_at) {
            (true, _) => "Importing…".to_string(),
            (false, Some(at)) => chrono::DateTime::from_timestamp_millis(at)
                .map(|t| format!("Last run {}", jobs::ago(t)))
                .unwrap_or_default(),
            (false, None) => "Never run".into(),
        };
        let button = Button::new("import-now")
            .label(if status.running {
                "Importing…"
            } else {
                "Import now"
            })
            .small()
            .disabled(busy)
            .on_click(cx.listener(|this, _, _, cx| {
                this.send(
                    SessionImportRequest {
                        run: Some(ImportTrigger::Manual),
                        ..SessionImportRequest::default()
                    },
                    cx,
                )
            }));
        let note = match &status.last_error {
            Some(error) => format!("{last}. {error}"),
            None => format!(
                "{last}. Imported threads land in Settled and resume in the CLI's own session."
            ),
        };
        rows.push(setting_row(
            "Import terminal sessions",
            Some(note.into()),
            button,
            cx,
        ));
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .child(rows_card(rows, cx))
    }
}

/// Ask for the first-launch import now, then a rescan every
/// [`RESCAN_EVERY`]. Fire and forget: failures only reach the debug log.
pub fn schedule<V: 'static>(backend: Backend, cx: &mut Context<V>) -> Task<()> {
    cx.spawn(async move |_, cx| {
        let mut trigger = ImportTrigger::Launch;
        loop {
            let job = backend.spawn(move |c| async move { c.session_import_tick(trigger).await });
            if let Err(e) = jobs::flatten(job.await) {
                tracing::debug!(error = %e, "session import tick failed");
            }
            cx.background_executor().timer(RESCAN_EVERY).await;
            trigger = ImportTrigger::Background;
        }
    })
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
