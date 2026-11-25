//! Installing a coding-agent CLI from the app: Settings → Providers → CLI
//! agents and setup's "Connect your agents" both press Install here.
//!
//! The installer (the catalog's fixed `install` command, never anything
//! typed) runs in this Mac's login shell with no terminal attached, its last
//! lines shown under the row. Its worker thread owns it to the end: a stop
//! (Cancel, the deadline, a computer switch) is a request the worker turns
//! into SIGTERM and then SIGKILL for the installer's process group, and the
//! row stays busy until the process is gone. Once it exits cleanly the
//! backend is asked to look again for CLIs (`POST /providers/rescan`), so
//! the agent turns up without cutting any running thread. Only when the
//! backend answers without it (or has no rescan) is this Mac's backend
//! restarted, once, as setup did before.
//!
//! An agent of another computer is installed there: the row shows the
//! command to run on it, and [`watch`] notices it once that backend finds
//! it. The same watch notices an install run in the person's own terminal.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use zlo_client::ClientError;
use zlo_client::orchestration::{CatalogLaunch, ProviderCatalog, RouteProvider};

use crate::jobs;
use crate::pages::providers::catalog::{KNOWN_CLIS, KnownCli, known_cli};
use crate::state::State;

/// How long an installer may run before it is stopped.
#[cfg(not(test))]
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const INSTALL_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a stopped installer gets after SIGTERM before SIGKILL.
#[cfg(not(test))]
const KILL_GRACE: Duration = Duration::from_secs(3);
#[cfg(test)]
const KILL_GRACE: Duration = Duration::from_millis(300);
/// How long output may keep coming after the installer exited (a
/// descendant holding its pipes) before the worker stops reading.
const PIPE_GRACE: Duration = Duration::from_secs(1);
/// Lines of installer output kept for the row.
const TAIL_LINES: usize = 3;
/// Longest output line shown, in characters.
const LINE_CHARS: usize = 160;
/// How often the row's output may redraw while the installer prints.
const TAIL_EVERY: Duration = Duration::from_millis(250);
/// Rescans after a clean install before it gives up on the backend.
const RESCAN_TRIES: u32 = 3;
#[cfg(not(test))]
const RESCAN_RETRY: Duration = Duration::from_secs(1);
#[cfg(test)]
const RESCAN_RETRY: Duration = Duration::from_millis(20);
/// How long a restarted backend gets to come back and list the agent.
#[cfg(not(test))]
const RESTART_WAIT: Duration = Duration::from_secs(45);
#[cfg(test)]
const RESTART_WAIT: Duration = Duration::from_millis(300);
const RESTART_POLL: Duration = Duration::from_secs(1);
/// How often an open CLI list asks the backend to look again.
#[cfg(not(test))]
const WATCH_EVERY: Duration = Duration::from_secs(4);
#[cfg(test)]
const WATCH_EVERY: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The installer is running.
    Running,
    /// A stop was asked; the installer is not gone yet.
    Stopping,
    /// It exited cleanly; the backend is looking for the agent.
    Finding,
    /// It stopped, or the agent was not found after it.
    Failed(String),
    /// Installed, and only a backend restart puts it to use: the person
    /// decides when (a restart stops the threads running now).
    NeedsRestart(String),
}

impl Phase {
    /// The attempt still owns its row: no second installer may start.
    pub fn busy(&self) -> bool {
        matches!(self, Phase::Running | Phase::Stopping | Phase::Finding)
    }
}

pub struct Install {
    pub phase: Phase,
    /// The installer's last lines, oldest first.
    pub tail: VecDeque<String>,
    /// Set to ask the worker to stop the installer.
    stop: Arc<AtomicBool>,
    generation: u64,
}

#[derive(Default)]
pub struct Installs {
    by_runtime: HashMap<&'static str, Install>,
    next_generation: u64,
    /// The watch's generation: a newer [`watch`] or a closed list ends the
    /// older chain.
    watch_generation: u64,
    /// A backend restart (asked here or by setup) may still be coming back
    /// until then: Restart is not offered again meanwhile.
    restart_until: Option<Instant>,
    /// Restarts the person dismissed: the watch does not ask again for them
    /// until the backend restarts or the computer changes.
    restart_dismissed: std::collections::HashSet<&'static str>,
}

impl Installs {
    pub fn get(&self, runtime: &str) -> Option<&Install> {
        self.by_runtime.get(runtime)
    }

    fn current(&self, runtime: &str, generation: u64) -> Option<&Install> {
        self.by_runtime
            .get(runtime)
            .filter(|i| i.generation == generation)
    }

    fn current_mut(&mut self, runtime: &str, generation: u64) -> Option<&mut Install> {
        self.by_runtime
            .get_mut(runtime)
            .filter(|i| i.generation == generation)
    }
}

fn known(runtime: &str) -> Option<&'static KnownCli> {
    known_cli(runtime)
}

/// Whether `catalog` lists `known` as an agent its own CLI runs (not only a
/// legacy runtime that cannot sign in).
pub fn listed(catalog: &[RouteProvider], known: &KnownCli) -> bool {
    catalog.iter().any(|r| {
        (r.provider.starts_with(known.runtime) || r.runtime.starts_with(known.runtime))
            && !(known.legacy_runtime && r.launch == Some(CatalogLaunch::Legacy))
    })
}

/// Whether this app can run `known`'s installer here: it has one, and the
/// backend runs on this Mac.
pub fn can_install_here(s: &State, known: &KnownCli) -> bool {
    known.install.is_some() && backend_on_this_mac(s)
}

/// The backend is this Mac's: not another computer, and reached over
/// loopback (an `IRONCLAW_DESKTOP_URL` pointing elsewhere is not this Mac's
/// whatever the connection is labelled).
pub fn backend_on_this_mac(s: &State) -> bool {
    if crate::remote::backend_is_remote(s) {
        return false;
    }
    if s.fixture {
        return true;
    }
    s.rt.backend
        .as_ref()
        .is_some_and(|b| is_loopback(&b.client().connection().base_url))
}

fn is_loopback(base_url: &str) -> bool {
    let host = url_host(base_url);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Run `runtime`'s installer.
pub fn start(s: &mut State, runtime: &'static str) {
    let Some(known) = known(runtime) else {
        return;
    };
    let Some(command) = known.install else {
        return;
    };
    if !can_install_here(s, known) {
        return;
    }
    begin(s, runtime, command);
}

/// Run `command` as `runtime`'s installer.
fn begin(s: &mut State, runtime: &'static str, command: &'static str) {
    let installs = &mut s.cli_installs;
    if installs.get(runtime).is_some_and(|i| i.phase.busy()) {
        return;
    }
    installs.next_generation += 1;
    let generation = installs.next_generation;
    let stop = Arc::new(AtomicBool::new(false));
    installs.by_runtime.insert(
        runtime,
        Install {
            phase: Phase::Running,
            tail: VecDeque::new(),
            stop: Arc::clone(&stop),
            generation,
        },
    );
    if s.fixture {
        return;
    }
    let ui = jobs::ui_queue(s);
    std::thread::spawn(move || {
        let exit = run_installer(command, &stop, generation, runtime, &ui);
        ui.deliver(move |s, tx| {
            tx.animated();
            finished(s, runtime, generation, exit);
            true
        });
    });
}

/// A scene's install, shown as if it were running or had ended.
pub fn fixture(s: &mut State, runtime: &'static str, phase: Phase, tail: &[&str]) {
    s.cli_installs.next_generation += 1;
    let generation = s.cli_installs.next_generation;
    s.cli_installs.by_runtime.insert(
        runtime,
        Install {
            phase,
            tail: tail.iter().map(|l| l.to_string()).collect(),
            stop: Arc::new(AtomicBool::new(false)),
            generation,
        },
    );
}

/// Stop `runtime`'s installer; its row comes back once the process is gone.
pub fn cancel(s: &mut State, runtime: &str) {
    let Some(install) = s.cli_installs.by_runtime.get_mut(runtime) else {
        return;
    };
    match install.phase {
        Phase::Running => {
            install.stop.store(true, Ordering::SeqCst);
            install.phase = Phase::Stopping;
        }
        Phase::Stopping => {}
        Phase::Finding | Phase::Failed(_) | Phase::NeedsRestart(_) => {
            s.cli_installs.by_runtime.remove(runtime);
        }
    }
}

/// Close a failed install's message, or a restart the person does not
/// want now (the watch does not bring that one back until a restart).
pub fn dismiss(s: &mut State, runtime: &'static str) {
    let installs = &mut s.cli_installs;
    match installs.get(runtime).map(|i| &i.phase) {
        Some(Phase::Failed(_)) => {
            installs.by_runtime.remove(runtime);
        }
        Some(Phase::NeedsRestart(_)) => {
            installs.by_runtime.remove(runtime);
            installs.restart_dismissed.insert(runtime);
        }
        _ => {}
    }
}

/// An install made in Terminal (setup's Open in Terminal, then Done) is
/// looked for like one made here: when the binary is there, the backend
/// rescans, and a restart, if needed, is the person's to press. A closed
/// Terminal with nothing installed changes nothing.
pub fn after_terminal_install(s: &mut State, runtime: &'static str) {
    let Some(known) = known(runtime) else {
        return;
    };
    let binary = known.binary;
    after_terminal_install_with(s, runtime, move || binary_on_login_path(binary));
}

fn after_terminal_install_with(
    s: &mut State,
    runtime: &'static str,
    present: impl FnOnce() -> bool + Send + 'static,
) {
    if s.cli_installs.get(runtime).is_some_and(|i| i.phase.busy()) || s.fixture {
        return;
    }
    // Another computer's Terminal: nothing here can tell whether it ran.
    // The Providers watch and setup's catalog notice it once that backend
    // lists it (or says it needs a restart).
    if !backend_on_this_mac(s) {
        return;
    }
    let installs = &mut s.cli_installs;
    installs.next_generation += 1;
    let generation = installs.next_generation;
    installs.by_runtime.insert(
        runtime,
        Install {
            phase: Phase::Finding,
            tail: VecDeque::new(),
            stop: Arc::new(AtomicBool::new(false)),
            generation,
        },
    );
    jobs::blocking(s, present, move |s, present, tx| {
        if s.cli_installs.current(runtime, generation).is_none() {
            return;
        }
        if present {
            find(s, runtime, generation, 1);
        } else {
            tx.animated();
            s.cli_installs.by_runtime.remove(runtime);
        }
    });
}

/// How long the login shell gets to say where a program is.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The program is on the login shell's PATH (where installers put it). A
/// shell that takes longer than [`LOOKUP_TIMEOUT`] (a slow profile) is
/// stopped, with its group, and counts as not found.
fn binary_on_login_path(binary: &str) -> bool {
    use std::os::unix::process::CommandExt as _;
    let Ok(mut child) = Command::new("/bin/zsh")
        .args(["-lc", &format!("command -v {binary}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                signal_group(child.id(), "-KILL");
                // silent-ok: reaping a child just killed; its status is not used.
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Where the backend runs, for "run this on …" lines: the computer's name,
/// or the host an `IRONCLAW_DESKTOP_URL` points at.
pub fn backend_host(s: &State) -> String {
    if crate::remote::backend_is_remote(s) {
        return crate::remote::computer_label(s);
    }
    s.rt.backend
        .as_ref()
        .map(|b| b.client().connection().base_url.clone())
        .map_or_else(
            || crate::remote::computer_label(s),
            |url| url_host(&url).to_string(),
        )
}

fn url_host(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
}

/// The app moved to another computer: what was being installed here is
/// stopped (its rescan would land on the other backend), and what was
/// being looked for is forgotten.
pub fn computer_switched(s: &mut State) {
    let installs = &mut s.cli_installs;
    installs.restart_until = None;
    installs.restart_dismissed.clear();
    installs
        .by_runtime
        .retain(|_, install| match install.phase {
            Phase::Running | Phase::Stopping => {
                install.stop.store(true, Ordering::SeqCst);
                install.phase = Phase::Stopping;
                true
            }
            Phase::Finding | Phase::Failed(_) | Phase::NeedsRestart(_) => false,
        });
}

/// How an installer ended.
enum Exit {
    Success,
    Failed(String),
    /// Stopped on request.
    Stopped,
    TimedOut,
}

/// The shell line run: fail when any stage of the `curl … | bash` pipe
/// fails (a download error must not read as a clean install).
fn shell_line(command: &str) -> String {
    format!("setopt pipefail; {command}")
}

fn run_installer(
    command: &'static str,
    stop: &AtomicBool,
    generation: u64,
    runtime: &'static str,
    ui: &jobs::UiQueue,
) -> Exit {
    use std::os::unix::process::CommandExt as _;
    let started = Instant::now();
    // Cancel pressed before the worker got here: nothing to start.
    if stop.load(Ordering::SeqCst) {
        return Exit::Stopped;
    }
    // Spawned and registered under the registry's lock, which quitting
    // takes too: every installer started is one `stop_all` ends, and none
    // starts once the app is quitting.
    let mut live = match live_groups().lock() {
        Ok(live) => live,
        Err(poisoned) => poisoned.into_inner(),
    };
    if live.quitting {
        return Exit::Stopped;
    }
    let spawned = Command::new("/bin/zsh")
        .args(["-lc", &shell_line(command)])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group: a stop ends the whole pipe.
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Exit::Failed(format!("Could not start the installer: {e}")),
    };
    let pgid = child.id();
    live.groups.insert(pgid);
    drop(live);
    // From here on every way out (a stop, the deadline, a clean or failed
    // exit) ends the whole group before the row is released.
    let _group = LiveGroup(pgid);
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    for stream in [
        child
            .stdout
            .take()
            .map(|o| Box::new(o) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = lines_tx.clone();
        // Detached: a descendant may hold the pipe after the installer is
        // gone; the reader ends with the pipe.
        std::thread::spawn(move || {
            for chunk in BufReader::new(stream).split(b'\n').map_while(Result::ok) {
                if let Some(line) = clean_line(&String::from_utf8_lossy(&chunk)) {
                    // silent-ok: the receiver is gone only once the worker has finished.
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        });
    }
    drop(lines_tx);
    let mut tail: VecDeque<String> = VecDeque::new();
    let mut last_shown = Instant::now() - TAIL_EVERY;
    let mut dirty = false;
    let mut pipes_open = true;
    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    // Why the worker is stopping it, and when SIGTERM went.
    let mut stopping: Option<(Exit, Instant)> = None;
    let mut killed = false;
    loop {
        if pipes_open {
            match lines_rx.recv_timeout(TAIL_EVERY) {
                Ok(line) => {
                    if tail.len() == TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                    dirty = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => pipes_open = false,
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some((status, Instant::now())),
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "installer wait failed; trying again"),
            }
        }
        if stopping.is_none() {
            let why = if stop.load(Ordering::SeqCst) {
                Some(Exit::Stopped)
            } else if started.elapsed() > INSTALL_TIMEOUT {
                Some(Exit::TimedOut)
            } else {
                None
            };
            if let Some(why) = why {
                signal_group(pgid, "-TERM");
                stopping = Some((why, Instant::now()));
            }
        }
        if let Some((_, at)) = &stopping
            && !killed
            && at.elapsed() > KILL_GRACE
        {
            // Ignored SIGTERM, or still going: the whole group goes.
            signal_group(pgid, "-KILL");
            killed = true;
        }
        if dirty && last_shown.elapsed() >= TAIL_EVERY {
            dirty = false;
            last_shown = Instant::now();
            let shown = tail.clone();
            ui.deliver(move |s, _| {
                let Some(install) = s.cli_installs.current_mut(runtime, generation) else {
                    return false;
                };
                if install.phase != Phase::Running {
                    return false;
                }
                install.tail = shown;
                true
            });
        }
        let Some((status, at)) = exited else {
            continue;
        };
        // A stop also waits for the rest of the group (the pipe's other
        // stages) to be gone, which the closed pipes show.
        if !pipes_open || (stopping.is_none() && at.elapsed() > PIPE_GRACE) {
            // Whatever of the group lives on past the installer (a stage
            // that closed its pipes, a descendant still holding them) goes
            // with it when `_group` drops, before the row is released.
            if let Some((why, _)) = stopping {
                return why;
            }
            return outcome(status, &tail);
        }
        if stopping.is_some() && killed && at.elapsed() > PIPE_GRACE {
            // Killed, and a descendant outside the group holds the pipes.
            if let Some((why, _)) = stopping {
                return why;
            }
        }
    }
}

fn outcome(status: std::process::ExitStatus, tail: &VecDeque<String>) -> Exit {
    if status.success() {
        return Exit::Success;
    }
    let last = tail.back().cloned().unwrap_or_default();
    let why = match status.code() {
        Some(code) => format!("The installer stopped with exit code {code}."),
        None => "The installer was stopped.".to_string(),
    };
    Exit::Failed(if last.is_empty() {
        why
    } else {
        format!("{why} {last}")
    })
}

/// One output line as the row shows it: the last frame of a `\r` progress
/// bar, without terminal escapes, trimmed and cut to [`LINE_CHARS`].
fn clean_line(raw: &str) -> Option<String> {
    let frame = raw.split('\r').rev().find(|f| !f.trim().is_empty())?;
    let mut out = String::new();
    let mut chars = frame.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI `ESC [ … final` or a two-character escape.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if !c.is_control() {
            out.push(c);
        }
    }
    let out = out.trim();
    if out.is_empty() {
        return None;
    }
    Some(if out.chars().count() > LINE_CHARS {
        let cut: String = out.chars().take(LINE_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        out.to_string()
    })
}

/// Installer process groups alive now, for [`stop_all`] at quit.
#[derive(Default)]
struct LiveGroups {
    groups: std::collections::HashSet<u32>,
    /// The app is quitting: no installer starts any more.
    quitting: bool,
}

fn live_groups() -> &'static std::sync::Mutex<LiveGroups> {
    static LIVE: std::sync::OnceLock<std::sync::Mutex<LiveGroups>> = std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// How long a killed group gets to be gone before the worker moves on.
const GROUP_GONE_WAIT: Duration = Duration::from_secs(2);

/// One installer's process group while its worker owns it. Dropping it
/// SIGKILLs the group and waits (bounded) until no member is left.
struct LiveGroup(u32);

impl Drop for LiveGroup {
    fn drop(&mut self) {
        signal_group(self.0, "-KILL");
        let deadline = Instant::now() + GROUP_GONE_WAIT;
        while group_alive(self.0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if group_alive(self.0) {
            tracing::debug!(pgid = self.0, "installer group outlived its SIGKILL wait");
        }
        if let Ok(mut live) = live_groups().lock() {
            live.groups.remove(&self.0);
        }
    }
}

fn group_alive(pgid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The app is quitting (Quit ZLO, or macOS ending it): no installer
/// outlives it, and none starts after. Safe to call more than once.
pub fn stop_all() {
    let mut live = match live_groups().lock() {
        Ok(live) => live,
        Err(poisoned) => poisoned.into_inner(),
    };
    live.quitting = true;
    for pgid in &live.groups {
        signal_group(*pgid, "-KILL");
    }
}

fn signal_group(pgid: u32, signal: &str) {
    // silent-ok: the group may have exited already.
    let _ = Command::new("/bin/kill")
        .args([signal, "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn finished(s: &mut State, runtime: &'static str, generation: u64, exit: Exit) {
    let Some(install) = s.cli_installs.current_mut(runtime, generation) else {
        return;
    };
    match (&install.phase, exit) {
        // A stop asked for: the process is gone, the row comes back.
        (Phase::Stopping, _) | (Phase::Running, Exit::Stopped) => {
            s.cli_installs.by_runtime.remove(runtime);
        }
        (Phase::Running, Exit::Success) => {
            install.phase = Phase::Finding;
            find(s, runtime, generation, 1);
        }
        (Phase::Running, Exit::Failed(why)) => install.phase = Phase::Failed(why),
        (Phase::Running, Exit::TimedOut) => {
            install.phase = Phase::Failed(
                "The installer ran for 10 minutes without finishing, so it was stopped. Run it in Terminal instead."
                    .into(),
            )
        }
        (Phase::Finding | Phase::Failed(_) | Phase::NeedsRestart(_), _) => {}
    }
}

/// The installer is done: ask the backend to look again. A backend that
/// answers without the agent, or has no rescan, gets a restart; one that
/// does not answer is asked again, and never restarted for it.
fn find(s: &mut State, runtime: &'static str, generation: u64, attempt: u32) {
    jobs::run_rerouted(
        s,
        "cli-install",
        |c| async move { c.rescan_providers().await },
        move |s, reply, tx| {
            if s.cli_installs.current(runtime, generation).is_none() {
                return false;
            }
            let result = match reply {
                jobs::Reply::Fresh(result) => result,
                // The same backend over a new connection: ask it again.
                jobs::Reply::Rerouted => {
                    find(s, runtime, generation, attempt);
                    return false;
                }
            };
            tx.animated();
            match result {
                Ok(catalog) => {
                    let found = known(runtime).is_some_and(|k| listed(&catalog.providers, k));
                    let flagged = catalog.restart_needed.iter().any(|r| r == runtime);
                    landed(s, catalog);
                    if found {
                        installed(s, runtime);
                    } else {
                        needs_restart(s, runtime, generation, flagged);
                    }
                }
                Err(e) if e.is_not_found() => {
                    tracing::debug!("backend has no rescan; a restart finds the agent");
                    needs_restart(s, runtime, generation, false);
                }
                Err(e) if attempt < RESCAN_TRIES => {
                    tracing::debug!(error = %e, attempt, "rescan failed; asking again");
                    jobs::after(s, RESCAN_RETRY, move |s, _| {
                        if s.cli_installs.current(runtime, generation).is_some() {
                            find(s, runtime, generation, attempt + 1);
                        }
                    });
                }
                Err(e) => fail(s, runtime, generation, unanswered(runtime, &e)),
            }
            true
        },
    );
}

fn unanswered(runtime: &str, e: &ClientError) -> String {
    let name = known(runtime).map_or(runtime, |k| k.name);
    format!(
        "{name} is installed, but IronClaw did not answer ({e}). It shows up here once IronClaw finds it."
    )
}

/// The backend answered without the agent (or cannot look again): only a
/// restart puts it to use, and a restart stops the threads running now, so
/// the row asks (Restart IronClaw) instead of restarting on its own.
fn needs_restart(s: &mut State, runtime: &'static str, generation: u64, flagged: bool) {
    let name = known(runtime).map_or(runtime, |k| k.name);
    let why = if !backend_on_this_mac(s) {
        format!("{name} is installed. Restart IronClaw on that computer so it finds it.")
    } else if flagged {
        format!("{name} is installed. Restart IronClaw to switch to it; threads running now stop.")
    } else {
        format!(
            "{name} is installed, but IronClaw does not see it yet. Restart IronClaw to look again; threads running now stop."
        )
    };
    if let Some(install) = s.cli_installs.current_mut(runtime, generation) {
        install.phase = Phase::NeedsRestart(why);
    }
}

/// The watch learned that `runtime`, installed outside the app, is put to
/// use only by a backend restart.
fn flag_restart(s: &mut State, runtime: &'static str) {
    let installs = &mut s.cli_installs;
    installs.next_generation += 1;
    let generation = installs.next_generation;
    installs.by_runtime.insert(
        runtime,
        Install {
            phase: Phase::Finding,
            tail: VecDeque::new(),
            stop: Arc::new(AtomicBool::new(false)),
            generation,
        },
    );
    needs_restart(s, runtime, generation, true);
}

/// A restart this app asked for may still be coming back.
pub fn restarting(s: &State) -> bool {
    s.cli_installs
        .restart_until
        .is_some_and(|until| Instant::now() < until)
}

/// The person pressed Restart IronClaw: restart this Mac's backend once,
/// and look for every agent waiting on it as it comes back.
pub fn restart_now(s: &mut State) {
    if restarting(s) || !backend_on_this_mac(s) {
        return;
    }
    let waiting: Vec<(&'static str, u64)> = s
        .cli_installs
        .by_runtime
        .iter()
        .filter(|(_, i)| matches!(i.phase, Phase::NeedsRestart(_)))
        .map(|(runtime, i)| (*runtime, i.generation))
        .collect();
    if !crate::service::restart() {
        for (runtime, generation) in waiting {
            let name = known(runtime).map_or(runtime, |k| k.name);
            fail(
                s,
                runtime,
                generation,
                format!(
                    "{name} is installed. This app did not start IronClaw, so restart it yourself."
                ),
            );
        }
        return;
    }
    note_restart(s);
    let deadline = Instant::now() + RESTART_WAIT;
    for (runtime, generation) in waiting {
        if let Some(install) = s.cli_installs.current_mut(runtime, generation) {
            install.phase = Phase::Finding;
        }
        poll_restarted(s, runtime, generation, deadline);
    }
}

fn poll_restarted(s: &mut State, runtime: &'static str, generation: u64, deadline: Instant) {
    jobs::after(s, RESTART_POLL, move |s, _| {
        if s.cli_installs.current(runtime, generation).is_none() {
            return;
        }
        jobs::run_rerouted(
            s,
            "cli-install",
            |c| async move { c.providers().await },
            move |s, reply, tx| {
                if s.cli_installs.current(runtime, generation).is_none() {
                    return false;
                }
                let found = match reply {
                    jobs::Reply::Fresh(Ok(catalog)) => {
                        let found = known(runtime).is_some_and(|k| listed(&catalog.providers, k));
                        landed(s, catalog);
                        found
                    }
                    // The backend is still coming back, or reconnecting.
                    jobs::Reply::Fresh(Err(_)) | jobs::Reply::Rerouted => false,
                };
                if found {
                    tx.animated();
                    restart_settled(s);
                    installed(s, runtime);
                    return true;
                }
                if Instant::now() < deadline {
                    poll_restarted(s, runtime, generation, deadline);
                    return false;
                }
                tx.animated();
                restart_settled(s);
                let k = known(runtime);
                let (name, binary) = (
                    k.map_or(runtime, |k| k.name),
                    k.map_or(runtime, |k| k.binary),
                );
                fail(
                    s,
                    runtime,
                    generation,
                    format!(
                        "{name} installed, but IronClaw cannot find `{binary}`. Open a new terminal, check `{binary} --version` runs, then restart IronClaw."
                    ),
                );
                true
            },
        );
    });
}

fn fail(s: &mut State, runtime: &str, generation: u64, why: String) {
    if let Some(install) = s.cli_installs.current_mut(runtime, generation) {
        install.phase = Phase::Failed(why);
    }
}

/// This Mac's backend was just restarted (here, or by setup's own install
/// path): Restart is not offered again while it comes back.
pub fn note_restart(s: &mut State) {
    let until = Instant::now() + RESTART_WAIT;
    s.cli_installs.restart_until = Some(until);
    s.cli_installs.restart_dismissed.clear();
    // Settled at the latest when the wait is over, with a redraw: setup
    // shows its agents as Checking until then.
    jobs::after_if(s, RESTART_WAIT, move |s, tx| {
        if s.cli_installs.restart_until != Some(until) {
            return false;
        }
        s.cli_installs.restart_until = None;
        tx.animated();
        true
    });
}

/// The restarted backend answered: the restart is over.
fn restart_settled(s: &mut State) {
    s.cli_installs.restart_until = None;
}

/// The agent is listed: its row turns into the found agent's row (sign in,
/// or ready), and both screens say so.
fn installed(s: &mut State, runtime: &'static str) {
    // Only the worker releases a row whose installer still runs: Cancel and
    // a computer switch need its stop flag until the process is gone.
    if s.cli_installs
        .get(runtime)
        .is_some_and(|i| matches!(i.phase, Phase::Running | Phase::Stopping))
    {
        return;
    }
    s.cli_installs.by_runtime.remove(runtime);
    let name = known(runtime).map_or(runtime, |k| k.name);
    let notice = format!("{name} is installed. Sign in to use it.");
    let p = &mut s.pages.providers;
    p.error = None;
    p.notice = Some(notice.clone());
    crate::pages::providers::refresh(s);
    crate::onboarding::agents_catalog_changed(s, notice);
}

/// A catalog the backend answered: the Providers page shows it at once.
fn landed(s: &mut State, catalog: ProviderCatalog) {
    s.pages.providers.runtimes = Some(catalog.providers);
}

/// While a CLI list is open with an agent not found, ask the backend every
/// [`WATCH_EVERY`] to look again, so an install made anywhere (another
/// computer, the person's own terminal) turns up on its own. `open` says
/// whether a list is still on screen. Each call starts a fresh watch (the
/// older one ends), so opening the page again always re-arms it; the watch
/// ends once the list closes, every agent is found, or the backend has no
/// rescan.
pub fn watch(s: &mut State, open: fn(&State) -> bool) {
    if s.fixture {
        return;
    }
    s.cli_installs.watch_generation += 1;
    let generation = s.cli_installs.watch_generation;
    tick(s, generation, open);
}

fn tick(s: &mut State, generation: u64, open: fn(&State) -> bool) {
    jobs::after_if(s, WATCH_EVERY, move |s, _| {
        if s.cli_installs.watch_generation != generation || !open(s) {
            return false;
        }
        // Without a catalog yet (still loading, or its read failed) the
        // rescan's answer is the catalog: it lands without notices.
        let catalog = s.pages.providers.runtimes.as_deref();
        let missing = catalog.is_none_or(|c| {
            KNOWN_CLIS
                .iter()
                .any(|k| !k.planned && k.install.is_some() && !listed(c, k))
        });
        if !missing {
            return false;
        }
        // An install from the app looks for its agent itself.
        if s.cli_installs.by_runtime.values().any(|i| i.phase.busy()) {
            tick(s, generation, open);
            return false;
        }
        let before: Option<Vec<&'static str>> = catalog.map(|c| {
            KNOWN_CLIS
                .iter()
                .filter(|k| listed(c, k))
                .map(|k| k.runtime)
                .collect()
        });
        jobs::run_rerouted(
            s,
            "cli-install",
            |c| async move { c.rescan_providers().await },
            move |s, reply, tx| {
                if s.cli_installs.watch_generation != generation {
                    return false;
                }
                match reply {
                    jobs::Reply::Fresh(Ok(catalog)) => {
                        let Some(before) = before else {
                            let first = s.pages.providers.runtimes.is_none();
                            if first {
                                // The page's own read failed: the picker
                                // comes with it, unless an edit set it since.
                                let p = &mut s.pages.providers;
                                if p.picker.is_none() {
                                    p.picker = Some(catalog.picker.clone());
                                }
                                landed(s, catalog);
                            }
                            tick(s, generation, open);
                            return first;
                        };
                        let new: Vec<&'static KnownCli> = KNOWN_CLIS
                            .iter()
                            .filter(|k| {
                                listed(&catalog.providers, k) && !before.contains(&k.runtime)
                            })
                            .collect();
                        // Installed elsewhere over a runtime only a restart
                        // replaces (Antigravity's legacy one): the row asks.
                        let flagged: Vec<&'static str> = KNOWN_CLIS
                            .iter()
                            .map(|k| k.runtime)
                            .filter(|r| {
                                catalog.restart_needed.iter().any(|n| n == r)
                                    // A failure since is older news; an
                                    // install in progress owns its row.
                                    && s.cli_installs
                                        .get(r)
                                        .is_none_or(|i| matches!(i.phase, Phase::Failed(_)))
                                    && !s.cli_installs.restart_dismissed.contains(r)
                            })
                            .collect();
                        let changed = !new.is_empty() || !flagged.is_empty();
                        if changed {
                            tx.animated();
                            landed(s, catalog);
                            for k in new {
                                installed(s, k.runtime);
                            }
                            for runtime in flagged {
                                flag_restart(s, runtime);
                            }
                        }
                        tick(s, generation, open);
                        changed
                    }
                    // An older backend without the rescan: nothing to watch with.
                    jobs::Reply::Fresh(Err(e)) if e.is_not_found() => {
                        tracing::debug!("backend has no providers rescan; CLI watch stops");
                        false
                    }
                    // A failed look or a new connection: the next tick asks again.
                    jobs::Reply::Fresh(Err(_)) | jobs::Reply::Rerouted => {
                        tick(s, generation, open);
                        false
                    }
                }
            },
        );
        false
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::harness::Pump;
    use crate::test_gateway::{Answer, Gateway};

    const RESCAN: &str = "/api/webchat/v2/providers/rescan";
    const WAIT: Duration = Duration::from_secs(10);

    fn state() -> State {
        let spec = rune::AppSpec::new("test", (100., 100.), unreachable_main, |_: &State, _| {
            rune::el()
        });
        let handle = rune::App::new(spec).handle();
        State::new(crate::jobs::Rt::new(handle))
    }

    fn unreachable_main() -> State {
        unreachable!("tests build their own state")
    }

    fn catalog(runtimes: &[&str]) -> String {
        let providers: Vec<serde_json::Value> = runtimes
            .iter()
            .map(|r| serde_json::json!({"provider": r, "runtime": r, "launch": "native"}))
            .collect();
        serde_json::json!({"providers": providers, "picker": {}, "added": 1}).to_string()
    }

    fn live(script: Vec<Answer>) -> (State, Pump, Gateway) {
        let gateway = Gateway::start(script);
        let mut s = state();
        s.rt.backend = Some(gateway.backend());
        let pump = Pump::install(&mut s);
        (s, pump, gateway)
    }

    /// The installer exits cleanly, the rescan lists the agent: its row is
    /// done (no restart), both screens say so, and the output streamed
    /// into the row while it ran.
    #[test]
    fn a_clean_install_is_found_by_a_rescan() {
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(catalog(&["codex"]))]);
        begin(
            &mut s,
            "codex",
            "printf 'Downloading\\r\\033[1m 50%%\\r100%%\\n'; sleep 0.4; echo 'codex installed'; sleep 0.4",
        );
        let mut seen = Vec::new();
        assert!(pump.until(&mut s, WAIT, |s| {
            if let Some(i) = s.cli_installs.get("codex") {
                seen.extend(i.tail.iter().cloned());
            }
            s.cli_installs.get("codex").is_none()
        }));
        assert!(
            seen.iter().any(|l| l == "100%"),
            "progress frames: {seen:?}"
        );
        assert!(seen.iter().any(|l| l == "codex installed"));
        assert_eq!(gateway.requests_to(RESCAN), 1);
        let p = &s.pages.providers;
        assert_eq!(
            p.notice.as_deref(),
            Some("Codex CLI is installed. Sign in to use it.")
        );
        assert!(
            p.runtimes
                .as_ref()
                .is_some_and(|c| c.iter().any(|r| r.runtime == "codex"))
        );
        assert_eq!(s.onboarding.agents.agent_notice, p.notice);
    }

    /// A failing installer says why, with its last line, and asks nothing.
    #[test]
    fn a_failed_install_keeps_its_reason() {
        let (mut s, mut pump, gateway) = live(vec![]);
        begin(
            &mut s,
            "codex",
            "echo 'curl: (6) Could not resolve host' >&2; exit 3",
        );
        assert!(pump.until(&mut s, WAIT, |s| {
            s.cli_installs
                .get("codex")
                .is_some_and(|i| matches!(i.phase, Phase::Failed(_)))
        }));
        let Phase::Failed(why) = &s.cli_installs.get("codex").unwrap().phase else {
            unreachable!()
        };
        assert!(why.contains("exit code 3"), "{why}");
        assert!(why.contains("Could not resolve host"), "{why}");
        assert_eq!(gateway.requests_to(RESCAN), 0);
        dismiss(&mut s, "codex");
        assert!(s.cli_installs.get("codex").is_none());
    }

    /// The installer's process group, written by the installer itself.
    fn pid_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pgid");
        (dir, path)
    }

    fn group_alive_str(pgid: &str) -> bool {
        pgid.parse().is_ok_and(group_alive)
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// Review 1: Cancel pressed before the installer even started, against
    /// an installer that ignores SIGTERM: the row stays busy (no second
    /// installer) until the whole group is gone, then comes back.
    #[test]
    fn cancel_ends_even_an_installer_that_ignores_sigterm() {
        let (mut s, mut pump, gateway) = live(vec![]);
        let (_dir, file) = pid_file();
        let command = leak(format!(
            "trap '' TERM; echo $$ > {}; echo started; sleep 30 | cat",
            file.display()
        ));
        begin(&mut s, "codex", command);
        cancel(&mut s, "codex");
        assert_eq!(s.cli_installs.get("codex").unwrap().phase, Phase::Stopping);
        // A second press while it stops starts nothing.
        let generation = s.cli_installs.get("codex").unwrap().generation;
        begin(&mut s, "codex", "echo second");
        assert_eq!(s.cli_installs.get("codex").unwrap().generation, generation);
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
        if let Ok(pgid) = std::fs::read_to_string(&file) {
            assert!(
                !group_alive_str(pgid.trim()),
                "the installer's group is gone"
            );
        }
        assert_eq!(gateway.requests_to(RESCAN), 0);
    }

    /// The same, once it runs: SIGTERM is ignored, SIGKILL follows.
    #[test]
    fn cancel_while_running_kills_the_group() {
        let (mut s, mut pump, _gateway) = live(vec![]);
        let (_dir, file) = pid_file();
        let command = leak(format!(
            "trap '' TERM; echo $$ > {}; echo started; sleep 30 | cat",
            file.display()
        ));
        begin(&mut s, "codex", command);
        assert!(pump.until(&mut s, WAIT, |s| {
            s.cli_installs
                .get("codex")
                .is_some_and(|i| !i.tail.is_empty())
        }));
        let pgid = std::fs::read_to_string(&file).unwrap();
        cancel(&mut s, "codex");
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
        assert!(!group_alive_str(pgid.trim()));
    }

    /// An installer that never ends is stopped at the deadline, and says so.
    #[test]
    fn a_hung_installer_times_out() {
        let (mut s, mut pump, _gateway) = live(vec![]);
        begin(&mut s, "codex", "trap '' TERM; sleep 30 | cat");
        assert!(pump.until(&mut s, WAIT, |s| {
            s.cli_installs.get("codex").is_some_and(
                |i| matches!(&i.phase, Phase::Failed(w) if w.contains("without finishing")),
            )
        }));
    }

    /// Review 2: switching computers stops the installer, and nothing of
    /// it reaches the other computer's backend.
    #[test]
    fn a_computer_switch_stops_the_install() {
        let (mut s, mut pump, gateway) = live(vec![]);
        begin(&mut s, "codex", "echo started; sleep 30");
        computer_switched(&mut s);
        assert_eq!(s.cli_installs.get("codex").unwrap().phase, Phase::Stopping);
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
        assert_eq!(gateway.requests_to(RESCAN), 0);
    }

    /// Review 3: a rescan that fails once is asked again; nothing restarts.
    #[test]
    fn a_transient_rescan_error_is_retried() {
        let (mut s, mut pump, gateway) =
            live(vec![Answer::fail(), Answer::ok(catalog(&["codex"]))]);
        begin(&mut s, "codex", "echo done");
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
        assert_eq!(gateway.requests_to(RESCAN), 2);
        assert!(s.cli_installs.restart_until.is_none(), "no restart");
        assert!(s.pages.providers.notice.is_some());
    }

    /// Review 4: a page whose own catalog read failed, and a failed look,
    /// do not strand the watch: its first answer is the catalog (no
    /// notice), and the next install is noticed.
    #[test]
    fn the_watch_outlives_a_missing_catalog_and_a_failed_look() {
        let (mut s, mut pump, _gateway) = live(vec![
            Answer::fail(),
            Answer::ok(catalog(&[])),
            Answer::fail(),
            Answer::ok(catalog(&["codex"])),
        ]);
        s.pages.providers.runtimes = None;
        watch(&mut s, |_| true);
        assert!(pump.until(&mut s, WAIT, |s| s.pages.providers.runtimes.is_some()));
        assert!(s.pages.providers.notice.is_none());
        assert!(pump.until(&mut s, WAIT, |s| s.pages.providers.notice.is_some()));
        assert_eq!(
            s.pages.providers.notice.as_deref(),
            Some("Codex CLI is installed. Sign in to use it.")
        );
    }

    /// Re-review 2: a watch answer listing the agent while its installer
    /// still runs leaves the row (and its Cancel) alone.
    #[test]
    fn a_watch_answer_never_releases_a_running_installer() {
        let (mut s, mut pump, _gateway) = live(vec![]);
        begin(&mut s, "codex", "echo started; sleep 30");
        installed(&mut s, "codex");
        assert_eq!(s.cli_installs.get("codex").unwrap().phase, Phase::Running);
        cancel(&mut s, "codex");
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
    }

    /// Re-review 3 P1: a backend that answers without the agent (here,
    /// Antigravity's CLI over its running legacy runtime) gets no restart
    /// on its own: the row asks, since a restart stops running threads.
    #[test]
    fn an_install_needing_a_restart_asks_for_it() {
        let answer = serde_json::json!({
            "providers": [{"provider": "antigravity", "runtime": "antigravity", "launch": "legacy"}],
            "picker": {},
            "restartNeeded": ["antigravity"],
        })
        .to_string();
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(answer)]);
        begin(&mut s, "antigravity", "echo done");
        assert!(pump.until(&mut s, WAIT, |s| {
            s.cli_installs
                .get("antigravity")
                .is_some_and(|i| matches!(i.phase, Phase::NeedsRestart(_)))
        }));
        let Phase::NeedsRestart(why) = &s.cli_installs.get("antigravity").unwrap().phase else {
            unreachable!()
        };
        assert!(why.contains("switch to it"), "{why}");
        assert!(s.cli_installs.restart_until.is_none(), "nothing restarted");
        pump.run_for(&mut s, Duration::from_millis(200));
        assert_eq!(gateway.requests_to(RESCAN), 1);
        assert!(!restarting(&s));
    }

    /// Re-review 3 P2: a clean install leaves nothing of its group behind.
    #[test]
    fn a_clean_install_leaves_no_process_behind() {
        let (mut s, mut pump, _gateway) = live(vec![Answer::ok(catalog(&["codex"]))]);
        let (_dir, file) = pid_file();
        let command = leak(format!(
            "sleep 30 > /dev/null 2>&1 & echo $! > {}; echo done",
            file.display()
        ));
        begin(&mut s, "codex", command);
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
        let pid = std::fs::read_to_string(&file).unwrap();
        let alive = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|st| st.success());
        assert!(!alive, "the installer's background child was ended");
    }

    /// Re-review 4 P1: setup's Terminal install, then Done, rescans and
    /// never restarts; a Terminal closed with nothing installed is quiet.
    #[test]
    fn a_terminal_install_is_found_by_a_rescan_and_never_restarts() {
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(catalog(&["codex"]))]);
        after_terminal_install_with(&mut s, "codex", || false);
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.get("codex").is_none()));
        assert_eq!(
            gateway.requests_to(RESCAN),
            0,
            "nothing installed, nothing asked"
        );

        after_terminal_install_with(&mut s, "codex", || true);
        assert!(pump.until(&mut s, WAIT, |s| s.pages.providers.notice.is_some()));
        assert_eq!(gateway.requests_to(RESCAN), 1);
        assert!(s.cli_installs.restart_until.is_none(), "nothing restarted");
    }

    /// Re-review 4 P2: the watch turns `restartNeeded` into a row that asks
    /// for the restart; dismissed, it does not come back.
    #[test]
    fn the_watch_asks_for_a_restart_the_backend_needs() {
        let legacy = serde_json::json!({
            "providers": [{"provider": "antigravity", "runtime": "antigravity", "launch": "legacy"}],
            "picker": {},
            "restartNeeded": ["antigravity"],
        })
        .to_string();
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(legacy)]);
        s.pages.providers.runtimes = Some(vec![]);
        watch(&mut s, |_| true);
        assert!(pump.until(&mut s, WAIT, |s| {
            s.cli_installs
                .get("antigravity")
                .is_some_and(|i| matches!(i.phase, Phase::NeedsRestart(_)))
        }));
        dismiss(&mut s, "antigravity");
        let asked = gateway.requests_to(RESCAN);
        assert!(pump.until(&mut s, WAIT, |_| gateway.requests_to(RESCAN) >= asked + 2));
        assert!(
            s.cli_installs.get("antigravity").is_none(),
            "stays dismissed"
        );
    }

    /// Re-review 5: Done on another computer's Terminal claims nothing;
    /// the watch notices that computer's install itself.
    #[test]
    fn a_remote_terminal_done_claims_nothing() {
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(catalog(&[]))]);
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-1", "studio.local");
        after_terminal_install_with(&mut s, "codex", || true);
        assert!(s.cli_installs.get("codex").is_none());
        pump.run_for(&mut s, Duration::from_millis(100));
        assert_eq!(gateway.requests_to(RESCAN), 0);
    }

    /// Re-review 5: a restart's busy state ends on its own, so setup's
    /// agents do not stay Checking.
    #[test]
    fn a_restart_window_settles_by_itself() {
        let (mut s, mut pump, _gateway) = live(vec![]);
        note_restart(&mut s);
        assert!(restarting(&s));
        assert!(pump.until(&mut s, WAIT, |s| s.cli_installs.restart_until.is_none()));
    }

    #[test]
    fn only_a_loopback_backend_is_this_macs() {
        assert_eq!(
            url_host("https://mac-mini.tail1234.ts.net:3000/x"),
            "mac-mini.tail1234.ts.net"
        );
        assert_eq!(url_host("http://[::1]:3000"), "::1");
        assert!(is_loopback("http://127.0.0.1:3000"));
        assert!(is_loopback("http://localhost:3000/"));
        assert!(is_loopback("http://[::1]:3000"));
        assert!(!is_loopback("https://mac-mini.tail1234.ts.net:3000"));
        assert!(!is_loopback("http://192.168.1.20:3000"));
    }

    /// An open CLI list notices an agent installed outside the app.
    #[test]
    fn the_watch_notices_an_install_made_elsewhere() {
        let (mut s, mut pump, gateway) = live(vec![
            Answer::ok(catalog(&[])),
            Answer::ok(catalog(&["claude-cli"])),
        ]);
        s.pages.providers.runtimes = Some(vec![]);
        watch(&mut s, |_| true);
        assert!(pump.until(&mut s, WAIT, |s| s.pages.providers.notice.is_some()));
        assert_eq!(
            s.pages.providers.notice.as_deref(),
            Some("Claude Code is installed. Sign in to use it.")
        );
        assert!(gateway.requests_to(RESCAN) >= 2);
    }

    #[test]
    fn a_progress_bar_shows_its_last_frame_without_escapes() {
        assert_eq!(
            clean_line("\u{1b}[32m 10%\r 55%\r\u{1b}[1m100% done\u{1b}[0m"),
            Some("100% done".into())
        );
        assert_eq!(clean_line("   \r  "), None);
        let long = "é".repeat(LINE_CHARS + 20);
        let cut = clean_line(&long).unwrap();
        assert_eq!(cut.chars().count(), LINE_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_failed_download_fails_the_pipe() {
        // `curl | bash` with curl failing: bash reads nothing and exits 0,
        // and only pipefail makes the line fail.
        let status = Command::new("/bin/zsh")
            .args(["-fc", &shell_line("false | cat")])
            .status()
            .unwrap();
        assert!(!status.success());
    }

    fn row(runtime: &str, launch: Option<CatalogLaunch>) -> RouteProvider {
        RouteProvider {
            provider: runtime.into(),
            runtime: runtime.into(),
            launch,
            ..Default::default()
        }
    }

    #[test]
    fn a_legacy_runtime_alone_is_not_the_installed_cli() {
        let agy = known("antigravity").unwrap();
        assert!(!listed(
            &[row("antigravity", Some(CatalogLaunch::Legacy))],
            agy
        ));
        assert!(listed(
            &[row("antigravity", Some(CatalogLaunch::Native))],
            agy
        ));
        let claude = known("claude-cli").unwrap();
        assert!(listed(&[row("claude-cli-2", None)], claude));
        assert!(!listed(&[row("codex", None)], claude));
    }
}
