//! First-run setup, ported from t3code's onboarding (`FirstRunGate` and
//! `WelcomeWizard`): the first time ZLO connects to a workspace with no
//! threads, a wizard walks through three steps before the app:
//!
//! 1. **Connect** — the computer to work on: this Mac's gateway, or another
//!    computer's, found on the tailnet or added with a pairing link or code
//!    (`crate::remote`). ZLO talks to one gateway at a time, so picking a
//!    computer switches to it.
//! 2. **Agents** — coding-agent CLIs (Codex, Claude Code) with Install and
//!    Sign in run in Terminal, as t3code runs them in its terminal; or,
//!    instead of a CLI, a provider (ChatGPT plan, an API key) that
//!    IronClaw's own agent runs on (`agents.rs`).
//! 3. **Projects** — folders Claude Code and Codex sessions ran in, the
//!    recent repositories picked, imported as threads (`projects.rs`); the
//!    pick limits the session import to those folders.
//!
//! The gate (t3code `resolveFirstRunDecision`): setup done before (the
//! record in `onboarding.json`) or a workspace that already has threads
//! goes straight to the app, and the second case records setup as done.
//! Setup shows only over the home screen (the chat or the new-thread
//! screen); a page opened from the menu shows as asked, so setup never locks
//! anyone out. Closing it finishes setup without importing (ZLO's usual
//! import of every folder then runs, as before setup existed).

mod agents;
pub mod fixtures;
pub mod gateway;
mod projects;
mod view;

use std::path::PathBuf;

use rune::El;
use rune_kit::Kit;
use serde::{Deserialize, Serialize};

use crate::state::{Page, State};

pub use agents::AgentsState;
pub use projects::ProjectsState;

/// t3code's `FirstRunDecision`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Decision {
    /// Not connected yet, or the inbox not read.
    #[default]
    Pending,
    App,
    Wizard,
}

/// The wizard's steps (t3code `ONBOARDING_STAGES`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    #[default]
    Connect,
    Agents,
    Projects,
}

pub const STEPS: [(Step, &str); 3] = [
    (Step::Connect, "Connect"),
    (Step::Agents, "Agents"),
    (Step::Projects, "Projects"),
];

impl Step {
    pub fn index(self) -> usize {
        match self {
            Step::Connect => 0,
            Step::Agents => 1,
            Step::Projects => 2,
        }
    }
}

#[derive(Default)]
pub struct OnboardingState {
    pub decision: Decision,
    /// The computer ZLO is connected to (the saved ones are
    /// `State::remote`).
    pub computer: gateway::Computer,
    pub step: Step,
    pub connect: ConnectState,
    pub agents: AgentsState,
    pub projects: ProjectsState,
    /// Finishing: the record is being written.
    pub finishing: bool,
    pub finish_error: Option<String>,
    /// The note shown after setup ("Imported 12 threads").
    pub toast: Option<Toast>,
    /// A scene's record is in memory only.
    pub fixture_completed: bool,
}

pub struct Toast {
    pub title: String,
    pub description: Option<String>,
    pub warning: bool,
    pub generation: crate::jobs::Generation,
}

#[derive(Default)]
pub struct ConnectState {
    /// "Add a computer" is unfolded (its form is `remote::pairing`'s).
    pub pairing_open: bool,
}

/// The saved record (t3code's `onboardingCompletedAt` client setting).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    completed_at: Option<String>,
}

fn record_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ZLO").join("onboarding.json"))
}

/// Setup was finished (or skipped) on this Mac before.
fn completed(s: &State) -> bool {
    if s.fixture {
        return s.onboarding.fixture_completed;
    }
    let Some(file) = record_file() else {
        return false;
    };
    match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice::<Record>(&bytes)
            .map(|r| r.completed_at.is_some())
            .unwrap_or_else(|e| {
                // A damaged record must not lock anyone into setup again
                // and again; it counts as done.
                tracing::warn!(error = %e, "onboarding record unreadable");
                true
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!(error = %e, "onboarding record unreadable");
            true
        }
    }
}

/// Write the record; `Err` keeps the wizard open with the error.
fn write_record() -> Result<(), String> {
    let file = record_file().ok_or("No settings folder on this Mac.")?;
    let record = Record {
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    crate::persist::atomic_write(&file, &bytes).map_err(|e| {
        tracing::warn!(error = %e, "onboarding record not saved");
        "Your settings could not be saved. Try again.".to_string()
    })
}

/// t3code `resolveFirstRunDecision` for ZLO's one gateway: done before, or
/// threads exist (persist), goes to the app; an empty workspace gets the
/// wizard.
pub fn resolve(completed: bool, thread_count: usize) -> (Decision, bool) {
    if completed {
        (Decision::App, false)
    } else if thread_count > 0 {
        (Decision::App, true)
    } else {
        (Decision::Wizard, false)
    }
}

/// The gateway answered and the inbox is read: decide once per launch.
/// Called by `workspace::connect` before polling starts.
pub fn decide(s: &mut State) {
    if s.onboarding.decision != Decision::Pending {
        return;
    }
    let (decision, persist) = resolve(completed(s), s.workspace.inbox.list.rows.len());
    s.onboarding.decision = decision;
    if persist && !s.fixture {
        jobs_write_record(s);
    }
    if decision == Decision::Wizard {
        tracing::debug!("first run: setup opens");
    }
}

fn jobs_write_record(s: &State) {
    crate::jobs::blocking(s, write_record, |_, result, _| {
        if let Err(e) = result {
            tracing::warn!(error = %e, "onboarding record not saved");
        }
    });
}

/// While setup is open, the launch-time session import waits: the
/// Projects step decides what it imports.
pub fn holds_launch_import(s: &State) -> bool {
    s.onboarding.decision == Decision::Wizard
}

/// Setup shows (t3code: the wizard owns only the home route).
pub fn wizard_shown(s: &State) -> bool {
    s.onboarding.decision == Decision::Wizard && matches!(s.page, Page::Chat | Page::Draft)
}

/// Setup switched computers: what it learned about the old one is gone.
pub fn computer_switched(s: &mut State) {
    projects::reset(s);
    agents::reset(s);
}

pub fn go_to(s: &mut State, step: Step) {
    if s.onboarding.projects.importing {
        return;
    }
    s.onboarding.step = step;
    match step {
        Step::Connect => {}
        Step::Agents => agents::entered(s),
        Step::Projects => projects::entered(s),
    }
}

/// The window came back to the front: an agent set up in Terminal may be
/// ready now.
pub fn window_focus(s: &mut State, focused: bool) {
    if focused && wizard_shown(s) && s.onboarding.step == Step::Agents {
        agents::refresh(s);
    }
}

/// How setup ends: what the Projects step brought in.
pub struct Finish {
    pub imported: u32,
    pub warning: Option<String>,
}

/// t3code `finish`: save the record, close the wizard, let the session
/// import run (the launch tick setup held back), and say what came in.
pub fn finish(s: &mut State, how: Finish) {
    if s.onboarding.finishing {
        return;
    }
    s.onboarding.finishing = true;
    s.onboarding.finish_error = None;
    if s.fixture {
        land(s, how);
        return;
    }
    crate::jobs::blocking(s, write_record, move |s, result, _| match result {
        Ok(()) => land(s, how),
        Err(e) => {
            s.onboarding.finishing = false;
            s.onboarding.finish_error = Some(e);
        }
    });
}

fn land(s: &mut State, how: Finish) {
    let o = &mut s.onboarding;
    o.finishing = false;
    o.decision = Decision::App;
    o.step = Step::Connect;
    let toast = match (&how.warning, how.imported) {
        (Some(w), _) => Some((
            "Some history was not imported".to_string(),
            Some(w.clone()),
            true,
        )),
        (None, 0) => None,
        (None, n) => Some((
            format!("Imported {n} {}", if n == 1 { "thread" } else { "threads" }),
            Some("They wait in Settled, ready to continue.".to_string()),
            false,
        )),
    };
    if let Some((title, description, warning)) = toast {
        show_toast(s, title, description, warning);
    }
    // The launch import setup held back runs now. Closed without a pick, it
    // covers every folder, as before setup existed; after a pick it keeps
    // to the picked folders (or imports nothing), and after an import it is
    // no longer due.
    crate::workspace::release_session_import(s);
    crate::workspace::refresh_inbox(s);
}

const TOAST_FOR: std::time::Duration = std::time::Duration::from_secs(8);

fn show_toast(s: &mut State, title: String, description: Option<String>, warning: bool) {
    let generation = crate::jobs::Generation::unique();
    s.onboarding.toast = Some(Toast {
        title,
        description,
        warning,
        generation,
    });
    // A warning stays until closed (t3code `timeout: 0`).
    if !warning {
        crate::jobs::after(s, TOAST_FOR, move |s, _| {
            if s.onboarding.toast.as_ref().map(|t| t.generation) == Some(generation) {
                s.onboarding.toast = None;
            }
        });
    }
}

/// The wizard over the home screen and the note after it, into `layer`.
pub fn view(s: &State, k: &Kit, layer: &mut Vec<El>) {
    if wizard_shown(s) {
        view::wizard(s, k, layer);
    }
    if let Some(toast) = &s.onboarding.toast {
        layer.push(view::toast(s, k, toast));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{App, AppSpec, el};

    fn state() -> State {
        let spec = AppSpec::new("test", (100., 100.), unreachable_main, |_: &State, _| el());
        let handle = App::new(spec).handle();
        let mut s = State::new(crate::jobs::Rt::new(handle));
        s.fixture = true;
        s
    }

    fn unreachable_main() -> State {
        unreachable!("tests build their own state")
    }

    #[test]
    fn a_failed_do_not_import_keeps_setup_open_and_the_import_held() {
        let mut s = state();
        s.onboarding.decision = Decision::Wizard;
        s.onboarding.step = Step::Projects;
        projects::skip_saved(&mut s, Err(zlo_client::ClientError::Config("down".into())));
        assert_eq!(s.onboarding.decision, Decision::Wizard);
        assert!(holds_launch_import(&s), "nothing may import yet");
        assert!(s.onboarding.projects.error.is_some());
        projects::skip_saved(&mut s, Ok(()));
        assert_eq!(s.onboarding.decision, Decision::App);
        assert!(!holds_launch_import(&s));
    }

    #[test]
    fn switching_computers_forgets_the_old_ones_projects_and_agents() {
        let mut s = state();
        fixtures::projects(&mut s);
        s.onboarding.projects.selection = Some(["/Users/me/src/rune".to_string()].into());
        fixtures::agents(&mut s);
        assert!(s.pages.providers.llm.is_some());
        computer_switched(&mut s);
        assert!(s.onboarding.projects.scan.is_none());
        assert!(s.onboarding.projects.selection.is_none());
        assert!(s.onboarding.agents.catalog.is_none());
        assert!(s.onboarding.agents.terminal.is_none());
        assert!(
            s.pages.providers.llm.is_none(),
            "the old computer's providers"
        );
    }

    #[test]
    fn another_computers_agent_is_never_set_up_on_this_mac() {
        let mut s = state();
        fixtures::agents(&mut s);
        s.onboarding.agents.terminal = None;
        s.onboarding.computer = gateway::Computer::remote("env-1", "studio.local");
        agents::open_terminal(&mut s, agents::Driver::Codex, true);
        let t = s
            .onboarding
            .agents
            .terminal
            .as_ref()
            .map(|t| t.state.clone());
        assert_eq!(
            t,
            Some(agents::TerminalState::Remote("studio.local".into()))
        );
    }

    #[test]
    fn the_gate_shows_setup_only_on_an_empty_workspace_never_done_before() {
        assert_eq!(resolve(false, 0), (Decision::Wizard, false));
        // Threads exist: the app, and setup is recorded as done.
        assert_eq!(resolve(false, 3), (Decision::App, true));
        assert_eq!(resolve(true, 0), (Decision::App, false));
        assert_eq!(resolve(true, 5), (Decision::App, false));
    }

    #[test]
    fn steps_count_up_in_order() {
        let order: Vec<usize> = STEPS.iter().map(|(s, _)| s.index()).collect();
        assert_eq!(order, vec![0, 1, 2]);
    }
}
