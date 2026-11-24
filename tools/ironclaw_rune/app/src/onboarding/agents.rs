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
    Ready,
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
    refresh_gen: Generation,
    /// Bumped per restart wait, and when the computer changes.
    restart_gen: Generation,
    pub terminal: Option<AgentTerminal>,
    /// The gateway restarts to find a CLI installed just now.
    pub restarting: bool,
    /// The agent whose sign-in setup watches for, after its sign-in panel
    /// closed ([`watch_sign_in`]).
    pub watching: Option<Driver>,
    /// Bumped per sign-in watch, and when the computer changes.
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
    a.watch_gen.bump();
    a.watching = None;
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
    refresh_shown(s, true);
    crate::pages::providers::refresh(s);
}

/// Ask the gateway which CLIs it found (t3code `refreshProviders`), in the
/// background (the window came back to the front): the answer lands at
/// once.
pub fn refresh(s: &mut State) {
    refresh_shown(s, false);
}

/// [`refresh`]; `shown` when it answers something the person just did
/// (entered the step, closed the Terminal panel), so the answer animates.
fn refresh_shown(s: &mut State, shown: bool) {
    let generation = s.onboarding.agents.refresh_gen.bump();
    jobs::run(
        s,
        "onboarding",
        |c| async move { c.providers().await },
        move |s, result, tx| {
            let a = &mut s.onboarding.agents;
            if a.refresh_gen != generation {
                return;
            }
            if shown {
                tx.animated();
            }
            match result {
                Ok(catalog) => {
                    a.hidden = catalog.picker.hidden_providers.clone();
                    a.catalog = Some(catalog.providers);
                    a.catalog_error = None;
                }
                // Not knowing must not block setup: the cards say so.
                Err(e) => {
                    a.catalog = Some(Vec::new());
                    a.catalog_error = Some(e.to_string());
                }
            }
        },
    );
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

fn run_in_terminal(command: &str) -> Result<(), String> {
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
        refresh_shown(s, true);
        watch_sign_in(s, t.driver);
        return;
    }
    if crate::remote::backend_is_remote(s) || s.fixture {
        refresh_shown(s, true);
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
                refresh_shown(s, true);
                return;
            }
            if !crate::service::restart() {
                // A gateway this app did not start (`zlo serve`).
                refresh_shown(s, true);
                s.onboarding.agents.agent_notice = Some(format!(
                    "{} is installed. Restart IronClaw so it finds it.",
                    driver.name()
                ));
                return;
            }
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
const SIGN_IN_POLL: std::time::Duration = std::time::Duration::from_secs(2);
const SIGN_IN_WATCH: std::time::Duration = std::time::Duration::from_secs(90);

/// After `driver`'s sign-in panel closed: look at the agents again every
/// [`SIGN_IN_POLL`], quietly (no one pressed anything just then), until
/// it is Ready, the step or the computer changes, or [`SIGN_IN_WATCH`]
/// is over.
fn watch_sign_in(s: &mut State, driver: Driver) {
    let a = &mut s.onboarding.agents;
    a.watching = Some(driver);
    let generation = a.watch_gen.bump();
    poll_sign_in(
        s,
        driver,
        generation,
        std::time::Instant::now() + SIGN_IN_WATCH,
    );
}

/// Whether the sign-in watch `generation` still has a reason to look.
fn keeps_watching(s: &State, driver: Driver, generation: Generation, before: bool) -> bool {
    let a = &s.onboarding.agents;
    before
        && a.watch_gen == generation
        && a.watching == Some(driver)
        && super::wizard_shown(s)
        && s.onboarding.step == super::Step::Agents
        && !a.restarting
        && matches!(
            agent_state(a.catalog.as_deref(), &a.hidden, driver),
            AgentState::SignIn | AgentState::Checking
        )
}

fn poll_sign_in(
    s: &mut State,
    driver: Driver,
    generation: Generation,
    deadline: std::time::Instant,
) {
    jobs::after(s, SIGN_IN_POLL, move |s, _| {
        let before = std::time::Instant::now() < deadline;
        if !keeps_watching(s, driver, generation, before) {
            if s.onboarding.agents.watch_gen == generation {
                s.onboarding.agents.watching = None;
            }
            return;
        }
        let asked = s.onboarding.agents.refresh_gen.bump();
        jobs::run(
            s,
            "onboarding",
            |c| async move { c.providers().await },
            move |s, result, tx| {
                let a = &mut s.onboarding.agents;
                if a.watch_gen != generation {
                    return;
                }
                if a.refresh_gen == asked {
                    match result {
                        Ok(catalog) => {
                            let ready = agent_state(Some(&catalog.providers), &a.hidden, driver)
                                == AgentState::Ready;
                            a.hidden = catalog.picker.hidden_providers.clone();
                            a.catalog = Some(catalog.providers);
                            a.catalog_error = None;
                            if ready {
                                // The sign-in the person just made lands.
                                tx.animated();
                            }
                        }
                        // The gateway went away: stop asking it.
                        Err(e) => {
                            a.catalog = Some(Vec::new());
                            a.catalog_error = Some(e.to_string());
                        }
                    }
                }
                poll_sign_in(s, driver, generation, deadline);
            },
        );
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
    match result {
        Ok(catalog) => {
            a.hidden = catalog.picker.hidden_providers.clone();
            a.catalog = Some(catalog.providers);
            a.catalog_error = None;
        }
        Err(e) => {
            a.catalog = Some(Vec::new());
            a.catalog_error = Some(e.to_string());
        }
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
    };
    let terminal_open = a.terminal.is_some();
    // Each state's trailing part is its own keyed element, so a change
    // (Checking… → Ready) fades one out as the next comes in.
    let action = |install: bool| {
        let command = if install {
            driver.install()
        } else {
            driver.login()
        };
        let label = if install { "Install" } else { "Sign in" };
        match command {
            Some(_) => row().key("action").transition(motion::fade()).child(
                button_sized(label, Look::Outline, Size::Small)
                    .icon(&crate::icons::SQUARE_TERMINAL)
                    .disabled(terminal_open)
                    .on_press(move |s: &mut State| open_terminal(s, driver, install))
                    .build(k)
                    .key(format!("agent-action-{}", driver.runtime()))
                    .press_scale(motion::PRESS),
            ),
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
        let catalog = vec![route("antigravity", "antigravity", false)];
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

    /// Review P2: closing a sign-in panel watches for the sign-in instead
    /// of looking twice at fixed times: it keeps looking while the agent
    /// still asks to sign in, and stops once it is Ready, setup moved on,
    /// the computer changed, or the window is over.
    #[test]
    fn closing_a_sign_in_watches_until_ready_or_setup_moves_on() {
        let mut s = state();
        super::super::fixtures::agents_long(&mut s);
        let driver = Driver::Antigravity;
        assert!(
            matches!(&s.onboarding.agents.terminal, Some(t) if t.driver == driver && !t.install)
        );
        close_terminal(&mut s);
        assert_eq!(s.onboarding.agents.watching, Some(driver));
        let watch = s.onboarding.agents.watch_gen;
        let signed_out = vec![route("antigravity", "antigravity", false)];
        s.onboarding.agents.catalog = Some(signed_out.clone());
        assert!(keeps_watching(&s, driver, watch, true));
        assert!(
            !keeps_watching(&s, driver, watch, false),
            "the window is over"
        );

        s.onboarding.agents.catalog = Some(vec![route("antigravity", "antigravity", true)]);
        assert!(!keeps_watching(&s, driver, watch, true), "Ready: done");

        s.onboarding.agents.catalog = Some(signed_out.clone());
        s.onboarding.step = super::super::Step::Projects;
        assert!(!keeps_watching(&s, driver, watch, true), "setup moved on");
        s.onboarding.step = super::super::Step::Agents;
        assert!(keeps_watching(&s, driver, watch, true));

        // The gateway stopped answering: the catalog reads empty.
        s.onboarding.agents.catalog = Some(Vec::new());
        assert!(
            !keeps_watching(&s, driver, watch, true),
            "the connection went away"
        );
        s.onboarding.agents.catalog = Some(signed_out);

        reset(&mut s);
        assert!(!keeps_watching(&s, driver, watch, true), "another computer");
        assert_eq!(s.onboarding.agents.watching, None);
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
