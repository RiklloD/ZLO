//! The app owns the local gateway's lifetime, like a normal app's helper:
//! opening IronClaw loads the launchd job (`ironclaw service install` wrote
//! it), quitting unloads it so the gateway stops with the window.
//!
//! `load -w` / `unload -w` match what `ironclaw service start|stop` run, and
//! the `-w` keeps launchd from starting the gateway at login on its own.
//! Skipped when `IRONCLAW_DESKTOP_URL` points the app at another gateway.

use std::path::PathBuf;
use std::process::{Command, Stdio};

const LABEL: &str = "com.ironclaw.reborn";

fn plist() -> Option<PathBuf> {
    if std::env::var_os("IRONCLAW_DESKTOP_URL").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    path.exists().then_some(path)
}

/// Load the job; launchd starts the gateway (`RunAtLoad`). Already loaded is
/// fine: launchctl just says so.
pub fn start() {
    let Some(plist) = plist() else {
        return;
    };
    if let Err(e) = launchctl("load", &plist).and_then(|mut c| c.wait()) {
        tracing::warn!(error = %e, "launchctl load failed");
    }
}

/// Unload without waiting: launchd sends SIGTERM and the gateway finishes on
/// its own once this process (and its open streams) is gone.
pub fn stop() {
    let Some(plist) = plist() else {
        return;
    };
    if let Err(e) = launchctl("unload", &plist) {
        tracing::warn!(error = %e, "launchctl unload failed");
    }
}

fn launchctl(verb: &str, plist: &PathBuf) -> std::io::Result<std::process::Child> {
    Command::new("/bin/launchctl")
        .args([verb, "-w"])
        .arg(plist)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}
