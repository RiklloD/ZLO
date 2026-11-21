//! Setup's "Connect your agents" (t3code `AgentsStep`): the computer's
//! coding-agent CLIs, Codex and Claude Code, each Ready, Checking, Disabled,
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

use rune::{El, col, el, row, text};
use rune_kit::Kit;
use zlo_client::orchestration::RouteProvider;

use crate::design::brands::provider_logo;
use crate::design::motion;
use crate::jobs::{self, Generation};
use crate::pages::common::card;
use crate::pages::common::controls::{Look, Size, button_sized, muted, spinner_line};
use crate::pages::common::ts::{sm_medium, xs};
use crate::state::State;

use super::view::{command_block, step_shell};

/// How long a restarted gateway gets to answer again.
const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// Fast enough to see the gap between the old gateway and the new one (the
/// supervisor waits at least a second before starting it again).
const RESTART_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// t3code `PRIMARY_AGENT_DRIVERS`, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    Codex,
    ClaudeCode,
}

pub const DRIVERS: [Driver; 2] = [Driver::Codex, Driver::ClaudeCode];

impl Driver {
    /// The runtime id prefix the gateway registers (`claude-cli-2`).
    pub fn runtime(self) -> &'static str {
        match self {
            Driver::Codex => "codex",
            Driver::ClaudeCode => "claude-cli",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Driver::Codex => "Codex",
            Driver::ClaudeCode => "Claude Code",
        }
    }

    /// t3code `NATIVE_INSTALL_COMMANDS` (no Node or npm needed).
    pub fn install(self) -> &'static str {
        match self {
            Driver::Codex => "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
            Driver::ClaudeCode => "curl -fsSL https://claude.ai/install.sh | bash",
        }
    }

    /// t3code `resolveOnboardingProviderLoginCommand`.
    pub fn login(self) -> &'static str {
        match self {
            Driver::Codex => "codex login",
            Driver::ClaudeCode => "claude auth login",
        }
    }
}

/// t3code `getOnboardingProviderState` (no "attention": ZLO's catalog
/// reports only installed and signed in).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Checking,
    Disabled,
    Install,
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
    if found
        .iter()
        .any(|r| r.authenticated && !hidden.contains(&r.provider))
    {
        AgentState::Ready
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
    if !t.install || crate::remote::backend_is_remote(s) || s.fixture {
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

/// The CLI's binary is on the login shell's PATH (where its installer
/// puts it).
fn cli_installed(driver: Driver) -> bool {
    let binary = match driver {
        Driver::Codex => "codex",
        Driver::ClaudeCode => "claude",
    };
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

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let a = &s.onboarding.agents;
    let computer = s.onboarding.computer.clone();
    let mut section = col().key("agents-computer").w_full().gap(6.).child(
        text(computer.label())
            .font(sm_medium())
            .color(p.text)
            .pb(2.),
    );
    for driver in DRIVERS {
        section = section.child(agent_card(s, k, driver));
    }
    if let Some(t) = &a.terminal {
        section = section.child(terminal_panel(s, k, t));
    }
    if a.restarting {
        section = section.child(
            spinner_line(p, k, "Restarting IronClaw so it finds the new agent…")
                .key("agents-restarting")
                .transition(motion::reveal()),
        );
    } else if let Some(e) = &a.catalog_error {
        section = section.child(
            muted(p, format!("Could not check the agents: {e}"))
                .key("agents-catalog-error")
                .transition(motion::reveal()),
        );
    }
    if let Some(n) = &a.agent_notice {
        section = section.child(
            muted(p, n.clone())
                .key("agents-notice")
                .transition(motion::reveal()),
        );
    }
    let body = col()
        .w_full()
        .gap(16.)
        .pt(20.)
        .child(section)
        .child(providers::section(s, k));
    step_shell(
        s,
        "Connect your agents",
        "Choose an agent to start coding. You can add more later.",
    )
    .child(
        el().key("agents-scroll")
            .w_full()
            .max_h(440.)
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .child(body.pr(12.)),
    )
    .child(
        row().w_full().pt(24.).justify_end().child(
            button_sized("Continue", Look::Primary, Size::Small)
                .icon(&crate::icons::ARROW_RIGHT)
                .on_press(|s: &mut State| super::go_to(s, super::Step::Projects))
                .build(k)
                .key("agents-continue")
                .press_scale(motion::PRESS),
        ),
    )
}

fn agent_card(s: &State, k: &Kit, driver: Driver) -> El {
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
        AgentState::SignIn => "Installed. Sign in to use your plan.",
    };
    let terminal_open = a.terminal.is_some();
    // Each state's trailing part is its own keyed element, so a change
    // (Checking… → Ready) fades one out as the next comes in.
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
        AgentState::Disabled => row()
            .key("disabled")
            .transition(motion::fade())
            .child(text("Disabled").font(xs()).color(p.text_muted)),
        AgentState::Install | AgentState::SignIn => {
            let install = state == AgentState::Install;
            row().key("action").transition(motion::fade()).child(
                button_sized(
                    if install { "Install" } else { "Sign in" },
                    Look::Ghost,
                    Size::Small,
                )
                .icon(&crate::icons::SQUARE_TERMINAL)
                .disabled(terminal_open)
                .on_press(move |s: &mut State| open_terminal(s, driver, install))
                .build(k)
                .key(format!("agent-action-{}", driver.runtime()))
                .press_scale(motion::PRESS),
            )
        }
    };
    card(p).key(format!("agent-{}", driver.runtime())).child(
        row()
            .w_full()
            .gap(12.)
            .center_y()
            .child(provider_logo(s, &[driver.runtime()], 20.))
            .child(
                col()
                    .grow(1.)
                    .min_w(0.)
                    .child(text(driver.name()).font(sm_medium()).color(p.text))
                    .child(text(headline).font(xs()).color(p.text_muted).wrap()),
            )
            .child(trailing),
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
        .grow(1.);
    if opening {
        // Waiting on Terminal: the loaders' 15 fps breath, still under
        // Reduce Motion, gone once Terminal answers.
        caption = caption.cycle(crate::design::kit::loops::breathe(
            1.6,
            0.,
            &crate::design::kit::loops::BREATH_55,
        ));
    }
    col()
        .key("agent-terminal")
        .w_full()
        .mt(10.)
        .transition(motion::reveal())
        .radius(8.)
        .border(1., p.border)
        .child(
            row()
                .w_full()
                .px(12.)
                .py(6.)
                .gap(8.)
                .center_y()
                .child(caption)
                .child(actions),
        )
        .child(el().w_full().h(1.).fill(p.border))
        .child(
            col()
                .w_full()
                .padding(12.)
                .child(command_block(s, k, t.command, "agent-command")),
        )
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
        let line = terminal_script("claude auth login");
        assert!(line.contains("'claude auth login'"));
        assert!(line.ends_with("read _ && claude auth login"));
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
