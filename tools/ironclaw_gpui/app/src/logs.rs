//! Where to look when something breaks: the app writes its own log to
//! `~/Library/Logs/Numoh/desktop.log` (stderr too, for `cargo run`), and
//! the backend Numoh starts writes `serve.stderr.log` under its home. Both
//! open in Console from the connection screen.
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
            tracing_subscriber::EnvFilter::new("warn,numoh=info,ironclaw_gpui_client=info")
        });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    match open_desktop_log() {
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
    Some(PathBuf::from(home).join("Library/Logs/Numoh/desktop.log"))
}

pub fn gateway_log() -> Option<PathBuf> {
    ironclaw_gpui_client::reborn_home()
        .ok()
        .map(|home| home.join("logs/serve.stderr.log"))
}

/// Open the app's and the gateway's log files in Console.
pub fn open_files() {
    let files: Vec<PathBuf> = [desktop_log(), gateway_log()]
        .into_iter()
        .flatten()
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
