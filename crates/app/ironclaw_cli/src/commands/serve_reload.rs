//! SIGHUP → `config.toml` live reload for `zlo serve`.
//!
//! The reload itself (validation, diff, fail-closed apply, restart-required
//! reporting) is `ironclaw_operator::RebornConfigReloader`'s; this file only
//! turns a signal into a call and records the report. It is a background task,
//! so it logs at `debug!` only (repo logging rule); the target is captured by
//! the operator Logs panel's default filter (`runtime::init_tracing`).

use std::sync::Arc;

use ironclaw_operator::RebornConfigReloader;

/// Log target for reload reports. Kept in sync with the operator log filter
/// default in `runtime::init_tracing`.
pub(crate) const CONFIG_RELOAD_LOG_TARGET: &str = "ironclaw::reborn::config_reload";

/// Spawn the SIGHUP listener. Abort the handle at shutdown. On non-unix there
/// is no SIGHUP and the task exits immediately.
pub(crate) fn spawn_sighup_config_reload(
    reloader: Arc<RebornConfigReloader>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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
                    return;
                }
            };
            while hangups.recv().await.is_some() {
                reload_and_record(&reloader, "sighup").await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = reloader;
        }
    })
}

/// Run one reload and record its report. Key names and redacted reasons
/// only; the report never carries config values.
pub(crate) async fn reload_and_record(reloader: &RebornConfigReloader, trigger: &'static str) {
    let report = reloader.reload().await;
    tracing::debug!(
        target: CONFIG_RELOAD_LOG_TARGET,
        trigger,
        outcome = ?report.outcome,
        applied = ?report.applied,
        restart_required = ?report.restart_required,
        "config reload: {report}"
    );
}
