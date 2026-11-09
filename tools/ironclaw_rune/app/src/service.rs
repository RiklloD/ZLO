//! ZLO owns the local backend's lifetime: opening the app starts
//! `Contents/MacOS/zlo-backend serve` as a child process, quitting stops it.
//!
//! Running it as a child (not a launchd job) makes ZLO the "responsible"
//! app for everything the backend and its computer-use engine do, so macOS
//! asks for Accessibility and Screen Recording once, for ZLO.
//!
//! A backend that exits on its own is restarted with backoff. Outside the app
//! bundle (`cargo run`) the installed `~/.ironclaw/bin/ironclaw` is used.
//!
//! Nothing starts or restarts unless the app said it may
//! ([`set_allowed`], from `remote::sync_local_backend`): never while
//! another computer is active, "Run IronClaw on this Mac" is off, the
//! computers list is unreadable, or `IRONCLAW_DESKTOP_URL` points the app
//! at another gateway.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The launchd job older installs used; retired on first start.
const LEGACY_LABEL: &str = "com.ironclaw.reborn";
const PID_FILE: &str = "zlo-backend.pid";
const STOP_GRACE: Duration = Duration::from_secs(6);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run this long resets the restart backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

struct Supervisor {
    /// The running backend's pid; 0 while none runs. The supervisor thread
    /// owns the `Child` and blocks in `wait` on it (no polling), so `stop`
    /// and `restart` signal it by pid.
    pid: AtomicU32,
    stopping: AtomicBool,
    running: AtomicBool,
}

fn supervisor() -> &'static Arc<Supervisor> {
    static SUPERVISOR: OnceLock<Arc<Supervisor>> = OnceLock::new();
    SUPERVISOR.get_or_init(|| {
        Arc::new(Supervisor {
            pid: AtomicU32::new(0),
            stopping: AtomicBool::new(false),
            running: AtomicBool::new(false),
        })
    })
}

/// Whether this app may start or restart a backend on this Mac. Off until
/// the app decided (`remote::sync_local_backend`).
static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Say whether this app may run the local backend.
pub fn set_allowed(allowed: bool) {
    ALLOWED.store(allowed, Ordering::SeqCst);
}

pub fn allowed() -> bool {
    ALLOWED.load(Ordering::SeqCst)
}

/// The supervisor runs (or is starting) a backend.
#[cfg(test)]
pub fn is_running() -> bool {
    supervisor().running.load(Ordering::SeqCst)
}

struct Paths {
    backend: PathBuf,
    home: PathBuf,
}

fn paths() -> Option<Paths> {
    if std::env::var_os("IRONCLAW_DESKTOP_URL").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let home = zlo_client::reborn_home().ok()?;
    Some(Paths {
        backend: backend_binary()?,
        home,
    })
}

/// The backend binary this app runs: the bundled `zlo-backend`, else the
/// installed CLI. Also what `remote enable` runs (Settings → Computers).
pub fn backend_binary() -> Option<PathBuf> {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("zlo-backend")));
    let installed = dirs::home_dir().map(|h| h.join(".ironclaw/bin/ironclaw"));
    [bundled, installed]
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
}

/// A Light bundle (`bundle.sh --light`): ZLO runs from an app bundle that
/// has no `zlo-backend` next to it. `cargo run` is never one.
pub fn light_bundle() -> bool {
    static LIGHT: OnceLock<bool> = OnceLock::new();
    *LIGHT.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .is_some_and(|exe| is_light_bundle(&exe))
    })
}

/// `…/Some.app/Contents/MacOS/zlo` with no `zlo-backend` beside it.
pub fn is_light_bundle(exe: &Path) -> bool {
    let Some(dir) = exe.parent() else {
        return false;
    };
    let in_bundle = dir.ends_with("Contents/MacOS")
        && dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::extension)
            .is_some_and(|ext| ext == "app");
    in_bundle && !dir.join("zlo-backend").is_file()
}

/// Start the backend unless it already runs, or the app may not run one
/// ([`set_allowed`]). Called by every connect (`remote::sync_local_backend`).
pub fn start() {
    // Unit tests never spawn a real backend, allowed or not.
    if !allowed() || cfg!(test) {
        return;
    }
    let Some(paths) = paths() else {
        return;
    };
    let sup = supervisor().clone();
    if sup.running.swap(true, Ordering::SeqCst) {
        return;
    }
    sup.stopping.store(false, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("zlo-backend".into())
        .spawn(move || {
            retire_launchd_job();
            stop_stale(&paths.home);
            supervise(&sup, &paths);
            sup.running.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Restart the backend this app runs (a CLI installed since it started is
/// registered only at startup): SIGTERM it and let the supervisor start it
/// again. `false` when this app does not run one.
pub fn restart() -> bool {
    let sup = supervisor();
    if !allowed() || sup.stopping.load(Ordering::SeqCst) {
        return false;
    }
    match backend_pid() {
        Some(pid) => {
            tracing::debug!(pid, "backend restarting");
            terminate(pid);
            true
        }
        None => false,
    }
}

/// SIGTERM the backend without waiting: its graceful shutdown drains open
/// streams, and this app's streams close only once it exits. A backend that
/// is still around at the next launch is stopped by `stop_stale`.
/// GPUI stopped the backend from its quit hook (`on_app_quit`). Rune's
/// `App::run` does not return when AppKit terminates the app itself (Quit
/// from the Dock, AppleScript `quit`, logout), so `main`'s stop after `run`
/// never ran and the backend was left to exit on its own a minute later.
/// Observe `NSApplicationWillTerminateNotification` too: it stops the
/// backend and flushes queued saves on every way out. Call once at launch
/// on the main thread.
#[cfg(target_os = "macos")]
pub fn stop_on_terminate() {
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    let (Some(center_class), Some(string)) = (
        AnyClass::get(c"NSNotificationCenter"),
        AnyClass::get(c"NSString"),
    ) else {
        return;
    };
    // SAFETY: plain Foundation calls on the main thread; the block matches
    // `void (^)(NSNotification *)` and the center copies and keeps it.
    unsafe {
        let center: *mut AnyObject = msg_send![center_class, defaultCenter];
        let name: *mut AnyObject = msg_send![
            string,
            stringWithUTF8String: c"NSApplicationWillTerminateNotification".as_ptr()
        ];
        if center.is_null() || name.is_null() {
            return;
        }
        let block = RcBlock::new(|_note: *mut AnyObject| {
            crate::persist::global().flush(Duration::from_secs(3));
            stop();
            crate::remote::tunnel::close();
        });
        let nil: *mut AnyObject = std::ptr::null_mut();
        let _observer: *mut AnyObject = msg_send![
            center,
            addObserverForName: name,
            object: nil,
            queue: nil,
            usingBlock: &*block
        ];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn stop_on_terminate() {}

/// Stop the backend this app started (never another one). Quit, and
/// turning "Run IronClaw on this Mac" off.
pub fn stop() {
    let sup = supervisor();
    sup.stopping.store(true, Ordering::SeqCst);
    if let Some(pid) = backend_pid() {
        terminate(pid);
    }
}

/// The backend this app runs, while it runs.
pub fn backend_pid() -> Option<u32> {
    Some(supervisor().pid.load(Ordering::SeqCst)).filter(|pid| *pid != 0)
}

fn supervise(sup: &Supervisor, paths: &Paths) {
    let mut backoff = Duration::from_secs(1);
    while !sup.stopping.load(Ordering::SeqCst) {
        let started = Instant::now();
        let mut child = match spawn(paths) {
            Ok(child) => child,
            Err(e) => {
                tracing::error!(error = %e, backend = %paths.backend.display(), "backend failed to start");
                return;
            }
        };
        let pid = child.id();
        tracing::debug!(pid, backend = %paths.backend.display(), "backend started");
        let _ = fs::write(paths.home.join(PID_FILE), pid.to_string());
        sup.pid.store(pid, Ordering::SeqCst);
        // Block until it exits: no wakeups while it runs.
        let status = child.wait().ok();
        sup.pid.store(0, Ordering::SeqCst);
        let _ = fs::remove_file(paths.home.join(PID_FILE));
        if sup.stopping.load(Ordering::SeqCst) {
            tracing::debug!(?status, "backend stopped");
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

/// A backend left over from a crashed ZLO still holds the port.
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
            .is_some_and(|comm| {
                comm.contains("zlo-backend")
                    || comm.contains("numoh-backend")
                    || comm.contains("ironclaw")
            })
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
        Ok(()) => tracing::debug!(kept = %kept.display(), "retired the old launchd backend job"),
        Err(e) => tracing::warn!(error = %e, "could not move the old launchd plist"),
    }
}

/// Tests that flip [`set_allowed`] hold this, so they never race.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_light_bundle_is_an_app_bundle_without_the_backend() {
        let root = std::env::temp_dir().join(format!("zlo-light-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let macos = root.join("ZLO Light.app/Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("zlo");
        assert!(is_light_bundle(&exe));
        fs::write(macos.join("zlo-backend"), b"").unwrap();
        assert!(!is_light_bundle(&exe), "the full bundle");
        // `cargo run`: no bundle, never light.
        let target = root.join("target/debug");
        fs::create_dir_all(&target).unwrap();
        assert!(!is_light_bundle(&target.join("zlo")));
        let not_app = root.join("Folder/Contents/MacOS");
        fs::create_dir_all(&not_app).unwrap();
        assert!(!is_light_bundle(&not_app.join("zlo")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_starts_or_restarts_unless_allowed() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_allowed(false);
        start();
        assert!(!is_running(), "a backend was spawned while not allowed");
        assert!(!restart(), "a restart while not allowed");
    }
}
