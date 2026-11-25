//! ZLO owns the local backend's lifetime: opening the app starts
//! `Contents/MacOS/zlo-backend serve` as a child process, quitting stops it.
//!
//! Running it as a child (not a launchd job) makes ZLO the "responsible"
//! app for everything the backend and its computer-use engine do, so macOS
//! asks for Accessibility and Screen Recording once, for ZLO.
//!
//! A backend that exits on its own is restarted with backoff. Outside the app
//! bundle (`cargo run`) the installed `~/.zlo/bin/zlo` is used (or a pre-ZLO
//! `~/.ironclaw/bin/ironclaw`).
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

/// launchd jobs that would run a second backend beside this app's child:
/// the pre-ZLO `ironclaw service` label and `zlo service`'s own. Retired on
/// start.
const LAUNCHD_LABELS: [&str; 2] = ["com.ironclaw.reborn", "com.zlo.serve"];
/// Read by `zlo triage` to report whether this app's backend runs.
const PID_FILE: &str = ironclaw_config::DESKTOP_BACKEND_PID_FILE;
/// The backend's stderr, under `<home>/logs`.
const STDERR_LOG: &str = "serve.stderr.log";
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
    /// `backend` is this app bundle's own `zlo-backend`.
    bundled: bool,
}

fn paths() -> Option<Paths> {
    if std::env::var_os("IRONCLAW_DESKTOP_URL").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let home = match zlo_client::prepare_reborn_home() {
        Ok(home) => home,
        Err(error) => {
            // Never start the backend on a home we could not resolve (for
            // example a failed move of the old data folder): say why instead.
            tracing::error!(%error, "not starting the local backend");
            return None;
        }
    };
    let backend = backend_binary()?;
    let bundled = paths_bundled(Some(&backend));
    Some(Paths {
        backend,
        home,
        bundled,
    })
}

/// The backend this app would run is its own bundled `zlo-backend`, the
/// one that makes its token on a first start (`--create-token-if-missing`).
pub fn provisions_token() -> bool {
    !light_bundle() && paths_bundled(backend_binary().as_deref())
}

fn paths_bundled(backend: Option<&Path>) -> bool {
    let own = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("zlo-backend")));
    backend.is_some_and(|b| own.as_deref() == Some(b))
}

/// The backend binary this app runs: the bundled `zlo-backend`, else the
/// installed CLI. Also what `remote enable` runs (Settings → Computers).
pub fn backend_binary() -> Option<PathBuf> {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("zlo-backend")));
    let installed = dirs::home_dir().map(|h| h.join(".zlo/bin/zlo"));
    let legacy = dirs::home_dir().map(|h| h.join(".ironclaw/bin/ironclaw"));
    [bundled, installed, legacy]
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
    // A new supervising lifecycle: an earlier one's error is history.
    let lifecycle = new_lifecycle();
    clear_startup_failure();
    std::thread::Builder::new()
        .name("zlo-backend".into())
        .spawn(move || {
            retire_launchd_jobs(&paths.home);
            stop_stale(&paths.home);
            supervise(&sup, &paths, lifecycle);
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
/// Rune's `App::run` does not return when AppKit terminates the app itself (Quit
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
            crate::cli_install::stop_all();
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
    new_lifecycle();
    clear_startup_failure();
    if let Some(pid) = backend_pid() {
        terminate(pid);
    }
}

/// The backend this app runs, while it runs.
pub fn backend_pid() -> Option<u32> {
    Some(supervisor().pid.load(Ordering::SeqCst)).filter(|pid| *pid != 0)
}

/// Why this app's backend last stopped on its own (its own `Error:`
/// output, or why it could not be spawned). Lives for one supervisor
/// lifecycle: cleared when the app starts or stops supervising, and when
/// it connects to This Mac. The connect screen shows it (for This Mac's
/// retryable failures only) instead of waiting out the startup grace.
#[cfg(not(test))]
static STARTUP_FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(not(test))]
fn with_failure<T>(f: impl FnOnce(&mut Option<String>) -> T) -> Option<T> {
    STARTUP_FAILURE.lock().ok().map(|mut g| f(&mut g))
}

// Tests never run a supervisor; each test thread gets its own slot, so a
// test that connects can't clear another's.
#[cfg(test)]
thread_local! {
    static STARTUP_FAILURE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_failure<T>(f: impl FnOnce(&mut Option<String>) -> T) -> Option<T> {
    Some(STARTUP_FAILURE.with(|c| f(&mut c.borrow_mut())))
}

/// Bumped whenever the app starts or stops supervising: a supervisor only
/// publishes a failure while its own lifecycle is the current one, so a
/// stopped one can't bring back an error a later start already cleared.
static LIFECYCLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn new_lifecycle() -> u64 {
    LIFECYCLE.fetch_add(1, Ordering::SeqCst) + 1
}

/// Publish (or clear) `line` for `lifecycle`, unless a newer one began.
fn publish_failure(lifecycle: u64, line: Option<String>) {
    with_failure(|f| {
        if LIFECYCLE.load(Ordering::SeqCst) == lifecycle {
            *f = line;
        }
    });
}

pub fn startup_failure() -> Option<String> {
    with_failure(|f| f.clone()).flatten()
}

pub fn clear_startup_failure() {
    with_failure(|f| *f = None);
}

#[cfg(test)]
pub(crate) fn set_startup_failure(line: Option<String>) {
    with_failure(|f| *f = line);
}

/// The error a backend printed before it exited: from its last `Error:`
/// line on (anyhow's message and causes), ANSI colours stripped, bounded.
/// `None` when it printed none (killed, or it never got that far).
fn startup_error(stderr: &str) -> Option<String> {
    let plain = strip_ansi(stderr);
    let lines: Vec<&str> = plain.lines().collect();
    let start = lines.iter().rposition(|l| l.starts_with("Error: "))?;
    let mut out = lines[start..]
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("\n");
    if out.len() > 800 {
        let mut end = 800;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    Some(out)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ … final byte in @..~
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What this child wrote to `serve.stderr.log`: read through the app's
/// own handle to the file it gave the child (the same inode, whatever a
/// rename does to the path), from the length it had at spawn (or the start,
/// if it was truncated since), at most the last 64 KiB — anyhow prints the
/// error last, right before the exit.
#[cfg(unix)]
fn child_stderr(log: &fs::File, from: u64) -> String {
    use std::os::unix::fs::FileExt as _;
    const MAX: u64 = 64 * 1024;
    let Ok(len) = log.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let from = if len < from { 0 } else { from }.max(len.saturating_sub(MAX));
    let mut buf = vec![0_u8; usize::try_from(len - from).unwrap_or(0)];
    match log.read_exact_at(&mut buf, from) {
        Ok(()) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(not(unix))]
fn child_stderr(_log: &fs::File, _from: u64) -> String {
    String::new()
}

fn supervise(sup: &Supervisor, paths: &Paths, lifecycle: u64) {
    let mut backoff = Duration::from_secs(1);
    while !sup.stopping.load(Ordering::SeqCst) {
        let started = Instant::now();
        let (mut child, log, log_from) = match spawn(paths) {
            Ok(spawned) => spawned,
            Err(e) => {
                tracing::error!(error = %e, backend = %paths.backend.display(), "backend failed to start");
                publish_failure(
                    lifecycle,
                    Some(format!(
                        "ZLO could not start {}: {e}",
                        paths.backend.display()
                    )),
                );
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
        // Its own words, however long it ran; `None` when it said nothing.
        publish_failure(lifecycle, startup_error(&child_stderr(&log, log_from)));
        tracing::warn!(?status, retry_in = ?backoff, "backend exited; restarting");
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Start one backend: the child, the app's handle to the stderr log the
/// child appends to, and that log's length at spawn.
fn spawn(paths: &Paths) -> std::io::Result<(Child, fs::File, u64)> {
    let logs = paths.home.join("logs");
    let workspace = paths.home.join("workspace");
    fs::create_dir_all(&logs)?;
    fs::create_dir_all(&workspace)?;
    let log = |name: &str| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(logs.join(name))
    };
    let stderr_log = log(STDERR_LOG)?;
    let from = stderr_log.metadata()?.len();
    let mut command = Command::new(&paths.backend);
    command.arg("serve");
    // On a fresh Mac nothing has made the WebUI token yet: this backend
    // makes it on its first start (loopback only, never replacing one).
    // Only the bundled backend, which matches this app, gets the flag: an
    // older installed CLI would refuse it.
    if paths.bundled {
        command.arg("--create-token-if-missing");
    }
    set_home_env(&mut command, &paths.home);
    // A Finder-opened app has launchd's PATH: give the backend a terminal's,
    // so it finds the coding-agent CLIs (and imports their sessions).
    command.env("PATH", crate::login_path::path());
    let child = command
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(log("serve.stdout.log")?)
        .stderr(stderr_log.try_clone()?)
        .spawn()?;
    Ok((child, stderr_log, from))
}

fn terminate(pid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

/// Point a backend command at `home`: `ZLO_HOME`, plus the pre-ZLO
/// `IRONCLAW_REBORN_HOME` for an older installed binary that only knows it.
pub fn set_home_env(command: &mut Command, home: &Path) {
    command
        .env("ZLO_HOME", home)
        .env("IRONCLAW_REBORN_HOME", home);
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
/// from starting at login) and move its plist next to the backend's data
/// (`<data>/retired-<label>.plist`), so two backends never fight over the
/// port.
fn retire_launchd_jobs(data: &Path) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    for label in LAUNCHD_LABELS {
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        if !plist.exists() {
            continue;
        }
        let _ = Command::new("/bin/launchctl")
            .args(["unload", "-w"])
            .arg(&plist)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let kept = data.join(format!("retired-{label}.plist"));
        match fs::rename(&plist, &kept) {
            Ok(()) => tracing::debug!(kept = %kept.display(), "retired a launchd backend job"),
            Err(e) => tracing::warn!(error = %e, "could not move a launchd backend plist"),
        }
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
    fn a_backend_that_stops_is_reported_by_its_own_error() {
        let log = "\u{1b}[2m2026-11-25T11:46:19Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m no LLM selection configured\n\
                   Error: failed to assemble Reborn runtime for `serve`\n\n\
                   Caused by:\n    0: workspace root must not overlap default skill root /skills\n";
        assert_eq!(
            startup_error(log).as_deref(),
            Some(
                "Error: failed to assemble Reborn runtime for `serve`\nCaused by:\n    0: workspace root must not overlap default skill root /skills"
            )
        );
        // A supervisor that was stopped (or replaced) can't publish its
        // error over a newer lifecycle's state.
        let stale = new_lifecycle();
        let current = new_lifecycle();
        clear_startup_failure();
        publish_failure(stale, Some("Error: old".into()));
        assert_eq!(startup_failure(), None);
        publish_failure(current, Some("Error: new".into()));
        assert_eq!(startup_failure().as_deref(), Some("Error: new"));
        clear_startup_failure();

        // Killed, or still starting: nothing it said is an error.
        assert_eq!(startup_error("\u{1b}[33m WARN\u{1b}[0m slow start\n"), None);
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
