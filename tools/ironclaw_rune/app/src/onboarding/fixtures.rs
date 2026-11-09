//! Fixture data for the setup scenes (`--scene onboarding-connect`,
//! `onboarding-pairing`, `onboarding-agents`, `onboarding-providers`,
//! `onboarding-projects`, `onboarding-projects-loading`, `onboarding-toast`).

use zlo_client::orchestration::RouteProvider;
use zlo_client::session_import::{SessionImportGit, SessionImportProject, SessionImportProjects};

use super::agents::{AgentTerminal, Driver, TerminalState};
use super::projects::Scan;
use super::{Decision, Step};
use crate::state::State;

fn base(s: &mut State, step: Step) {
    // Fills the provider list (and marks the state a fixture).
    crate::pages::providers::fixtures::providers(s);
    crate::workspace::fixtures::fixture_empty(s);
    s.onboarding.decision = Decision::Wizard;
    s.onboarding.step = step;
}

pub fn connect(s: &mut State) {
    base(s, Step::Connect);
    s.remote.registry.environments = vec![zlo_client::environments::SavedEnvironment {
        environment_id: "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10".into(),
        label: "Studio".into(),
        routes: vec![zlo_client::environments::Route {
            kind: zlo_client::environments::RouteKind::Tailnet,
            url: "https://studio.tail9915dd.ts.net".into(),
            remote_port: None,
        }],
        legacy_token: false,
    }];
}

pub fn pairing(s: &mut State) {
    base(s, Step::Connect);
    s.onboarding.connect.pairing_open = true;
    let page = &mut s.remote.page;
    page.tailscale = Some(false);
    page.input = rune::TextEdit::new("https://studio.tail9915dd.ts.net/pair");
    page.error = Some("The link has no code. It ends in #code=…".into());
}

fn route(provider: &str, name: &str, authenticated: bool) -> RouteProvider {
    RouteProvider {
        provider: provider.into(),
        name: name.into(),
        runtime: provider.into(),
        authenticated,
        models: vec!["default".into()],
        ..RouteProvider::default()
    }
}

pub fn agents(s: &mut State) {
    base(s, Step::Agents);
    let a = &mut s.onboarding.agents;
    a.catalog = Some(vec![route("claude-cli", "Claude Code", false)]);
    a.terminal = Some(AgentTerminal {
        driver: Driver::ClaudeCode,
        command: Driver::ClaudeCode.login(),
        install: false,
        state: TerminalState::Ready,
    });
}

pub fn providers(s: &mut State) {
    base(s, Step::Agents);
    let a = &mut s.onboarding.agents;
    a.catalog = Some(vec![route("codex", "Codex", true)]);
    a.providers_open = true;
    let p = &mut s.pages.providers;
    p.key_editor = Some("openai".into());
    p.key_input = rune::TextEdit::new("").secure();
}

fn project(
    path: &str,
    repo: Option<(&str, &str)>,
    sources: &[&str],
    threads: u32,
    hours: i64,
) -> SessionImportProject {
    let now = chrono::Utc::now().timestamp_millis();
    SessionImportProject {
        path: path.into(),
        title: super::projects::split_path(path).0.into(),
        sources: sources.iter().map(|s| s.to_string()).collect(),
        thread_count: threads,
        last_active_at: Some(now - hours * 3_600_000),
        git: repo.map(|(repository, key)| SessionImportGit {
            repository: Some(repository.into()),
            remote_key: Some(key.into()),
        }),
        imported: false,
    }
}

pub fn projects(s: &mut State) {
    base(s, Step::Projects);
    let both = &["claude_code", "codex"];
    let claude = &["claude_code"];
    s.onboarding.projects.scan = Some(Scan::Ready(SessionImportProjects {
        projects: vec![
            project(
                "/Users/me/code/ironclaw",
                Some(("nearai/ironclaw", "github.com/nearai/ironclaw")),
                both,
                48,
                2,
            ),
            project(
                "/Users/me/code/ironclaw-wt",
                Some(("nearai/ironclaw", "github.com/nearai/ironclaw")),
                claude,
                6,
                20,
            ),
            project(
                "/Users/me/src/rune",
                Some(("anomalyco/rune", "github.com/anomalyco/rune")),
                both,
                12,
                30,
            ),
            project(
                "/Users/me/code/site",
                Some(("me/site", "github.com/me/site")),
                claude,
                2,
                24 * 9,
            ),
            project("/Users/me/Downloads", None, claude, 3, 24 * 3),
            project("/Users/me/notes", None, &["codex"], 1, 24 * 40),
        ],
        truncated: false,
    }));
}

pub fn projects_loading(s: &mut State) {
    base(s, Step::Projects);
    s.onboarding.projects.scan = Some(Scan::Pending);
}

pub fn toast(s: &mut State) {
    crate::pages::providers::fixtures::providers(s);
    crate::workspace::fixtures::fixture_empty(s);
    s.onboarding.decision = Decision::App;
    s.onboarding.toast = Some(super::Toast {
        title: "Imported 66 threads".into(),
        description: Some("They wait in Settled, ready to continue.".into()),
        warning: false,
        generation: crate::jobs::Generation::unique(),
    });
}
