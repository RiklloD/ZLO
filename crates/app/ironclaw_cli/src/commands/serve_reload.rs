//! SIGHUP → `config.toml` live reload for `zlo serve`.
//!
//! The reload itself (validation, diff, fail-closed apply, restart-required
//! reporting) is `ironclaw_operator::RebornConfigReloader`'s; this file only
//! turns a signal into a call and records the report. It is a background task,
//! so it logs at `debug!` only (repo logging rule); the target is captured by
//! the operator Logs panel's default filter (`runtime::init_tracing`).
//!
//! Installing a SIGHUP handler replaces the default "terminate" disposition.
//! That is right for the installed service (no controlling terminal; SIGHUP
//! is the conventional reload signal) and wrong for a server started in a
//! terminal, where closing the window or dropping the SSH session must still
//! stop it. [`should_reload_on_sighup`] makes that call from whether stdin is
//! a terminal.
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

use std::sync::Arc;

use ironclaw_config::ConfigReloadReport;
use ironclaw_operator::RebornConfigReloader;

/// Log target for reload reports. Kept in sync with the operator log filter
/// default in `runtime::init_tracing`.
pub(crate) const CONFIG_RELOAD_LOG_TARGET: &str = "ironclaw::reborn::config_reload";

/// Whether `zlo serve` should take over SIGHUP for config reload. Only when
/// stdin is not a terminal, i.e. the process runs as a service (launchd and
/// systemd give it `/dev/null`); attached to a terminal, a hangup keeps its
/// default meaning and stops the server.
pub(crate) fn should_reload_on_sighup(stdin_is_terminal: bool) -> bool {
    !stdin_is_terminal
}

/// Install the SIGHUP handler and spawn the listener. The handler is installed
/// before this returns, so a SIGHUP sent afterwards always reloads. `None`
/// when the handler could not be installed, and always on non-unix (no
/// SIGHUP). Abort the handle at shutdown. Must be called inside a tokio
/// runtime.
pub(crate) fn spawn_sighup_config_reload(
    reloader: Arc<RebornConfigReloader>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_sighup_config_reload_with(reloader, |_| {})
}

fn spawn_sighup_config_reload_with(
    reloader: Arc<RebornConfigReloader>,
    observe: impl Fn(&ConfigReloadReport) + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hangups = match signal(SignalKind::hangup()) {
            Ok(hangups) => hangups,
            Err(error) => {
                tracing::debug!(
                    target: CONFIG_RELOAD_LOG_TARGET,
                    %error,
                    "could not install the SIGHUP handler; config reload on SIGHUP is off"
                );
                return None;
            }
        };
        Some(tokio::spawn(async move {
            while hangups.recv().await.is_some() {
                let report = reload_and_record(&reloader, "sighup").await;
                observe(&report);
            }
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = (reloader, observe);
        None
    }
}

/// Run one reload and record its report. Key names and redacted reasons
/// only; the report never carries config values.
async fn reload_and_record(
    reloader: &RebornConfigReloader,
    trigger: &'static str,
) -> ConfigReloadReport {
    let report = reloader.reload().await;
    tracing::debug!(
        target: CONFIG_RELOAD_LOG_TARGET,
        trigger,
        outcome = ?report.outcome,
        applied = ?report.applied,
        restart_required = ?report.restart_required,
        "config reload: {report}"
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sighup_reloads_only_when_detached_from_a_terminal() {
        assert!(should_reload_on_sighup(false), "service: SIGHUP reloads");
        assert!(
            !should_reload_on_sighup(true),
            "terminal: SIGHUP keeps its default and stops the server"
        );
    }

    /// The wiring itself: a real SIGHUP to this process runs a reload of the
    /// file on disk and reports what changed.
    #[cfg(unix)]
    #[tokio::test]
    async fn sighup_runs_a_reload_of_the_file_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write config");
        let boot_file = ironclaw_config::RebornConfigFile::load(&path).expect("parse");
        let reloader = Arc::new(RebornConfigReloader::new(path.clone(), boot_file, None));

        let (reports, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task = spawn_sighup_config_reload_with(reloader, move |report| {
            let _ = reports.send(report.clone());
        })
        .expect("SIGHUP handler installs");

        std::fs::write(&path, "[webui]\nlisten_port = 4111\n").expect("edit config");
        // SAFETY: `kill` on our own pid with a signal whose handler is
        // installed above; no memory is touched.
        let sent = unsafe { libc::kill(libc::getpid(), libc::SIGHUP) };
        assert_eq!(sent, 0, "raise SIGHUP");

        let report = tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
            .await
            .expect("a reload ran within the timeout")
            .expect("report");
        assert_eq!(
            report.outcome,
            ironclaw_config::ConfigReloadOutcome::RestartRequired
        );
        assert_eq!(report.restart_required, vec!["webui"]);
        task.abort();
    }
}
