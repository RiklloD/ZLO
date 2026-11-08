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
//! The first provider connected becomes the active one.

use ironclaw_gpui_client::operator_llm::LlmProviderView;
use ironclaw_gpui_client::orchestration::RouteProvider;
use rune::{El, col, el, row, text};
use rune_kit::Kit;

use crate::design::brands::provider_logo;
use crate::jobs::{self, Generation};
use crate::pages::common::controls::{
    Look, Size, button_sized, input, muted, open_url, outlined_tag, spinner_line,
};
use crate::pages::common::ts::{sm_medium, xs};
use crate::pages::common::{Tone, card, error_line, notice_line};
use crate::pages::providers::catalog::display_name;
use crate::state::{Page, State};

use super::gateway::Computer;
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

/// The step opened.
pub fn entered(s: &mut State) {
    refresh(s);
    crate::pages::providers::refresh(s);
}

/// Ask the gateway which CLIs it found (t3code `refreshProviders`).
pub fn refresh(s: &mut State) {
    let generation = s.onboarding.agents.refresh_gen.bump();
    jobs::run(
        s,
        "onboarding",
        |c| async move { c.providers().await },
        move |s, result, _| {
            let a = &mut s.onboarding.agents;
            if a.refresh_gen != generation {
                return;
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
    if let Computer::Remote(_) = &s.onboarding.computer {
        let host = s.onboarding.computer.label();
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
        move |s, result, _| {
            let Some(t) = s.onboarding.agents.terminal.as_mut() else {
                return;
            };
            if t.command != command {
                return;
            }
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
    if !t.install || s.onboarding.computer != Computer::ThisMac || s.fixture {
        refresh(s);
        return;
    }
    // Only a CLI that is really there now is worth a restart: closing a
    // cancelled install must not cut the gateway's streams for nothing.
    let driver = t.driver;
    jobs::blocking(
        s,
        move || cli_installed(driver),
        move |s, installed, _| {
            if !installed {
                refresh(s);
                return;
            }
            if !crate::service::restart() {
                // A gateway this app did not start (`ironclaw serve`).
                refresh(s);
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
            move |s, result, _| {
                if s.onboarding.agents.restart_gen != generation {
                    return;
                }
                let before = std::time::Instant::now() < deadline;
                match (restart_step(result.is_ok(), seen_down, before), result) {
                    (RestartStep::Wait { seen_down }, _) => {
                        wait_for_gateway(s, generation, deadline, seen_down)
                    }
                    (RestartStep::Done, result) => restarted(s, result),
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
    result: Result<
        ironclaw_gpui_client::orchestration::ProviderCatalog,
        ironclaw_gpui_client::ClientError,
    >,
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

/// The providers offered here: ChatGPT plan first, then the common API
/// keys, then any other already connected. The rest live in Settings.
pub const OFFERED: [&str; 4] = ["openai_codex", "anthropic", "openai", "openrouter"];

fn offered(s: &State) -> Vec<&LlmProviderView> {
    let Some(llm) = &s.pages.providers.llm else {
        return Vec::new();
    };
    let mut out: Vec<&LlmProviderView> = OFFERED
        .iter()
        .filter_map(|id| llm.providers.iter().find(|p| p.id == *id))
        .collect();
    for p in &llm.providers {
        if (p.active || p.api_key_set) && !out.iter().any(|o| o.id == p.id) {
            out.push(p);
        }
    }
    out
}

fn open_key(s: &mut State, id: String) {
    let p = &mut s.pages.providers;
    p.key_input.clear();
    p.key_input.focused = true;
    p.key_editor = Some(id);
}

/// Save the key; the first provider connected becomes the active one so
/// IronClaw's agent can run on it at once.
fn save_key(s: &mut State, provider: LlmProviderView) {
    let input = &mut s.pages.providers.key_input;
    let key = input.text.trim().to_string();
    if key.is_empty() || s.onboarding.agents.saving_key {
        return;
    }
    // Drop the plaintext from the field at once; the request owns it now.
    input.clear();
    s.onboarding.agents.saving_key = true;
    s.onboarding.agents.error = None;
    jobs::run(
        s,
        "onboarding",
        move |c| async move {
            let llm = c.set_provider_key(&provider, &key).await?;
            if llm.active.is_none() {
                return c.set_active_llm(&provider.id, None).await;
            }
            Ok(llm)
        },
        |s, result, _| {
            s.onboarding.agents.saving_key = false;
            match result {
                Ok(llm) => {
                    let p = &mut s.pages.providers;
                    p.llm = Some(llm);
                    p.key_editor = None;
                    s.onboarding.agents.notice =
                        Some("API key saved to the encrypted secret store.".into());
                }
                Err(e) => s.onboarding.agents.error = Some(e.to_string()),
            }
        },
    );
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
        section = section.child(spinner_line(
            p,
            k,
            "Restarting IronClaw so it finds the new agent…",
        ));
    } else if let Some(e) = &a.catalog_error {
        section = section.child(muted(p, format!("Could not check the agents: {e}")));
    }
    if let Some(n) = &a.agent_notice {
        section = section.child(muted(p, n.clone()));
    }
    let body = col()
        .w_full()
        .gap(16.)
        .pt(20.)
        .child(section)
        .child(providers_section(s, k));
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
                .key("agents-continue"),
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
    let trailing = match state {
        AgentState::Ready => row()
            .gap(6.)
            .center_y()
            .child(rune::icon(&crate::icons::CHECK, 14.).color(p.success))
            .child(text("Ready").font(xs()).color(p.success)),
        AgentState::Checking => row().child(text("Checking…").font(xs()).color(p.text_muted)),
        AgentState::Disabled => row().child(text("Disabled").font(xs()).color(p.text_muted)),
        AgentState::Install | AgentState::SignIn => {
            let install = state == AgentState::Install;
            row().child(
                button_sized(
                    if install { "Install" } else { "Sign in" },
                    Look::Ghost,
                    Size::Small,
                )
                .icon(&crate::icons::SQUARE_TERMINAL)
                .disabled(terminal_open)
                .on_press(move |s: &mut State| open_terminal(s, driver, install))
                .build(k)
                .key(format!("agent-action-{}", driver.runtime())),
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
                .key("agent-terminal-retry"),
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
        .key("agent-terminal-close"),
    );
    col()
        .key("agent-terminal")
        .w_full()
        .mt(10.)
        .radius(8.)
        .border(1., p.border)
        .child(
            row()
                .w_full()
                .px(12.)
                .py(6.)
                .gap(8.)
                .center_y()
                .child(text(caption).font(xs()).color(p.text_muted).wrap().grow(1.))
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

fn providers_section(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let a = &s.onboarding.agents;
    let open = a.providers_open;
    let header = row()
        .key("providers-toggle")
        .w_full()
        .min_h(52.)
        .px(12.)
        .gap(10.)
        .center_y()
        .radius(8.)
        .border(1., p.border)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label("Connect a provider instead")
        .focusable(true)
        .on_press(|s: &mut State| {
            let a = &mut s.onboarding.agents;
            a.providers_open = !a.providers_open;
            if a.providers_open && s.pages.providers.llm.is_none() {
                crate::pages::providers::refresh(s);
            }
        })
        .child(rune::icon(&crate::icons::BOT, 16.).color(p.text_muted))
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(text("Connect a provider instead").font(sm_medium()).color(p.text))
                .child(
                    text("No CLI needed: IronClaw's own agent runs on your ChatGPT plan or an API key.")
                        .font(xs())
                        .color(p.text_muted)
                        .wrap(),
                ),
        )
        .child(
            rune::icon(
                if open {
                    &crate::icons::CHEVRON_DOWN
                } else {
                    &crate::icons::CHEVRON_RIGHT
                },
                16.,
            )
            .color(p.text_muted),
        );
    let mut c = col().key("agents-providers").w_full().gap(6.).child(header);
    if !open {
        return c;
    }
    if let Some(e) = &a.error {
        c = c.child(error_line(p, e));
    }
    if let Some(e) = &s.pages.providers.error {
        c = c.child(error_line(p, e));
    }
    if let Some(n) = &a.notice {
        c = c.child(notice_line(n, p.success));
    }
    if s.pages.providers.llm.is_none() {
        return c.child(spinner_line(p, k, "Loading providers"));
    }
    for pv in offered(s) {
        c = c.child(provider_card(s, k, pv));
    }
    c.child(
        row().w_full().justify_end().child(
            button_sized("All providers", Look::Ghost, Size::XSmall)
                .icon(&crate::icons::EXTERNAL_LINK)
                .help("Settings → Providers. Setup comes back when you return.")
                .on_press(|s: &mut State| crate::workspace::show(s, Page::Providers))
                .build(k)
                .key("agents-all-providers"),
        ),
    )
}

fn provider_card(s: &State, k: &Kit, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let codex = pv.adapter == "openai_codex";
    let status = if pv.active {
        Some(outlined_tag(p, Tone::Success, "Active"))
    } else if pv.api_key_set {
        Some(outlined_tag(p, Tone::Secondary, "Key saved"))
    } else {
        None
    };
    let mut line = row()
        .w_full()
        .gap(12.)
        .center_y()
        .child(provider_logo(
            s,
            &[pv.id.as_str(), pv.adapter.as_str()],
            20.,
        ))
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(text(display_name(&pv.id)).font(sm_medium()).color(p.text))
                .child(
                    text(pv.description.clone())
                        .font(xs())
                        .color(p.text_muted)
                        .truncate(),
                ),
        );
    if let Some(status) = status {
        line = line.child(status);
    }
    let id = pv.id.clone();
    if codex && !pv.active {
        line = line.child(
            button_sized("Sign in with ChatGPT", Look::Outline, Size::Small)
                .disabled(ps.codex.is_some())
                .on_press(move |s: &mut State| crate::pages::providers::start_codex(s, id.clone()))
                .build(k)
                .key(format!("onboarding-codex-{}", pv.id)),
        );
    } else if !codex && pv.accepts_api_key && ps.key_editor.as_deref() != Some(pv.id.as_str()) {
        line = line.child(
            button_sized(
                if pv.api_key_set {
                    "Replace key"
                } else {
                    "Add key"
                },
                Look::Ghost,
                Size::Small,
            )
            .on_press(move |s: &mut State| open_key(s, id.clone()))
            .build(k)
            .key(format!("onboarding-key-{}", pv.id)),
        );
    }
    let mut c = card(p)
        .key(format!("onboarding-provider-{}", pv.id))
        .child(line);
    if let Some(login) = ps.codex.as_ref().filter(|l| l.provider_id == pv.id) {
        let uri = login.uri.clone();
        c = c.child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(muted(
                    p,
                    if login.code.is_empty() {
                        "Finish signing in on the ChatGPT page that just opened."
                    } else {
                        "Enter this code on the ChatGPT page that just opened:"
                    },
                ))
                .child(
                    text(login.code.clone())
                        .font(rune::TextStyle::new(16., 600.).mono())
                        .color(p.text)
                        .selectable(true),
                )
                .child(rune_kit::Spinner::new().size(12.).build(k))
                .child(el().grow(1.))
                .child(
                    button_sized("Open page", Look::Ghost, Size::XSmall)
                        .on_press_tx(move |_: &mut State, tx| open_url(tx, &uri))
                        .build(k)
                        .key("onboarding-codex-open"),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::XSmall)
                        .on_press(|s: &mut State| s.pages.providers.codex = None)
                        .build(k)
                        .key("onboarding-codex-cancel"),
                ),
        );
    }
    if ps.key_editor.as_deref() == Some(pv.id.as_str()) {
        let provider = pv.clone();
        let saving = s.onboarding.agents.saving_key;
        c = c.child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&ps.key_input)
                            .secure()
                            .placeholder("Paste the API key")
                            .label("API key")
                            .on_edit(|s: &mut State| &mut s.pages.providers.key_input)
                            .build(k)
                            .key(format!("onboarding-key-input-{}", pv.id)),
                    ),
                )
                .child(
                    crate::pages::common::loading(
                        button_sized("Save", Look::Primary, Size::Small)
                            .on_press(move |s: &mut State| save_key(s, provider.clone())),
                        saving,
                    )
                    .build(k)
                    .key("onboarding-key-save"),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| s.pages.providers.key_editor = None)
                        .build(k)
                        .key("onboarding-key-cancel"),
                ),
        );
    }
    c
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
