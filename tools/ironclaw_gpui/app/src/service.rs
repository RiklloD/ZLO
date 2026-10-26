//! Numoh owns the local backend's lifetime: opening the app starts
//! `Contents/MacOS/numoh-backend serve` as a child process, quitting stops it.
//!
//! Running it as a child (not a launchd job) makes Numoh the "responsible"
//! app for everything the backend and its computer-use engine do, so macOS
//! asks for Accessibility and Screen Recording once, for Numoh.
//!
//! A backend that exits on its own is restarted with backoff. Outside the app
//! bundle (`cargo run`) the installed `~/.ironclaw/bin/ironclaw` is used.
//! Skipped when `IRONCLAW_DESKTOP_URL` points the app at another gateway.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The launchd job older installs used; retired on first start.
const LEGACY_LABEL: &str = "com.ironclaw.reborn";
const PID_FILE: &str = "numoh-backend.pid";
const STOP_GRACE: Duration = Duration::from_secs(6);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run this long resets the restart backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

struct Supervisor {
    child: Mutex<Option<Child>>,
    stopping: AtomicBool,
    running: AtomicBool,
}

fn supervisor() -> &'static Arc<Supervisor> {
    static SUPERVISOR: OnceLock<Arc<Supervisor>> = OnceLock::new();
    SUPERVISOR.get_or_init(|| {
        Arc::new(Supervisor {
            child: Mutex::new(None),
            stopping: AtomicBool::new(false),
            running: AtomicBool::new(false),
        })
    })
}

struct Paths {
    backend: PathBuf,
    home: PathBuf,
}

fn paths() -> Option<Paths> {
    if std::env::var_os("IRONCLAW_DESKTOP_URL").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let home = ironclaw_gpui_client::reborn_home().ok()?;
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("numoh-backend")));
    let installed = dirs::home_dir().map(|h| h.join(".ironclaw/bin/ironclaw"));
    let backend = [bundled, installed]
        .into_iter()
        .flatten()
        .find(|p| p.is_file())?;
    Some(Paths { backend, home })
}

/// Start the backend unless it already runs. Called at launch and by the
/// connection screen's "Try again".
pub fn start() {
    let Some(paths) = paths() else {
        return;
    };
    let sup = supervisor().clone();
    if sup.running.swap(true, Ordering::SeqCst) {
        return;
    }
    sup.stopping.store(false, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("numoh-backend".into())
        .spawn(move || {
            retire_launchd_job();
            stop_stale(&paths.home);
            supervise(&sup, &paths);
            sup.running.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// SIGTERM the backend without waiting: its graceful shutdown drains open
/// streams, and this app's streams close only once it exits. A backend that
/// is still around at the next launch is stopped by `stop_stale`.
pub fn stop() {
    let sup = supervisor();
    sup.stopping.store(true, Ordering::SeqCst);
    if let Some(pid) = sup
        .child
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(Child::id))
    {
        terminate(pid);
    }
}

fn supervise(sup: &Supervisor, paths: &Paths) {
    let mut backoff = Duration::from_secs(1);
    while !sup.stopping.load(Ordering::SeqCst) {
        let started = Instant::now();
        let child = match spawn(paths) {
            Ok(child) => child,
            Err(e) => {
                tracing::error!(error = %e, backend = %paths.backend.display(), "backend failed to start");
                return;
            }
        };
        let pid = child.id();
        tracing::info!(pid, backend = %paths.backend.display(), "backend started");
        let _ = fs::write(paths.home.join(PID_FILE), pid.to_string());
        if let Ok(mut slot) = sup.child.lock() {
            *slot = Some(child);
        }
        let status = wait(sup);
        let _ = fs::remove_file(paths.home.join(PID_FILE));
        if sup.stopping.load(Ordering::SeqCst) {
            tracing::info!(?status, "backend stopped");
            return;
        }
        if started.elapsed() > HEALTHY_RUN {
            backoff = Duration::from_secs(1);
        }
        tracing::warn!(?status, retry_in = ?backoff, "backend exited; restarting");
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Poll rather than block, so `stop` can take the lock to read the pid.
fn wait(sup: &Supervisor) -> Option<std::process::ExitStatus> {
    loop {
        {
            let mut slot = sup.child.lock().ok()?;
            let child = slot.as_mut()?;
            match child.try_wait() {
                Ok(Some(status)) => {
                    *slot = None;
                    return Some(status);
                }
                Ok(None) => {}
                Err(_) => {
                    *slot = None;
                    return None;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn spawn(paths: &Paths) -> std::io::Result<Child> {
    let logs = paths.home.join("logs");
    let workspace = paths.home.join("workspace");
    fs::create_dir_all(&logs)?;
    fs::create_dir_all(&workspace)?;
    let log = |name: &str| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.join(name))
    };
    Command::new(&paths.backend)
        .arg("serve")
        .env("IRONCLAW_REBORN_HOME", &paths.home)
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(log("serve.stdout.log")?)
        .stderr(log("serve.stderr.log")?)
        .spawn()
}

fn terminate(pid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

/// A backend left over from a crashed Numoh still holds the port.
fn stop_stale(home: &Path) {
    let Some(pid) = fs::read_to_string(home.join(PID_FILE))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    let alive = |pid: u32| {
        Command::new("/bin/ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .is_some_and(|comm| comm.contains("numoh-backend") || comm.contains("ironclaw"))
    };
    if !alive(pid) {
        return;
    }
    tracing::warn!(pid, "stopping a backend left over from a previous run");
    terminate(pid);
    let deadline = Instant::now() + STOP_GRACE;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(pid) {
        tracing::warn!(pid, "leftover backend did not stop; killing it");
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Older installs ran the backend as a launchd job. Unload it (`-w` keeps it
/// from starting at login) and move its plist next to the backend's data, so
/// two backends never fight over the port.
fn retire_launchd_job() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let plist = home
        .join("Library/LaunchAgents")
        .join(format!("{LEGACY_LABEL}.plist"));
    if !plist.exists() {
        return;
    }
    let _ = Command::new("/bin/launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let kept = home.join(".ironclaw/reborn/retired-launchd.plist");
    match fs::rename(&plist, &kept) {
        Ok(()) => tracing::info!(kept = %kept.display(), "retired the old launchd backend job"),
        Err(e) => tracing::warn!(error = %e, "could not move the old launchd plist"),
    }
}
