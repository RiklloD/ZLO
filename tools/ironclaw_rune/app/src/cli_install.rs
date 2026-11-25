//! Installing a coding-agent CLI from the app: Settings → Providers → CLI
//! agents and setup's "Connect your agents" both press Install here.
//!
//! The installer (the catalog's fixed `install` command, never anything
//! typed) runs in this Mac's login shell with no terminal attached, its last
//! lines shown under the row. Once it exits cleanly the backend is asked to
//! look again for CLIs (`POST /providers/rescan`), so the agent turns up
//! without cutting any running thread. Only when that does not find it (an
//! older backend, a runtime already registered under the agent's id) is
//! this Mac's backend restarted, as setup did before.
//!
//! An agent of another computer is installed there: the row shows the
//! command to run on it, and [`watch`] notices it once that backend finds
//! it. The same watch notices an install run in the person's own terminal.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use zlo_client::orchestration::{CatalogLaunch, ProviderCatalog, RouteProvider};

use crate::jobs;
use crate::pages::providers::catalog::{KNOWN_CLIS, KnownCli, known_cli};
use crate::state::State;

/// How long an installer may run before it is stopped.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Lines of installer output kept for the row.
const TAIL_LINES: usize = 3;
/// Longest output line shown, in characters.
const LINE_CHARS: usize = 160;
/// How often the row's output may redraw while the installer prints.
const TAIL_EVERY: Duration = Duration::from_millis(250);
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
    /// It exited cleanly; the backend is looking for the agent.
    Finding,
    /// It stopped, or the agent was not found after it.
    Failed(String),
}

pub struct Install {
    pub phase: Phase,
    /// The installer's last lines, oldest first.
    pub tail: VecDeque<String>,
    /// The installer's process group, while it runs.
    pgid: Option<u32>,
    generation: u64,
}

#[derive(Default)]
pub struct Installs {
    by_runtime: HashMap<&'static str, Install>,
    next_generation: u64,
    /// The watch's generation: bumped to stop a running one.
    watch_generation: u64,
    watching: bool,
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
    if installs
        .get(runtime)
        .is_some_and(|i| matches!(i.phase, Phase::Running | Phase::Finding))
    {
        return;
    }
    installs.next_generation += 1;
    let generation = installs.next_generation;
    installs.by_runtime.insert(
        runtime,
        Install {
            phase: Phase::Running,
            tail: VecDeque::new(),
            pgid: None,
            generation,
        },
    );
    if s.fixture {
        return;
    }
    let ui = jobs::ui_queue(s);
    std::thread::spawn(move || {
        let exit = run_installer(command, generation, runtime, &ui);
        ui.deliver(move |s, tx| {
            tx.animated();
            finished(s, runtime, generation, exit);
            true
        });
    });
    // Stop an installer that hangs (a prompt with no terminal to answer).
    jobs::after_if(s, INSTALL_TIMEOUT, move |s, tx| {
        let Some(install) = s.cli_installs.current_mut(runtime, generation) else {
            return false;
        };
        if install.phase != Phase::Running {
            return false;
        }
        if let Some(pgid) = install.pgid.take() {
            kill_group(pgid);
        }
        install.phase = Phase::Failed(
            "The installer ran for 10 minutes without finishing, so it was stopped. Run it in Terminal instead."
                .into(),
        );
        tx.animated();
        true
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
            pgid: None,
            generation,
        },
    );
}

/// Stop `runtime`'s installer and put the row back.
pub fn cancel(s: &mut State, runtime: &str) {
    if let Some(install) = s.cli_installs.by_runtime.remove(runtime)
        && let Some(pgid) = install.pgid
    {
        kill_group(pgid);
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

/// How an installer ended.
enum Exit {
    Success,
    Failed(String),
}

/// The shell line run: fail when any stage of the `curl … | bash` pipe
/// fails (a download error must not read as a clean install).
fn shell_line(command: &str) -> String {
    format!("setopt pipefail; {command}")
}

fn run_installer(
    command: &'static str,
    generation: u64,
    runtime: &'static str,
    ui: &jobs::UiQueue,
) -> Exit {
    use std::os::unix::process::CommandExt as _;
    let spawned = Command::new("/bin/zsh")
        .args(["-lc", &shell_line(command)])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group: Cancel stops the whole pipe.
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Exit::Failed(format!("Could not start the installer: {e}")),
    };
    let pgid = child.id();
    ui.deliver(move |s, _| {
        if let Some(install) = s.cli_installs.current_mut(runtime, generation) {
            install.pgid = Some(pgid);
        }
        false
    });
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    let readers: Vec<_> = [
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
    .map(|stream| {
        let tx = lines_tx.clone();
        std::thread::spawn(move || {
            for chunk in BufReader::new(stream).split(b'\n').map_while(Result::ok) {
                if let Some(line) = clean_line(&String::from_utf8_lossy(&chunk)) {
                    // silent-ok: the receiver is gone only once the installer has ended.
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        })
    })
    .collect();
    drop(lines_tx);
    let mut tail: VecDeque<String> = VecDeque::new();
    let mut last_shown = Instant::now() - TAIL_EVERY;
    let mut dirty = false;
    loop {
        match lines_rx.recv_timeout(TAIL_EVERY) {
            Ok(line) => {
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
                dirty = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
    }
    for reader in readers {
        // silent-ok: a reader only ends with its stream.
        let _ = reader.join();
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => return Exit::Failed(format!("Lost the installer: {e}")),
    };
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

fn kill_group(pgid: u32) {
    // silent-ok: the group may have exited already.
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn finished(s: &mut State, runtime: &'static str, generation: u64, exit: Exit) {
    let Some(install) = s.cli_installs.current_mut(runtime, generation) else {
        return;
    };
    if install.phase != Phase::Running {
        return;
    }
    install.pgid = None;
    match exit {
        Exit::Failed(why) => install.phase = Phase::Failed(why),
        Exit::Success => {
            install.phase = Phase::Finding;
            find(s, runtime, generation);
        }
    }
}

/// The installer is done: ask the backend to look again.
fn find(s: &mut State, runtime: &'static str, generation: u64) {
    jobs::run(
        s,
        "cli-install",
        |c| async move { c.rescan_providers().await },
        move |s, result, tx| {
            if s.cli_installs.current(runtime, generation).is_none() {
                return;
            }
            tx.animated();
            let found = match result {
                Ok(catalog) => {
                    let found = known(runtime).is_some_and(|k| listed(&catalog.providers, k));
                    landed(s, catalog);
                    found
                }
                Err(e) => {
                    tracing::debug!(error = %e, "rescan failed; restarting the backend instead");
                    false
                }
            };
            if found {
                installed(s, runtime);
            } else {
                restart_to_find(s, runtime, generation);
            }
        },
    );
}

/// The rescan did not turn the agent up: restart this Mac's backend, which
/// looks for every CLI as it starts.
fn restart_to_find(s: &mut State, runtime: &'static str, generation: u64) {
    let name = known(runtime).map_or(runtime, |k| k.name);
    if !crate::service::restart() {
        fail(
            s,
            runtime,
            generation,
            format!("{name} is installed. Restart IronClaw so it finds it."),
        );
        return;
    }
    poll_restarted(s, runtime, generation, Instant::now() + RESTART_WAIT);
}

fn poll_restarted(s: &mut State, runtime: &'static str, generation: u64, deadline: Instant) {
    jobs::after(s, RESTART_POLL, move |s, _| {
        if s.cli_installs.current(runtime, generation).is_none() {
            return;
        }
        jobs::run(
            s,
            "cli-install",
            |c| async move { c.providers().await },
            move |s, result, tx| {
                if s.cli_installs.current(runtime, generation).is_none() {
                    return;
                }
                let found = match &result {
                    Ok(catalog) => known(runtime).is_some_and(|k| listed(&catalog.providers, k)),
                    // The backend is still coming back.
                    Err(_) => false,
                };
                if let Ok(catalog) = result {
                    landed(s, catalog);
                }
                if found {
                    tx.animated();
                    installed(s, runtime);
                } else if Instant::now() < deadline {
                    poll_restarted(s, runtime, generation, deadline);
                } else {
                    tx.animated();
                    let k = known(runtime);
                    fail(
                        s,
                        runtime,
                        generation,
                        format!(
                            "{} installed, but IronClaw cannot find `{}`. Open a new terminal, check `{} --version` runs, then restart IronClaw.",
                            k.map_or(runtime, |k| k.name),
                            k.map_or(runtime, |k| k.binary),
                            k.map_or(runtime, |k| k.binary),
                        ),
                    );
                }
            },
        );
    });
}

fn fail(s: &mut State, runtime: &str, generation: u64, why: String) {
    if let Some(install) = s.cli_installs.current_mut(runtime, generation) {
        install.phase = Phase::Failed(why);
    }
}

/// The agent is listed: its row turns into the found agent's row (sign in,
/// or ready), and both screens say so.
fn installed(s: &mut State, runtime: &'static str) {
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
/// whether a list is still on screen; the watch stops once it is not.
pub fn watch(s: &mut State, open: fn(&State) -> bool) {
    let installs = &mut s.cli_installs;
    if installs.watching || s.fixture {
        return;
    }
    installs.watching = true;
    installs.watch_generation += 1;
    let generation = installs.watch_generation;
    tick(s, generation, open);
}

fn tick(s: &mut State, generation: u64, open: fn(&State) -> bool) {
    jobs::after_if(s, WATCH_EVERY, move |s, _| {
        if s.cli_installs.watch_generation != generation {
            return false;
        }
        let missing = |s: &State| {
            s.pages.providers.runtimes.as_ref().is_some_and(|c| {
                KNOWN_CLIS
                    .iter()
                    .any(|k| !k.planned && k.install.is_some() && !listed(c, k))
            })
        };
        // An install from the app looks for its agent itself.
        let busy = s
            .cli_installs
            .by_runtime
            .values()
            .any(|i| matches!(i.phase, Phase::Running | Phase::Finding));
        if !open(s) || !missing(s) {
            s.cli_installs.watching = false;
            return false;
        }
        if busy {
            tick(s, generation, open);
            return false;
        }
        let before: Vec<&'static str> = s
            .pages
            .providers
            .runtimes
            .as_deref()
            .map(|c| {
                KNOWN_CLIS
                    .iter()
                    .filter(|k| listed(c, k))
                    .map(|k| k.runtime)
                    .collect()
            })
            .unwrap_or_default();
        jobs::run(
            s,
            "cli-install",
            |c| async move { c.rescan_providers().await },
            move |s, result, tx| {
                if s.cli_installs.watch_generation != generation {
                    return;
                }
                match result {
                    Ok(catalog) => {
                        let new: Vec<&'static KnownCli> = KNOWN_CLIS
                            .iter()
                            .filter(|k| {
                                listed(&catalog.providers, k) && !before.contains(&k.runtime)
                            })
                            .collect();
                        if !new.is_empty() {
                            tx.animated();
                            landed(s, catalog);
                            for k in new {
                                installed(s, k.runtime);
                            }
                        }
                        tick(s, generation, open);
                    }
                    // An older backend without the rescan: nothing to watch with.
                    Err(e) => {
                        tracing::debug!(error = %e, "providers rescan unavailable; CLI watch stops");
                        s.cli_installs.watching = false;
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

    /// Cancel stops the whole installer and puts the row back; its late
    /// exit changes nothing.
    #[test]
    fn cancel_stops_the_installer() {
        let (mut s, mut pump, gateway) = live(vec![]);
        begin(&mut s, "codex", "echo started; sleep 30 | cat");
        assert!(pump.until(&mut s, WAIT, |s| {
            s.cli_installs
                .get("codex")
                .is_some_and(|i| i.pgid.is_some())
        }));
        let pgid = s.cli_installs.get("codex").unwrap().pgid.unwrap();
        cancel(&mut s, "codex");
        assert!(s.cli_installs.get("codex").is_none());
        let alive = || {
            Command::new("/bin/kill")
                .args(["-0", &format!("-{pgid}")])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|st| st.success())
        };
        let deadline = Instant::now() + WAIT;
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the installer's group is gone");
        pump.run_for(&mut s, Duration::from_millis(200));
        assert!(s.cli_installs.get("codex").is_none());
        assert_eq!(gateway.requests_to(RESCAN), 0);
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
