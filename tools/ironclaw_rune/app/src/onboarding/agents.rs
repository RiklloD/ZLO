//! Setup's "Connect your agents" (t3code `AgentsStep`): the computer's
//! coding-agent CLIs (Codex, Claude Code, Antigravity; Muse Code once the
//! computer has it), each Ready, Checking, Disabled,
//! or a button to Install or Sign in. t3code types the command into its own
//! terminal for the person to review and run; ZLO opens Terminal with the
//! command shown and run only once Enter is pressed. A CLI installed after
//! the gateway started is found once the gateway restarts, so closing an
//! install restarts this Mac's gateway.
//!
//! Instead of a CLI, IronClaw's own agent can run on a provider: the
//! ChatGPT plan (device sign-in, as Settings → Providers) or an API key.
//! The first provider connected becomes the active one (`agents/providers.rs`).

mod providers;

use rune::{El, col, row, text};
use rune_kit::Kit;
use zlo_client::orchestration::RouteProvider;

use crate::design::brands::provider_logo;
use crate::design::motion;
use crate::jobs::{self, Generation};
use crate::pages::common::controls::{Look, Size, button_sized, muted, spinner_line};
use crate::pages::common::ts::xs;
use crate::pages::common::{Tone, tag};
use crate::pages::providers::catalog::{KnownCli, known_cli};
use crate::state::State;

use super::view::{StepView, command_block, list_row, section, step_shell};

/// How long a restarted gateway gets to answer again.
const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// Fast enough to see the gap between the old gateway and the new one (the
/// supervisor waits at least a second before starting it again).
const RESTART_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// The coding-agent CLIs setup offers: t3code's `PRIMARY_AGENT_DRIVERS`
/// (Codex, Claude Code), then the others ZLO's CLI manager runs. Names,
/// commands and binaries come from the one list Settings → Providers reads
/// ([`KNOWN_CLIS`](crate::pages::providers::catalog::KNOWN_CLIS)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    Codex,
    ClaudeCode,
    Antigravity,
    /// Shown only once the computer has it: it has no public installer.
    Muse,
}

pub const DRIVERS: [Driver; 4] = [
    Driver::Codex,
    Driver::ClaudeCode,
    Driver::Antigravity,
    Driver::Muse,
];

impl Driver {
    /// The runtime id prefix the gateway registers (`claude-cli-2`).
    pub fn runtime(self) -> &'static str {
        match self {
            Driver::Codex => "codex",
            Driver::ClaudeCode => "claude-cli",
            Driver::Antigravity => "antigravity",
            Driver::Muse => "muse",
        }
    }

    fn known(self) -> Option<&'static KnownCli> {
        known_cli(self.runtime())
    }

    pub fn name(self) -> &'static str {
        match self {
            // t3code's short name; Settings says "Codex CLI".
            Driver::Codex => "Codex",
            _ => self.known().map_or(self.runtime(), |k| k.name),
        }
    }

    /// The installer, when there is a public one.
    pub fn install(self) -> Option<&'static str> {
        self.known().and_then(|k| k.install)
    }

    /// The sign-in command (t3code `resolveOnboardingProviderLoginCommand`).
    pub fn login(self) -> Option<&'static str> {
        self.known().and_then(|k| k.login)
    }

    /// The program its installer puts on the login shell's PATH.
    fn binary(self) -> &'static str {
        self.known().map_or(self.runtime(), |k| k.binary)
    }

    /// Listed even when the computer does not have it: it can be installed
    /// from here.
    fn offered(self) -> bool {
        self.install().is_some()
    }
}

/// t3code `getOnboardingProviderState` (no "attention": ZLO's catalog
/// reports only installed and signed in).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Checking,
    Disabled,
    Install,
    /// Found only through a runtime with no sign-in command of its own
    /// (Antigravity's legacy ACP runtime): installing the CLI is the way
    /// to sign in.
    NeedsCli,
    SignIn,
    /// Signed out, and the gateway did not say which program it found (an
    /// older gateway): it may be the legacy runtime with no sign-in, so
    /// installing the CLI is offered beside the sign-in.
    SignInOrInstall,
    Ready,
}

impl AgentState {
    /// Still waiting for the person to sign in.
    fn signed_out(self) -> bool {
        matches!(
            self,
            AgentState::SignIn | AgentState::SignInOrInstall | AgentState::Checking
        )
    }
}

pub fn agent_state(
    catalog: Option<&[RouteProvider]>,
    hidden: &[String],
    driver: Driver,
) -> AgentState {
    let Some(catalog) = catalog else {
        return AgentState::Checking;
    };
    // t3code `selectOnboardingProvidersByDriver`: the most ready account.
    let found: Vec<&RouteProvider> = catalog
        .iter()
        .filter(|r| r.runtime.starts_with(driver.runtime()))
        .collect();
    if found.is_empty() {
        return AgentState::Install;
    }
    if found.iter().all(|r| hidden.contains(&r.provider)) {
        return AgentState::Disabled;
    }
    let shown: Vec<&&RouteProvider> = found
        .iter()
        .filter(|r| !hidden.contains(&r.provider))
        .collect();
    if shown.iter().any(|r| r.authenticated) {
        AgentState::Ready
    } else if shown.iter().all(|r| r.needs_native_cli()) && driver.install().is_some() {
        AgentState::NeedsCli
    } else if driver.known().is_some_and(|k| k.legacy_runtime)
        && driver.install().is_some()
        && shown
            .iter()
            .all(|r| r.launch_unknown() || r.needs_native_cli())
    {
        AgentState::SignInOrInstall
    } else {
        AgentState::SignIn
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalState {
    Opening,
    Ready,
    Failed(String),
    /// The agent belongs to another computer: its command is to run there,
    /// not in this Mac's Terminal.
    Remote(String),
}

pub struct AgentTerminal {
    pub driver: Driver,
    pub command: &'static str,
    pub install: bool,
    pub state: TerminalState,
}

#[derive(Default)]
pub struct AgentsState {
    /// `GET /providers`: the CLI runtimes and whether they are signed in;
    /// `None` while checking.
    pub catalog: Option<Vec<RouteProvider>>,
    pub hidden: Vec<String>,
    pub catalog_error: Option<String>,
    /// Bumped when the computer changes: a catalog request of the old one
    /// lands nowhere.
    refresh_gen: Generation,
    /// A catalog request is out: at most one is ([`request_catalog`]).
    in_flight: bool,
    /// Asked again while one was out: one more request once it lands
    /// (`true` when the answer is to animate).
    queued: Option<bool>,
    /// Bumped per restart wait, and when the computer changes.
    restart_gen: Generation,
    pub terminal: Option<AgentTerminal>,
    /// The gateway restarts to find a CLI installed just now.
    pub restarting: bool,
    /// The agents whose sign-in setup watches for, after their sign-in
    /// panels closed ([`watch_sign_in`]); one watch covers them all.
    pub watching: Vec<Driver>,
    /// When the watch gives up; `None` while no watch runs.
    watch_until: Option<std::time::Instant>,
    /// Bumped per watch, when setup leaves the step, and when the
    /// computer changes: an older watch's timer stops.
    watch_gen: Generation,
    /// What happened to an install, under the agents.
    pub agent_notice: Option<String>,
    /// "Connect a provider instead" is open.
    pub providers_open: bool,
    pub saving_key: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

/// The computer changed: its agents and providers are another set.
pub fn reset(s: &mut State) {
    let a = &mut s.onboarding.agents;
    a.refresh_gen.bump();
    a.restart_gen.bump();
    a.in_flight = false;
    a.queued = None;
    stop_watch(a);
    a.catalog = None;
    a.hidden.clear();
    a.catalog_error = None;
    a.terminal = None;
    a.restarting = false;
    a.agent_notice = None;
    a.saving_key = false;
    a.notice = None;
    a.error = None;
    let p = &mut s.pages.providers;
    p.llm = None;
    p.codex = None;
    p.key_editor = None;
    p.key_input.clear();
}

/// The step opened: the agents' states land as the reply to the
/// person's step, so they settle in with motion.
pub fn entered(s: &mut State) {
    request_catalog(s, true);
    crate::pages::providers::refresh(s);
}

/// Setup left the step: its sign-in watch ends, so coming back starts
/// afresh rather than reviving it.
pub fn left(s: &mut State) {
    stop_watch(&mut s.onboarding.agents);
}

fn stop_watch(a: &mut AgentsState) {
    a.watch_gen.bump();
    a.watching.clear();
    a.watch_until = None;
}

/// Ask the gateway which CLIs it found (t3code `refreshProviders`), in the
/// background (the window came back to the front): the answer lands at
/// once.
pub fn refresh(s: &mut State) {
    request_catalog(s, false);
}

/// Ask the gateway which CLIs it found; `shown` when it answers something
/// the person just did (entered the step, closed the Terminal panel), so
/// the answer animates. One request at a time: asked while one is out, it
/// goes once that one lands (several asks fold into one). A restart owns
/// the requests until the new gateway answers ([`wait_for_gateway`]).
pub(super) fn request_catalog(s: &mut State, shown: bool) {
    let a = &mut s.onboarding.agents;
    if s.fixture {
        return;
    }
    if a.in_flight || a.restarting {
        a.queued = Some(a.queued.unwrap_or(false) || shown);
        return;
    }
    a.in_flight = true;
    let generation = a.refresh_gen;
    jobs::run_rerouted(
        s,
        "onboarding",
        |c| async move { c.providers().await },
        move |s, reply, tx| {
            let a = &mut s.onboarding.agents;
            if a.refresh_gen != generation {
                return false;
            }
            a.in_flight = false;
            let mut again = a.queued.take();
            match reply {
                jobs::Reply::Fresh(result) => {
                    let signed_in = land_catalog(a, result);
                    // The answer to the person's step, or the sign-in they
                    // just made landing; the polls land quietly.
                    if shown || signed_in {
                        tx.animated();
                    }
                    settle_watch(a);
                }
                // The same computer, over another of its routes now: ask it
                // again there.
                jobs::Reply::Rerouted => again = Some(again.unwrap_or(false) || shown),
            }
            if let Some(shown) = again {
                request_catalog(s, shown);
            }
            true
        },
    );
}

/// One catalog answer into the step. A failed one keeps the agents last
/// seen (a gateway that blinks must not read as one with no agents), and
/// says it could not check. Returns whether a watched agent is Ready now.
fn land_catalog(
    a: &mut AgentsState,
    result: Result<zlo_client::orchestration::ProviderCatalog, zlo_client::ClientError>,
) -> bool {
    match result {
        Ok(catalog) => {
            let hidden = catalog.picker.hidden_providers;
            let signed_in = a
                .watching
                .iter()
                .any(|d| agent_state(Some(&catalog.providers), &hidden, *d) == AgentState::Ready);
            a.hidden = hidden;
            a.catalog = Some(catalog.providers);
            a.catalog_error = None;
            signed_in
        }
        // Not knowing must not block setup: the cards say so.
        Err(e) => {
            if a.catalog.is_none() {
                a.catalog = Some(Vec::new());
            }
            a.catalog_error = Some(e.to_string());
            false
        }
    }
}

/// Every watched agent the catalog no longer shows waiting for a sign-in
/// (Ready, mostly) is settled: the watch stops looking for it.
fn settle_watch(a: &mut AgentsState) {
    let (catalog, hidden) = (a.catalog.as_deref(), &a.hidden);
    a.watching
        .retain(|d| agent_state(catalog, hidden, *d).signed_out());
}

/// Open Terminal with `command` shown; it runs once Enter is pressed.
pub(super) fn open_terminal(s: &mut State, driver: Driver, install: bool) {
    let command = if install {
        driver.install()
    } else {
        driver.login()
    };
    let Some(command) = command else {
        return;
    };
    // An agent of another computer is set up there: show the command for
    // that computer, never run it on this Mac.
    if crate::remote::backend_is_remote(s) {
        let host = crate::remote::computer_label(s);
        s.onboarding.agents.terminal = Some(AgentTerminal {
            driver,
            command,
            install,
            state: TerminalState::Remote(host),
        });
        return;
    }
    s.onboarding.agents.terminal = Some(AgentTerminal {
        driver,
        command,
        install,
        state: TerminalState::Opening,
    });
    if s.fixture {
        return;
    }
    jobs::blocking(
        s,
        move || run_in_terminal(command),
        move |s, result, tx| {
            let Some(t) = s.onboarding.agents.terminal.as_mut() else {
                return;
            };
            if t.command != command {
                return;
            }
            // The reply to the person's Install / Sign in.
            tx.animated();
            t.state = match result {
                Ok(()) => TerminalState::Ready,
                Err(e) => {
                    tracing::warn!(error = %e, "Terminal did not open");
                    TerminalState::Failed(e)
                }
            };
        },
    );
}

/// The shell line Terminal runs: show the command, wait for Enter, run it.
pub fn terminal_script(command: &str) -> String {
    let shown = command.replace('\'', "'\\''");
    format!(
        "clear; printf '\\n  %s\\n\\n' '{shown}'; printf 'Press Enter to run it, or close this window to cancel. '; read _ && {command}"
    )
}

/// AppleScript string literal of `s`.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn run_in_terminal(command: &str) -> Result<(), String> {
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script {}\nend tell",
        applescript_string(&terminal_script(command))
    );
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("Could not open Terminal: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err("Could not open Terminal. Copy the command and run it yourself.".into())
    }
}

/// t3code closes its terminal and refreshes the providers. An install is
/// found once the gateway restarts, so this Mac's gateway restarts first.
fn close_terminal(s: &mut State) {
    let Some(t) = s.onboarding.agents.terminal.take() else {
        return;
    };
    // Only this Mac's own CLI and gateway are looked at or restarted here.
    if !t.install {
        watch_sign_in(s, t.driver);
        request_catalog(s, true);
        return;
    }
    if crate::remote::backend_is_remote(s) || s.fixture {
        request_catalog(s, true);
        return;
    }
    // Only a CLI that is really there now is worth a restart: closing a
    // cancelled install must not cut the gateway's streams for nothing.
    let driver = t.driver;
    jobs::blocking(
        s,
        move || cli_installed(driver),
        move |s, installed, tx| {
            // The reply to the person's Done.
            tx.animated();
            if !installed {
                request_catalog(s, true);
                return;
            }
            if !crate::service::restart() {
                // A gateway this app did not start (`zlo serve`).
                request_catalog(s, true);
                s.onboarding.agents.agent_notice = Some(format!(
                    "{} is installed. Restart IronClaw so it finds it.",
                    driver.name()
                ));
                return;
            }
            crate::cli_install::note_restart(s);
            let a = &mut s.onboarding.agents;
            a.restarting = true;
            a.catalog = None;
            let generation = a.restart_gen.bump();
            wait_for_gateway(
                s,
                generation,
                std::time::Instant::now() + RESTART_WAIT,
                false,
            );
        },
    );
}

/// How often setup looks again while it waits for a sign-in, and for how
/// long. However late the gateway learns of a sign-in (some agents keep it
/// where it can only check now and then), a look lands within a poll of
/// it, as long as it learns within the window; the window also covers a
/// sign-in finished a while after the panel closed.
#[cfg(not(test))]
const SIGN_IN_POLL: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const SIGN_IN_POLL: std::time::Duration = std::time::Duration::from_millis(40);
#[cfg(not(test))]
const SIGN_IN_WATCH: std::time::Duration = std::time::Duration::from_secs(90);
#[cfg(test)]
const SIGN_IN_WATCH: std::time::Duration = std::time::Duration::from_secs(3);

/// After `driver`'s sign-in panel closed: look at the agents again every
/// [`SIGN_IN_POLL`], quietly (no one pressed anything just then), until
/// every watched agent is settled, setup leaves the step or the computer
/// changes, or [`SIGN_IN_WATCH`] since the last panel closed is over. A
/// second panel closing joins the watch already running.
fn watch_sign_in(s: &mut State, driver: Driver) {
    let a = &mut s.onboarding.agents;
    if !a.watching.contains(&driver) {
        a.watching.push(driver);
    }
    let until = std::time::Instant::now() + SIGN_IN_WATCH;
    let running = a.watch_until.is_some();
    a.watch_until = Some(a.watch_until.map_or(until, |u| u.max(until)));
    if !running {
        let generation = a.watch_gen.bump();
        poll_sign_in(s, generation);
    }
}

/// Whether the watch still has a reason to look.
fn keeps_watching(s: &State) -> bool {
    let a = &s.onboarding.agents;
    a.watch_until
        .is_some_and(|until| std::time::Instant::now() < until)
        && !a.watching.is_empty()
        && super::wizard_shown(s)
        && s.onboarding.step == super::Step::Agents
}

fn poll_sign_in(s: &mut State, generation: Generation) {
    jobs::after_if(s, SIGN_IN_POLL, move |s, _| {
        let a = &mut s.onboarding.agents;
        if a.watch_gen != generation {
            return false;
        }
        if !keeps_watching(s) {
            stop_watch(&mut s.onboarding.agents);
            return false;
        }
        // A restart holds the requests: look again after it.
        request_catalog(s, false);
        poll_sign_in(s, generation);
        false
    });
}

/// The CLI's binary is on the login shell's PATH (where its installer
/// puts it).
fn cli_installed(driver: Driver) -> bool {
    let binary = driver.binary();
    std::process::Command::new("/bin/zsh")
        .args(["-lc", &format!("command -v {binary}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Wait for the restarted gateway: the old one keeps answering (with its
/// old catalog) while it shuts down, so an answer counts only once one
/// request has failed, or once the wait is over.
fn wait_for_gateway(
    s: &mut State,
    generation: Generation,
    deadline: std::time::Instant,
    seen_down: bool,
) {
    jobs::after(s, RESTART_POLL, move |s, _| {
        if s.onboarding.agents.restart_gen != generation {
            return;
        }
        jobs::run(
            s,
            "onboarding",
            |c| async move { c.providers().await },
            move |s, result, tx| {
                if s.onboarding.agents.restart_gen != generation {
                    return;
                }
                let before = std::time::Instant::now() < deadline;
                match (restart_step(result.is_ok(), seen_down, before), result) {
                    (RestartStep::Wait { seen_down }, _) => {
                        wait_for_gateway(s, generation, deadline, seen_down)
                    }
                    (RestartStep::Done, result) => {
                        // The polls land quietly; the end of the restart
                        // the person asked for is shown.
                        tx.animated();
                        restarted(s, result)
                    }
                }
            },
        );
    });
}

#[derive(Debug, PartialEq, Eq)]
enum RestartStep {
    Wait { seen_down: bool },
    Done,
}

/// One answer while waiting for a restart: an answer before the gateway
/// was seen down is the old one still shutting down.
fn restart_step(answered: bool, seen_down: bool, before_deadline: bool) -> RestartStep {
    match (answered, seen_down, before_deadline) {
        (_, _, false) => RestartStep::Done,
        (true, true, true) => RestartStep::Done,
        (true, false, true) => RestartStep::Wait { seen_down: false },
        (false, _, true) => RestartStep::Wait { seen_down: true },
    }
}

fn restarted(
    s: &mut State,
    result: Result<zlo_client::orchestration::ProviderCatalog, zlo_client::ClientError>,
) {
    let a = &mut s.onboarding.agents;
    a.restarting = false;
    land_catalog(a, result);
    settle_watch(a);
    // Asked while the restart held the requests.
    if let Some(shown) = a.queued.take() {
        request_catalog(s, shown);
    }
}

/// The agents listed: those with an installer, and any other the computer
/// already has.
fn listed(catalog: Option<&[RouteProvider]>) -> Vec<Driver> {
    DRIVERS
        .into_iter()
        .filter(|d| {
            d.offered()
                || catalog.is_some_and(|c| c.iter().any(|r| r.runtime.starts_with(d.runtime())))
        })
        .collect()
}

pub fn view(s: &State, k: &Kit) -> StepView {
    let p = &s.palette;
    let a = &s.onboarding.agents;
    let computer = s.onboarding.computer.clone();
    let rows = listed(a.catalog.as_deref())
        .into_iter()
        .map(|driver| agent_row(s, k, driver))
        .collect();
    let mut body = step_shell(
        s,
        "Connect your agents",
        "Choose an agent to start coding. You can add more later.",
    )
    .child(section(s, "agents-computer", Some(&computer.label()), rows));
    if let Some(t) = &a.terminal {
        body = body.child(terminal_panel(s, k, t));
    }
    if a.restarting {
        body = body.child(
            spinner_line(p, k, "Restarting IronClaw so it finds the new agent…")
                .key("agents-restarting")
                .pt(10.)
                .transition(motion::reveal()),
        );
    } else if let Some(e) = &a.catalog_error {
        body = body.child(
            muted(p, format!("Could not check the agents: {e}"))
                .key("agents-catalog-error")
                .pt(10.)
                .transition(motion::reveal()),
        );
    }
    if let Some(n) = &a.agent_notice {
        body = body.child(
            muted(p, n.clone())
                .key("agents-notice")
                .pt(10.)
                .transition(motion::reveal()),
        );
    }
    StepView::new(body.child(providers::section(s, k))).action(
        button_sized("Continue", Look::Primary, Size::Small)
            .icon(&crate::icons::ARROW_RIGHT)
            .on_press(|s: &mut State| super::go_to(s, super::Step::Projects))
            .build(k)
            .key("agents-continue")
            .press_scale(motion::PRESS),
    )
}

fn agent_row(s: &State, k: &Kit, driver: Driver) -> El {
    let p = &s.palette;
    let a = &s.onboarding.agents;
    let state = if a.restarting {
        AgentState::Checking
    } else {
        agent_state(a.catalog.as_deref(), &a.hidden, driver)
    };
    let headline = match state {
        AgentState::Ready => "Ready to code.",
        AgentState::Checking => "Checking…",
        AgentState::Disabled => "Removed from ZLO. Add it back in Settings → Providers.",
        AgentState::Install => "Not installed on this computer.",
        AgentState::NeedsCli => {
            "Found an older runtime with no sign-in. Install the CLI to sign in."
        }
        AgentState::SignIn => "Installed. Sign in to use your plan.",
        AgentState::SignInOrInstall => {
            "Installed. Sign in to use your plan; if it has no sign-in, install the CLI."
        }
    };
    let terminal_open = a.terminal.is_some();
    // Each state's trailing part is its own keyed element, so a change
    // (Checking… → Ready) fades one out as the next comes in.
    let button = |label: &'static str, install: bool, key: String| {
        button_sized(label, Look::Outline, Size::Small)
            .icon(&crate::icons::SQUARE_TERMINAL)
            .disabled(terminal_open)
            .on_press(move |s: &mut State| press(s, driver, install))
            .build(k)
            .key(key)
            .press_scale(motion::PRESS)
    };
    let action = |install: bool| {
        let command = if install {
            driver.install()
        } else {
            driver.login()
        };
        let label = if install { "Install" } else { "Sign in" };
        match command {
            Some(_) => row().key("action").transition(motion::fade()).child(button(
                label,
                install,
                format!("agent-action-{}", driver.runtime()),
            )),
            // No command to run: Settings → Providers says how.
            None => row().key("action").transition(motion::fade()).child(tag(
                p,
                if install {
                    "Not installed"
                } else {
                    "Not signed in"
                },
                Tone::Warning,
            )),
        }
    };
    let trailing = match state {
        AgentState::Ready => row()
            .key("ready")
            .gap(6.)
            .center_y()
            .transition(motion::fade())
            .child(
                rune::icon(&crate::icons::CHECK, 14.)
                    .key("ready-check")
                    .color(p.success)
                    .transition(motion::glyph_pop()),
            )
            .child(text("Ready").font(xs()).color(p.success)),
        AgentState::Checking => row().key("checking").transition(motion::fade()).child(
            text("Checking…").font(xs()).color(p.text_muted).cycle(
                crate::design::kit::loops::breathe(1.6, 0., &crate::design::kit::loops::BREATH_55),
            ),
        ),
        AgentState::Disabled => row().key("disabled").transition(motion::fade()).child(tag(
            p,
            "Removed",
            Tone::Secondary,
        )),
        AgentState::Install | AgentState::NeedsCli => action(true),
        AgentState::SignIn => action(false),
        // Both ways, each its own button: the sign-in first, the CLI as
        // the way out when there is no sign-in to run.
        AgentState::SignInOrInstall => row()
            .key("sign-in-or-install")
            .gap(6.)
            .center_y()
            .transition(motion::fade())
            .child(button(
                "Install CLI",
                true,
                format!("agent-install-{}", driver.runtime()),
            ))
            .child(button(
                "Sign in",
                false,
                format!("agent-action-{}", driver.runtime()),
            )),
    };
    let (headline, trailing) = match install_shown(s, k, driver) {
        Some((line, trailing)) => (line, trailing),
        None => (headline.to_string(), trailing),
    };
    list_row(
        s,
        format!("agent-{}", driver.runtime()),
        provider_logo(s, &[driver.runtime()], 20.),
        driver.name(),
        Some(text(headline).font(xs()).color(p.text_muted).wrap()),
        trailing,
    )
}

/// Install runs the installer here when this Mac has the backend
/// (`cli_install`); another computer's install, and every sign-in, opens the
/// command in Terminal.
fn press(s: &mut State, driver: Driver, install: bool) {
    if install
        && driver
            .known()
            .is_some_and(|k| crate::cli_install::can_install_here(s, k))
    {
        crate::cli_install::start(s, driver.runtime());
    } else {
        open_terminal(s, driver, install);
    }
}

/// An install from the app in progress or failed: the row's line and its
/// trailing part while it lasts.
fn install_shown(s: &State, k: &Kit, driver: Driver) -> Option<(String, El)> {
    use crate::cli_install::Phase;
    let p = &s.palette;
    let runtime = driver.runtime();
    let install = s.cli_installs.get(runtime)?;
    let breathing = |label: &'static str| {
        text(label)
            .font(xs())
            .color(p.text_muted)
            .cycle(crate::design::kit::loops::breathe(
                1.6,
                0.,
                &crate::design::kit::loops::BREATH_55,
            ))
    };
    let small = |label: &'static str, look: Look| button_sized(label, look, Size::Small);
    Some(match &install.phase {
        Phase::Running => (
            install
                .tail
                .back()
                .cloned()
                .unwrap_or_else(|| "Starting the installer…".into()),
            row()
                .key("installing")
                .gap(8.)
                .center_y()
                .transition(motion::fade())
                .child(breathing("Installing…"))
                .child(
                    small("Cancel", Look::Ghost)
                        .on_press(move |s: &mut State| crate::cli_install::cancel(s, runtime))
                        .build(k)
                        .key(format!("agent-install-cancel-{runtime}"))
                        .press_scale(motion::PRESS),
                ),
        ),
        Phase::Stopping => (
            "Stopping the installer…".into(),
            row()
                .key("stopping")
                .transition(motion::fade())
                .child(breathing("Stopping…")),
        ),
        Phase::Finding => (
            "Installed. Finding it…".into(),
            row()
                .key("finding")
                .transition(motion::fade())
                .child(breathing("Finding it…")),
        ),
        Phase::NeedsRestart(why) => {
            let restarting = crate::cli_install::restarting(s);
            let mut restart = small(
                if restarting {
                    "Restarting…"
                } else {
                    "Restart IronClaw"
                },
                Look::Outline,
            )
            .disabled(restarting || !crate::cli_install::backend_on_this_mac(s));
            if !restarting {
                restart = restart.on_press(|s: &mut State| crate::cli_install::restart_now(s));
            }
            (
                why.clone(),
                row()
                    .key("install-restart")
                    .transition(motion::fade())
                    .child(
                        restart
                            .build(k)
                            .key(format!("agent-install-restart-{runtime}"))
                            .press_scale(motion::PRESS),
                    ),
            )
        }
        Phase::Failed(why) => (
            why.clone(),
            row()
                .key("install-failed")
                .gap(6.)
                .center_y()
                .transition(motion::fade())
                .child(
                    small("Retry", Look::Outline)
                        .on_press(move |s: &mut State| crate::cli_install::start(s, runtime))
                        .build(k)
                        .key(format!("agent-install-retry-{runtime}"))
                        .press_scale(motion::PRESS),
                )
                .child(
                    small("Open in Terminal", Look::Ghost)
                        .icon(&crate::icons::SQUARE_TERMINAL)
                        .on_press(move |s: &mut State| {
                            crate::cli_install::dismiss(s, runtime);
                            open_terminal(s, driver, true);
                        })
                        .build(k)
                        .key(format!("agent-install-terminal-{runtime}"))
                        .press_scale(motion::PRESS),
                ),
        ),
    })
}

fn terminal_panel(s: &State, k: &Kit, t: &AgentTerminal) -> El {
    let p = &s.palette;
    let opening = t.state == TerminalState::Opening;
    let caption = match &t.state {
        TerminalState::Opening => "Opening Terminal…".to_string(),
        TerminalState::Ready => {
            "Review the command in Terminal, then press Enter to run it. Close this when it is done."
                .to_string()
        }
        TerminalState::Failed(e) => e.clone(),
        TerminalState::Remote(host) if t.install => format!(
            "Run this in a terminal on {host}, then restart IronClaw there so it finds the agent. Close this when it is done."
        ),
        TerminalState::Remote(host) => format!(
            "Run this in a terminal on {host}. Close this when it is done."
        ),
    };
    let mut actions = row().gap(4.).center_y();
    if matches!(t.state, TerminalState::Failed(_)) {
        let (driver, install) = (t.driver, t.install);
        actions = actions.child(
            button_sized("Retry", Look::Ghost, Size::XSmall)
                .on_press(move |s: &mut State| open_terminal(s, driver, install))
                .build(k)
                .key("agent-terminal-retry")
                .press_scale(motion::PRESS),
        );
    }
    actions = actions.child(
        button_sized(
            if t.install { "Done" } else { "Close" },
            Look::Ghost,
            Size::XSmall,
        )
        .on_press(close_terminal)
        .build(k)
        .key("agent-terminal-close")
        .press_scale(motion::PRESS),
    );
    let mut caption = text(caption)
        .key("caption")
        .font(xs())
        .color(p.text_muted)
        .wrap()
        .grow(1.)
        .min_w(0.);
    if opening {
        // Waiting on Terminal: the loaders' 15 fps breath, still under
        // Reduce Motion, gone once Terminal answers.
        caption = caption.cycle(crate::design::kit::loops::breathe(
            1.6,
            0.,
            &crate::design::kit::loops::BREATH_55,
        ));
    }
    section(
        s,
        "agent-terminal",
        None,
        vec![
            col()
                .key("agent-terminal-body")
                .w_full()
                .px(16.)
                .py(12.)
                .gap(10.)
                .child(
                    row()
                        .w_full()
                        .gap(8.)
                        .center_y()
                        .child(caption)
                        .child(actions),
                )
                .child(command_block(s, k, t.command, "agent-command")),
        ],
    )
    .pt(8.)
    .transition(motion::reveal())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(provider: &str, runtime: &str, authenticated: bool) -> RouteProvider {
        RouteProvider {
            provider: provider.into(),
            runtime: runtime.into(),
            authenticated,
            ..RouteProvider::default()
        }
    }

    #[test]
    fn agent_states_follow_the_catalog() {
        let none: &[RouteProvider] = &[];
        assert_eq!(agent_state(None, &[], Driver::Codex), AgentState::Checking);
        assert_eq!(
            agent_state(Some(none), &[], Driver::Codex),
            AgentState::Install
        );
        let catalog = vec![
            route("claude-cli", "claude-cli", false),
            route("claude-cli-2", "claude-cli-2", true),
            route("codex", "codex", false),
        ];
        // The most ready account wins (t3code `selectOnboardingProvidersByDriver`).
        assert_eq!(
            agent_state(Some(&catalog), &[], Driver::ClaudeCode),
            AgentState::Ready
        );
        assert_eq!(
            agent_state(Some(&catalog), &[], Driver::Codex),
            AgentState::SignIn
        );
        assert_eq!(
            agent_state(Some(&catalog), &["codex".into()], Driver::Codex),
            AgentState::Disabled
        );
        // A hidden signed-in account does not make the agent ready.
        assert_eq!(
            agent_state(Some(&catalog), &["claude-cli-2".into()], Driver::ClaudeCode),
            AgentState::SignIn
        );
    }

    #[test]
    fn antigravity_is_offered_and_muse_only_once_found() {
        let none: &[RouteProvider] = &[];
        assert_eq!(
            listed(Some(none)),
            vec![Driver::Codex, Driver::ClaudeCode, Driver::Antigravity],
            "every agent with an installer, before the gateway answers too"
        );
        assert_eq!(listed(None), listed(Some(none)));
        let found = vec![route("muse", "muse", true)];
        assert_eq!(listed(Some(&found)).last(), Some(&Driver::Muse));
        // Antigravity's states come from the gateway's catalog, as the
        // others' do.
        let catalog = vec![RouteProvider {
            launch: Some(zlo_client::orchestration::CatalogLaunch::Native),
            ..route("antigravity", "antigravity", false)
        }];
        assert_eq!(
            agent_state(Some(&catalog), &[], Driver::Antigravity),
            AgentState::SignIn
        );
        assert_eq!(
            agent_state(Some(none), &[], Driver::Antigravity),
            AgentState::Install
        );
        // Its commands are the ones Settings → Providers shows.
        assert_eq!(
            Driver::Antigravity.install(),
            Some("curl -fsSL https://antigravity.google/cli/install.sh | bash")
        );
        assert_eq!(Driver::Antigravity.login(), Some("agy acp --login"));
        assert_eq!(Driver::Antigravity.binary(), "agy");
        assert_eq!(Driver::Muse.install(), None);
    }

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

    // The sign-in watch, driven for real: its timers fire on the app's
    // timer queue and its requests go over HTTP to a scripted loopback
    // gateway; the test runs the completions as the UI thread would.

    use crate::jobs::harness::Pump;
    use crate::test_gateway::{Answer, Gateway};
    use std::time::Duration;

    /// The catalog's path (`Client::providers`).
    const CATALOG: &str = "/api/webchat/v2/providers";

    /// Long enough for any of these to happen, short of a hang.
    const WAIT: Duration = Duration::from_secs(5);

    fn catalog(rows: &[(&str, bool)]) -> String {
        let providers: Vec<serde_json::Value> = rows
            .iter()
            .map(|(runtime, authenticated)| {
                serde_json::json!({
                    "provider": runtime, "runtime": runtime,
                    "authenticated": authenticated, "launch": "native",
                })
            })
            .collect();
        serde_json::json!({ "providers": providers, "picker": {} }).to_string()
    }

    /// Setup open on the Agents step of a live gateway answering `script`.
    fn live(script: Vec<Answer>) -> (State, Pump, Gateway) {
        let gateway = Gateway::start(script);
        let mut s = state();
        s.onboarding.decision = super::super::Decision::Wizard;
        s.onboarding.step = super::super::Step::Agents;
        s.rt.backend = Some(gateway.backend());
        let pump = Pump::install(&mut s);
        assert!(super::super::wizard_shown(&s));
        (s, pump, gateway)
    }

    /// The person closes `driver`'s sign-in panel.
    fn close_sign_in(s: &mut State, driver: Driver) {
        s.onboarding.agents.terminal = Some(AgentTerminal {
            driver,
            command: driver.login().unwrap(),
            install: false,
            state: TerminalState::Ready,
        });
        close_terminal(s);
    }

    fn state_of(s: &State, driver: Driver) -> AgentState {
        let a = &s.onboarding.agents;
        agent_state(a.catalog.as_deref(), &a.hidden, driver)
    }

    /// Review P2: a second sign-in panel closing joins the watch instead
    /// of cancelling the first; each answer settles every agent it shows
    /// Ready, and the watch ends once all are.
    #[test]
    fn one_watch_covers_every_closed_sign_in_until_each_is_ready() {
        let (agy, claude) = (Driver::Antigravity, Driver::ClaudeCode);
        let signed_out = catalog(&[("antigravity", false), ("claude-cli", false)]);
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(signed_out)]);
        close_sign_in(&mut s, agy);
        assert!(pump.until(&mut s, WAIT, |s| state_of(s, agy) == AgentState::SignIn));
        close_sign_in(&mut s, claude);
        assert_eq!(s.onboarding.agents.watching, vec![agy, claude]);

        // Claude Code signs in first: it settles, Antigravity is still
        // watched, and the polls go on.
        gateway.script(vec![Answer::ok(catalog(&[
            ("antigravity", false),
            ("claude-cli", true),
        ]))]);
        assert!(pump.until(&mut s, WAIT, |s| state_of(s, claude) == AgentState::Ready));
        assert_eq!(s.onboarding.agents.watching, vec![agy]);
        let asked = gateway.requests_to(CATALOG);
        assert!(
            pump.until(&mut s, WAIT, |_| gateway.requests_to(CATALOG) >= asked + 2),
            "the first panel's watch still looks"
        );

        // Then Antigravity: everything is settled and the watch ends.
        gateway.script(vec![Answer::ok(catalog(&[
            ("antigravity", true),
            ("claude-cli", true),
        ]))]);
        assert!(pump.until(&mut s, WAIT, |s| {
            s.onboarding.agents.watch_until.is_none()
        }));
        assert_eq!(state_of(&s, agy), AgentState::Ready);
        assert!(s.onboarding.agents.watching.is_empty());
        let asked = gateway.requests_to(CATALOG);
        pump.run_for(&mut s, SIGN_IN_POLL * 5);
        assert_eq!(gateway.requests_to(CATALOG), asked, "no watch, no polls");
    }

    /// Review P2: one failed answer keeps the agents last seen and the
    /// watch going: the next answer brings the sign-in.
    #[test]
    fn a_transient_catalog_error_keeps_the_agents_and_the_watch() {
        let agy = Driver::Antigravity;
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(catalog(&[("antigravity", false)]))]);
        close_sign_in(&mut s, agy);
        assert!(pump.until(&mut s, WAIT, |s| state_of(s, agy) == AgentState::SignIn));
        gateway.script(vec![
            Answer::fail(),
            Answer::ok(catalog(&[("antigravity", true)])),
        ]);
        let mut lost = false;
        let mut failed = false;
        assert!(
            pump.until(&mut s, WAIT, |s| {
                let a = &s.onboarding.agents;
                failed |= a.catalog_error.is_some();
                lost |= a.catalog.as_ref().is_none_or(|c| c.is_empty());
                state_of(s, agy) == AgentState::Ready
            }),
            "the watch outlived the failed answer"
        );
        assert!(failed, "the failed answer landed");
        assert!(!lost, "the known agents were never cleared");
        assert!(s.onboarding.agents.catalog_error.is_none());
    }

    /// Review P2: leaving the step ends the watch; coming straight back
    /// starts afresh (one look on entering), not the old watch's polls.
    #[test]
    fn leaving_the_step_ends_the_watch_and_a_quick_return_starts_fresh() {
        let agy = Driver::Antigravity;
        let (mut s, mut pump, gateway) = live(vec![Answer::ok(catalog(&[("antigravity", false)]))]);
        close_sign_in(&mut s, agy);
        assert!(pump.until(&mut s, WAIT, |s| state_of(s, agy) == AgentState::SignIn));
        super::super::go_to(&mut s, super::super::Step::Projects);
        assert!(s.onboarding.agents.watching.is_empty());
        // Let the watch's last look (and any ask folded behind it) land, so
        // the count below sees only what the return itself asks for.
        assert!(pump.until(&mut s, WAIT, |s| {
            let a = &s.onboarding.agents;
            !a.in_flight && a.queued.is_none()
        }));
        super::super::go_to(&mut s, super::super::Step::Agents);
        // The return's own looks (setup's, and the Settings providers page
        // it refreshes, same endpoint) land; after that nothing polls.
        assert!(pump.until(&mut s, WAIT, |s| {
            let a = &s.onboarding.agents;
            !a.in_flight && a.queued.is_none()
        }));
        pump.run_for(&mut s, SIGN_IN_POLL * 2);
        let asked = gateway.requests_to(CATALOG);
        pump.run_for(&mut s, SIGN_IN_POLL * 8);
        assert!(s.onboarding.agents.watching.is_empty());
        assert_eq!(
            gateway.requests_to(CATALOG),
            asked,
            "the old watch kept polling: {:?}",
            gateway.paths()
        );

        // A new computer ends a watch too.
        close_sign_in(&mut s, agy);
        assert_eq!(s.onboarding.agents.watching, vec![agy]);
        reset(&mut s);
        assert!(s.onboarding.agents.watching.is_empty());
        assert!(s.onboarding.agents.watch_until.is_none());
    }

    /// Review P2: the computer in use moving to another of its routes
    /// while a look is out asks again on the new route, instead of
    /// dropping the answer and the watch with it.
    #[test]
    fn a_route_change_mid_request_resumes_the_watch() {
        let agy = Driver::Antigravity;
        let (mut s, mut pump, _gateway) = live(vec![
            Answer::ok(catalog(&[("antigravity", false)])).after(Duration::from_millis(300)),
            Answer::ok(catalog(&[("antigravity", true)])),
        ]);
        close_sign_in(&mut s, agy);
        // `connect::adopt` on a route refresh: the same computer, a new
        // connection.
        s.remote.tag.generation = Generation::unique();
        assert!(
            pump.until(&mut s, WAIT, |s| state_of(s, agy) == AgentState::Ready),
            "the watch went on over the new route"
        );
        assert!(!s.onboarding.agents.in_flight);
    }

    /// Review P2: closing panels again and again while the gateway is slow
    /// never has more than one look out at a time.
    #[test]
    fn repeated_closes_keep_one_request_in_flight() {
        let slow = Answer::ok(catalog(&[("antigravity", false), ("claude-cli", false)]))
            .after(Duration::from_millis(120));
        let (mut s, mut pump, gateway) = live(vec![slow]);
        for driver in [Driver::Antigravity, Driver::ClaudeCode].repeat(3) {
            close_sign_in(&mut s, driver);
            pump.run_for(&mut s, Duration::from_millis(15));
        }
        s.onboarding.agents.terminal = None;
        super::super::window_focus(&mut s, true);
        pump.run_for(&mut s, Duration::from_millis(700));
        assert!(gateway.requests_to(CATALOG) >= 3, "it kept looking");
        assert_eq!(gateway.max_in_flight(), 1, "one look at a time");
    }

    /// Review P2: Antigravity found only through its legacy ACP runtime
    /// has no `agy` to sign in with: setup offers installing the CLI.
    #[test]
    fn a_legacy_only_antigravity_offers_the_cli_install() {
        use zlo_client::orchestration::CatalogLaunch;
        let legacy = |authenticated| RouteProvider {
            launch: Some(CatalogLaunch::Legacy),
            ..route("antigravity", "antigravity", authenticated)
        };
        assert_eq!(
            agent_state(Some(&[legacy(false)]), &[], Driver::Antigravity),
            AgentState::NeedsCli
        );
        assert_eq!(
            agent_state(Some(&[legacy(true)]), &[], Driver::Antigravity),
            AgentState::Ready,
            "a legacy sign-in still runs"
        );
        let native = RouteProvider {
            launch: Some(CatalogLaunch::Native),
            ..route("antigravity", "antigravity", false)
        };
        assert_eq!(
            agent_state(Some(&[native]), &[], Driver::Antigravity),
            AgentState::SignIn
        );
        // Review P2: an older gateway sends no launch kind. Signed out, it
        // may be the legacy runtime: install is offered beside sign-in.
        let unknown = route("antigravity", "antigravity", false);
        assert_eq!(
            agent_state(
                Some(std::slice::from_ref(&unknown)),
                &[],
                Driver::Antigravity
            ),
            AgentState::SignInOrInstall
        );
        assert!(
            AgentState::SignInOrInstall.signed_out(),
            "a sign-in is watched for"
        );
        assert_eq!(
            agent_state(
                Some(&[RouteProvider {
                    authenticated: true,
                    ..unknown
                }]),
                &[],
                Driver::Antigravity
            ),
            AgentState::Ready
        );
        // Claude Code has one program: its sign-in alone.
        assert_eq!(
            agent_state(
                Some(&[route("claude-cli", "claude-cli", false)]),
                &[],
                Driver::ClaudeCode
            ),
            AgentState::SignIn
        );
    }

    #[test]
    fn a_restart_counts_only_once_the_old_gateway_went_away() {
        // The old gateway still answers while it shuts down: keep waiting.
        assert_eq!(
            restart_step(true, false, true),
            RestartStep::Wait { seen_down: false }
        );
        // Down, then up again: the new gateway.
        assert_eq!(
            restart_step(false, false, true),
            RestartStep::Wait { seen_down: true }
        );
        assert_eq!(restart_step(true, true, true), RestartStep::Done);
        // Out of time either way: stop waiting.
        assert_eq!(restart_step(true, false, false), RestartStep::Done);
        assert_eq!(restart_step(false, true, false), RestartStep::Done);
    }

    #[test]
    fn terminal_shows_the_command_and_waits_for_enter() {
        assert_eq!(Driver::ClaudeCode.login(), Some("claude auth login"));
        let line = terminal_script("claude auth login");
        assert!(line.contains("'claude auth login'"));
        assert!(line.ends_with("read _ && claude auth login"));
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
