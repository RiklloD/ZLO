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
const RESTART_WAIT: Duration = Duration::from_secs(45);
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
    /// When the installer exited cleanly: a backend restarted before then
    /// started too early to find it.
    exited_at: Option<Instant>,
}

#[derive(Default)]
pub struct Installs {
    by_runtime: HashMap<&'static str, Install>,
    next_generation: u64,
    /// The watch's generation: a newer [`watch`] or a closed list ends the
    /// older chain.
    watch_generation: u64,
    /// The last backend restart (this module's or setup's) and until when
    /// it may still be coming back: an install that exited before it
    /// started waits for it instead of restarting again.
    restart: Option<Restart>,
}

#[derive(Clone, Copy)]
struct Restart {
    at: Instant,
    until: Instant,
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
    known.install.is_some() && !crate::remote::backend_is_remote(s)
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
            exited_at: None,
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
            exited_at: None,
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
        Phase::Finding | Phase::Failed(_) => {
            s.cli_installs.by_runtime.remove(runtime);
        }
    }
}

/// Close a failed install's message.
pub fn dismiss(s: &mut State, runtime: &str) {
    if s.cli_installs
        .get(runtime)
        .is_some_and(|i| matches!(i.phase, Phase::Failed(_)))
    {
        s.cli_installs.by_runtime.remove(runtime);
    }
}

/// The app moved to another computer: what was being installed here is
/// stopped (its rescan would land on the other backend), and what was
/// being looked for is forgotten.
pub fn computer_switched(s: &mut State) {
    let installs = &mut s.cli_installs;
    installs.restart = None;
    installs
        .by_runtime
        .retain(|_, install| match install.phase {
            Phase::Running | Phase::Stopping => {
                install.stop.store(true, Ordering::SeqCst);
                install.phase = Phase::Stopping;
                true
            }
            Phase::Finding | Phase::Failed(_) => false,
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
            // with it, on every way out: the row is released next.
            signal_group(pgid, "-KILL");
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
            install.exited_at = Some(Instant::now());
            find(s, runtime, generation, 1);
        }
        (Phase::Running, Exit::Failed(why)) => install.phase = Phase::Failed(why),
        (Phase::Running, Exit::TimedOut) => {
            install.phase = Phase::Failed(
                "The installer ran for 10 minutes without finishing, so it was stopped. Run it in Terminal instead."
                    .into(),
            )
        }
        (Phase::Finding | Phase::Failed(_), _) => {}
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
                    landed(s, catalog);
                    if found {
                        installed(s, runtime);
                    } else {
                        restart_to_find(s, runtime, generation);
                    }
                }
                Err(e) if e.is_not_found() => {
                    tracing::debug!("backend has no rescan; restarting it instead");
                    restart_to_find(s, runtime, generation);
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

/// The backend answered without the agent: restart this Mac's backend,
/// which looks for every CLI as it starts. One restart serves every
/// install waiting on it.
fn restart_to_find(s: &mut State, runtime: &'static str, generation: u64) {
    let name = known(runtime).map_or(runtime, |k| k.name);
    if crate::remote::backend_is_remote(s) {
        fail(
            s,
            runtime,
            generation,
            format!("{name} is installed. Restart IronClaw on that computer so it finds it."),
        );
        return;
    }
    let now = Instant::now();
    let exited_at = s
        .cli_installs
        .current(runtime, generation)
        .and_then(|i| i.exited_at)
        .unwrap_or(now);
    let joinable = joinable_restart(s.cli_installs.restart, exited_at, now);
    let deadline = match joinable {
        Some(r) => r.until,
        None => {
            if !crate::service::restart() {
                fail(
                    s,
                    runtime,
                    generation,
                    format!("{name} is installed. Restart IronClaw so it finds it."),
                );
                return;
            }
            note_restart(s);
            now + RESTART_WAIT
        }
    };
    poll_restarted(s, runtime, generation, deadline);
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
                    installed(s, runtime);
                    return true;
                }
                if Instant::now() < deadline {
                    poll_restarted(s, runtime, generation, deadline);
                    return false;
                }
                tx.animated();
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

/// A restart still coming back that began after the install was done finds
/// it as it starts; one that began earlier does not.
fn joinable_restart(restart: Option<Restart>, exited_at: Instant, now: Instant) -> Option<Restart> {
    restart.filter(|r| r.until > now && r.at >= exited_at)
}

/// This Mac's backend was just restarted (here, or by setup's own install
/// path): installs done before now wait for it rather than restart again.
pub fn note_restart(s: &mut State) {
    let at = Instant::now();
    s.cli_installs.restart = Some(Restart {
        at,
        until: at + RESTART_WAIT,
    });
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
                        let changed = !new.is_empty();
                        if changed {
                            tx.animated();
                            landed(s, catalog);
                            for k in new {
                                installed(s, k.runtime);
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

    fn group_alive(pgid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{pgid}")])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|st| st.success())
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
            assert!(!group_alive(pgid.trim()), "the installer's group is gone");
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
        assert!(!group_alive(pgid.trim()));
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
        assert!(s.cli_installs.restart.is_none(), "no restart");
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

    /// Re-review 4: an install done after a restart began needs its own;
    /// one done before joins it, while that restart is still coming back.
    #[test]
    fn only_a_restart_begun_after_an_install_serves_it() {
        let at = Instant::now();
        let restart = Some(Restart {
            at,
            until: at + RESTART_WAIT,
        });
        let before = at - Duration::from_secs(1);
        let after = at + Duration::from_millis(1);
        assert!(joinable_restart(restart, before, at).is_some());
        assert!(joinable_restart(restart, after, at).is_none());
        assert!(joinable_restart(restart, before, at + RESTART_WAIT).is_none());
        assert!(joinable_restart(None, before, at).is_none());
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
