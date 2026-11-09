//! Where to look when something breaks: the app writes its own log to
//! `~/Library/Logs/ZLO/rune.log` (stderr too, for `cargo run`), and the
//! backend ZLO starts writes `serve.stderr.log` under its home. Both open in
//! Console from the connection screen (only the app's while the backend is
//! remote). (GPUI `logs.rs`; the GPUI app keeps
//! `desktop.log`, so the two apps never interleave lines in one file.)
//!
//! Headless runs (`--snap`, `--tree`, `--script`) log to stderr only, so a
//! scene render never lands in the person's log file.
//!
//! `IRONCLAW_DESKTOP_LOG` overrides the filter (`RUST_LOG` syntax).

use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::MakeWriterExt as _;

/// The log starts over past this size; the previous one is kept as `.1`.
const ROTATE_BYTES: u64 = 4 * 1024 * 1024;

pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("IRONCLAW_DESKTOP_LOG")
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("warn,zlo_rune=info,zlo_client=info")
        });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    let file = if rune::is_headless() {
        None
    } else {
        open_desktop_log()
    };
    match file {
        Some(file) => builder
            .with_writer(std::io::stderr.and(Mutex::new(file)))
            .init(),
        None => builder.init(),
    }
}

fn open_desktop_log() -> Option<File> {
    let path = desktop_log()?;
    fs::create_dir_all(path.parent()?).ok()?;
    if fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

pub fn desktop_log() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/ZLO/rune.log"))
}

pub fn gateway_log() -> Option<PathBuf> {
    zlo_client::reborn_home()
        .ok()
        .map(|home| home.join("logs/serve.stderr.log"))
}

/// The log files worth opening: the app's, and this Mac's gateway's unless
/// the backend runs on another computer (its log is there, read in the app
/// through Operator → Logs; the local one belongs to a backend not in use).
fn files_for(remote: bool) -> Vec<PathBuf> {
    let gateway = if remote { None } else { gateway_log() };
    [desktop_log(), gateway].into_iter().flatten().collect()
}

/// Open the app's and the gateway's log files in Console.
pub fn open_files(s: &crate::state::State) {
    let files: Vec<PathBuf> = files_for(crate::remote::backend_is_remote(s))
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if files.is_empty() {
        tracing::warn!("no log files to open");
        return;
    }
    let opened = Command::new("/usr/bin/open")
        .args(["-a", "Console"])
        .args(&files)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = opened {
        tracing::warn!(error = %e, "could not open the log files");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_remote_backend_leaves_this_macs_gateway_log_out() {
        let remote = super::files_for(true);
        assert!(
            remote
                .iter()
                .all(|p| p.ends_with("Library/Logs/ZLO/rune.log"))
        );
        if let Some(gateway) = super::gateway_log() {
            assert!(super::files_for(false).contains(&gateway));
            assert!(!remote.contains(&gateway));
        }
    }
}
