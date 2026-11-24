//! Setup's "Import your projects" (t3code `ImportStep` and
//! `projectImport.logic.ts`): the folders Claude Code and Codex sessions ran
//! in, grouped by repository, the recent ones picked (a git checkout with
//! three or more sessions in the last 30 days). "Import N projects" limits
//! the session import to the picked folders and runs it; "Do not import
//! projects" limits it to none. Settings → General → Import terminal
//! sessions shows the limit and imports every folder again
//! (`pages/settings/session_import.rs`).

use std::collections::BTreeSet;
use std::time::Duration;

use rune_kit::Kit;
use zlo_client::session_import::{
    ImportTrigger, ProjectSelection, SessionImportProject, SessionImportProjects,
    SessionImportRequest, SessionImportStatus,
};

use crate::jobs::{self, Generation};
use crate::state::State;

use super::Finish;

mod list;

const RECENT_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const DEFAULT_SELECTION_MIN_THREADS: u32 = 3;
const IMPORT_POLL: Duration = Duration::from_secs(1);
const SCAN_LIMIT_MESSAGE: &str =
    "Scan limit reached. Some projects or conversations may be missing.";

pub enum Scan {
    Pending,
    Ready(SessionImportProjects),
    Failed(String),
    /// The gateway predates the project list.
    Unsupported,
}

#[derive(Default)]
pub struct ProjectsState {
    pub scan: Option<Scan>,
    scan_gen: Generation,
    /// The person's pick; `None` keeps the recent ones (t3code's default).
    pub selection: Option<BTreeSet<String>>,
    /// Groups whose open state differs from their default.
    pub flipped: BTreeSet<String>,
    pub importing: bool,
    import_gen: Generation,
    pub error: Option<String>,
}

/// t3code `partitionOnboardingProjects`: the recent repositories.
pub fn recent(projects: &[SessionImportProject], now_ms: i64) -> BTreeSet<String> {
    let cutoff = now_ms - RECENT_WINDOW_MS;
    projects
        .iter()
        .filter(|p| {
            p.git.is_some()
                && p.thread_count >= DEFAULT_SELECTION_MIN_THREADS
                && p.last_active_at
                    .is_some_and(|at| at >= cutoff && at <= now_ms)
        })
        .map(|p| p.path.clone())
        .collect()
}

pub struct Group<'a> {
    pub key: String,
    pub label: String,
    pub candidates: Vec<&'a SessionImportProject>,
    pub thread_count: u32,
    pub last_active_at: Option<i64>,
}

/// t3code `groupOnboardingProjects`: clones of one repository together,
/// newest first; folders outside git apart, as "Other folders".
pub fn group(projects: &[SessionImportProject]) -> (Vec<Group<'_>>, Vec<&SessionImportProject>) {
    let mut groups: Vec<Group<'_>> = Vec::new();
    let mut other = Vec::new();
    for p in projects {
        let Some(git) = &p.git else {
            other.push(p);
            continue;
        };
        let key = match &git.remote_key {
            Some(remote) => format!("remote:{remote}"),
            None => format!("path:{}", p.path),
        };
        match groups.iter_mut().find(|g| g.key == key) {
            Some(g) => {
                g.candidates.push(p);
                g.thread_count += p.thread_count;
                g.last_active_at = g.last_active_at.max(p.last_active_at);
            }
            None => groups.push(Group {
                label: git.repository.clone().unwrap_or_else(|| p.title.clone()),
                key,
                candidates: vec![p],
                thread_count: p.thread_count,
                last_active_at: p.last_active_at,
            }),
        }
    }
    groups.sort_by(|a, b| {
        b.last_active_at
            .cmp(&a.last_active_at)
            .then_with(|| a.label.cmp(&b.label))
    });
    (groups, other)
}

/// t3code `splitOnboardingProjectPath`: `/work/app` → (`app`, `/work/`).
pub fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return (path, "");
    }
    match trimmed.rfind('/') {
        Some(ix) => (&trimmed[ix + 1..], &trimmed[..=ix]),
        None => (trimmed, ""),
    }
}

/// t3code `formatRelativeTime`'s short form: `now`, `5m`, `3h`, `2d`, `6w`.
pub fn short_age(at_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms - at_ms).max(0) / 1000;
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s if s < 365 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

/// The computer changed: its folders are another list, and a scan still
/// running for the old one must not land.
pub fn reset(s: &mut State) {
    let p = &mut s.onboarding.projects;
    p.scan_gen.bump();
    p.import_gen.bump();
    p.scan = None;
    p.selection = None;
    p.flipped.clear();
    p.importing = false;
    p.error = None;
}

/// The step opened: list the folders (once; Retry lists again).
pub fn entered(s: &mut State) {
    if matches!(s.onboarding.projects.scan, Some(Scan::Ready(_))) {
        return;
    }
    scan(s);
}

fn scan(s: &mut State) {
    let generation = s.onboarding.projects.scan_gen.bump();
    s.onboarding.projects.scan = Some(Scan::Pending);
    jobs::run(
        s,
        "onboarding",
        |c| async move { c.session_import_projects().await },
        move |s, result, tx| {
            let p = &mut s.onboarding.projects;
            if p.scan_gen != generation {
                return;
            }
            // The reply to the person opening the step (or Retry): the
            // list takes over from the spinner with motion.
            tx.animated();
            p.scan = Some(match result {
                Ok(list) => Scan::Ready(list),
                Err(e) if e.is_not_found() => Scan::Unsupported,
                Err(e) => Scan::Failed(e.to_string()),
            });
        },
    );
}

fn listed(s: &State) -> &[SessionImportProject] {
    match &s.onboarding.projects.scan {
        Some(Scan::Ready(list)) => &list.projects,
        _ => &[],
    }
}

/// The picked folders: the person's pick, else the recent ones.
pub fn selected(s: &State) -> BTreeSet<String> {
    let projects = listed(s);
    match &s.onboarding.projects.selection {
        Some(pick) => projects
            .iter()
            .filter(|p| pick.contains(&p.path))
            .map(|p| p.path.clone())
            .collect(),
        None => recent(projects, chrono::Utc::now().timestamp_millis()),
    }
}

fn set_keys(s: &mut State, keys: Vec<String>, on: bool) {
    let mut pick = selected(s);
    for key in keys {
        if on {
            pick.insert(key);
        } else {
            pick.remove(&key);
        }
    }
    s.onboarding.projects.selection = Some(pick);
}

/// "Do not import projects": nothing new comes in, now or later.
fn skip(s: &mut State) {
    if s.onboarding.projects.importing {
        return;
    }
    let none = ProjectSelection::Only(BTreeSet::new());
    if matches!(s.onboarding.projects.scan, Some(Scan::Unsupported)) {
        // An older gateway cannot limit the import; leave it as it was.
        super::finish(s, finish_with(0, None));
        return;
    }
    let request = SessionImportRequest {
        projects: Some(none.clone()),
        ..SessionImportRequest::default()
    };
    jobs::run(
        s,
        "onboarding",
        move |c| async move { c.session_import(&request).await },
        |s, result, tx| {
            tx.animated();
            skip_saved(s, result.map(|_| ()))
        },
    );
}

/// "Do not import" landed. Finishing after a failed save would let the
/// held import bring in every folder, the opposite of what was asked: the
/// wizard stays, says so, and the button tries again.
pub(super) fn skip_saved(s: &mut State, result: Result<(), zlo_client::ClientError>) {
    match result {
        Ok(()) => super::finish(s, finish_with(0, None)),
        Err(e) => {
            tracing::warn!(error = %e, "import limit not saved");
            s.onboarding.projects.error =
                Some(format!("Could not save your choice: {e}. Try again."));
        }
    }
}

fn finish_with(imported: u32, warning: Option<String>) -> Finish {
    Finish { imported, warning }
}

/// t3code `runImport`: limit the import to the pick, run it, wait for it.
fn run_import(s: &mut State) {
    let pick = selected(s);
    let p = &mut s.onboarding.projects;
    if p.importing || pick.is_empty() {
        return;
    }
    p.importing = true;
    p.error = None;
    let generation = p.import_gen.bump();
    let request = SessionImportRequest {
        projects: Some(ProjectSelection::Only(pick)),
        run: Some(ImportTrigger::Manual),
        ..SessionImportRequest::default()
    };
    send_import(s, generation, request, 1);
}

/// Whose run the import step is waiting on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Waiting {
    /// The run this step started: its end is the import's end.
    Ours,
    /// Another run was going on (Settings' "Import now", a rescan) and read
    /// the settings before the pick: once it ends, ask again.
    Other { retries: u8 },
}

fn send_import(s: &mut State, generation: Generation, request: SessionImportRequest, retries: u8) {
    let again = request.clone();
    jobs::run(
        s,
        "onboarding",
        move |c| async move { c.session_import(&request).await },
        move |s, result, tx| {
            let waiting = match &result {
                Ok(status) if status.started => Waiting::Ours,
                _ => Waiting::Other { retries },
            };
            landed(s, generation, result, waiting, again, tx);
        },
    );
}

/// The next step for an answer while importing.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    Poll,
    Ask,
    Finish,
    GiveUp,
}

pub(super) fn next(running: bool, waiting: Waiting) -> Next {
    match (running, waiting) {
        (true, _) => Next::Poll,
        (false, Waiting::Ours) => Next::Finish,
        (false, Waiting::Other { retries }) if retries > 0 => Next::Ask,
        (false, Waiting::Other { .. }) => Next::GiveUp,
    }
}

fn landed(
    s: &mut State,
    generation: Generation,
    result: Result<SessionImportStatus, zlo_client::ClientError>,
    waiting: Waiting,
    request: SessionImportRequest,
    tx: &mut rune::Tx,
) {
    if s.onboarding.projects.import_gen != generation {
        return;
    }
    let status = match result {
        Ok(status) => status,
        Err(e) => {
            // The end of the person's Import: shown, not cut in.
            tx.animated();
            let p = &mut s.onboarding.projects;
            p.importing = false;
            p.error = Some(format!("Could not import: {e}"));
            return;
        }
    };
    match next(status.running, waiting) {
        Next::Poll => {
            jobs::after(s, IMPORT_POLL, move |s, _| {
                if s.onboarding.projects.import_gen != generation {
                    return;
                }
                jobs::run(
                    s,
                    "onboarding",
                    |c| async move { c.session_import_status().await },
                    move |s, result, tx| landed(s, generation, result, waiting, request, tx),
                );
            });
        }
        Next::Ask => {
            let retries = match waiting {
                Waiting::Other { retries } => retries - 1,
                Waiting::Ours => 0,
            };
            send_import(s, generation, request, retries);
        }
        Next::Finish => {
            tx.animated();
            s.onboarding.projects.importing = false;
            let (imported, warning) = outcome(&status);
            super::finish(s, finish_with(imported, warning));
        }
        Next::GiveUp => {
            tx.animated();
            let p = &mut s.onboarding.projects;
            p.importing = false;
            p.error = Some("Another import was running. Try again.".into());
        }
    }
}

/// What a finished run brought in, and t3code's warning when some of it
/// did not come in.
pub fn outcome(status: &SessionImportStatus) -> (u32, Option<String>) {
    let mut imported = 0;
    let mut failed = 0;
    let mut deferred = 0;
    for c in status.sources.iter().filter_map(|s| s.counts) {
        imported += c.imported;
        failed += c.failed;
        deferred += c.deferred;
    }
    let threads = |n: u32| if n == 1 { "thread" } else { "threads" };
    let warning = if let Some(e) = &status.last_error {
        Some(e.clone())
    } else if failed > 0 && imported > 0 {
        Some(format!(
            "Imported {imported} {}. {failed} {} could not be imported.",
            threads(imported),
            threads(failed)
        ))
    } else if failed > 0 {
        Some(format!(
            "{failed} {} could not be imported.",
            threads(failed)
        ))
    } else if deferred > 0 {
        Some(format!(
            "Imported {imported} {}. The rest comes in on the next rescan.",
            threads(imported)
        ))
    } else {
        None
    };
    (imported, warning)
}

/// The step's view (`projects/list.rs`).
pub fn view(s: &State, k: &Kit) -> super::view::StepView {
    list::view(s, k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlo_client::session_import::{SessionImportCounts, SessionImportGit};

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn project(
        path: &str,
        remote: Option<&str>,
        threads: u32,
        last: Option<i64>,
    ) -> SessionImportProject {
        SessionImportProject {
            path: path.into(),
            title: split_path(path).0.into(),
            sources: vec!["claude_code".into()],
            thread_count: threads,
            last_active_at: last,
            git: remote.map(|r| SessionImportGit {
                repository: r.rsplit_once('/').map(|_| {
                    let mut parts = r.rsplit('/');
                    let name = parts.next().unwrap_or_default();
                    let owner = parts.next().unwrap_or_default();
                    format!("{owner}/{name}")
                }),
                remote_key: (!r.is_empty()).then(|| r.to_string()),
            }),
            imported: false,
        }
    }

    #[test]
    fn recent_repositories_are_picked_by_default() {
        let now = 100 * DAY;
        let list = vec![
            project("/w/app", Some("github.com/acme/app"), 5, Some(now - DAY)),
            // Too few sessions.
            project("/w/lib", Some("github.com/acme/lib"), 2, Some(now - DAY)),
            // Too old.
            project(
                "/w/old",
                Some("github.com/acme/old"),
                9,
                Some(now - 40 * DAY),
            ),
            // Not a checkout.
            project("/w/notes", None, 9, Some(now - DAY)),
            // From the future (a skewed clock).
            project("/w/next", Some("github.com/acme/next"), 9, Some(now + DAY)),
        ];
        assert_eq!(recent(&list, now), ["/w/app".to_string()].into());
    }

    #[test]
    fn clones_of_one_repository_group_and_plain_folders_stand_apart() {
        let list = vec![
            project("/w/app", Some("github.com/acme/app"), 2, Some(10)),
            project("/w/app-2", Some("github.com/acme/app"), 3, Some(30)),
            project("/w/lib", Some("github.com/acme/lib"), 1, Some(20)),
            project("/w/scratch", Some(""), 1, Some(5)),
            project("/tmp/x", None, 1, Some(50)),
        ];
        let (groups, other) = group(&list);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["acme/app", "acme/lib", "scratch"]);
        assert_eq!(groups[0].candidates.len(), 2);
        assert_eq!(groups[0].thread_count, 5);
        assert_eq!(groups[0].last_active_at, Some(30));
        assert_eq!(groups[2].key, "path:/w/scratch");
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn paths_split_and_ages_shorten() {
        assert_eq!(split_path("/work/app/"), ("app", "/work/"));
        assert_eq!(split_path("app"), ("app", ""));
        assert_eq!(split_path("/"), ("/", ""));
        assert_eq!(short_age(0, 30_000), "now");
        assert_eq!(short_age(0, 3 * 3_600_000), "3h");
        assert_eq!(short_age(0, 2 * DAY), "2d");
        assert_eq!(short_age(0, 14 * DAY), "2w");
    }

    #[test]
    fn an_import_that_joined_another_run_asks_again_once_it_ends() {
        let other = Waiting::Other { retries: 1 };
        assert_eq!(next(true, other), Next::Poll);
        assert_eq!(next(false, other), Next::Ask);
        assert_eq!(next(false, Waiting::Other { retries: 0 }), Next::GiveUp);
        assert_eq!(next(true, Waiting::Ours), Next::Poll);
        assert_eq!(next(false, Waiting::Ours), Next::Finish);
    }

    #[test]
    fn a_finished_run_says_what_did_not_come_in() {
        let status = |imported, failed, deferred| SessionImportStatus {
            sources: vec![zlo_client::session_import::SessionImportSource {
                source: "claude_code".into(),
                label: "Claude Code".into(),
                enabled: true,
                available: true,
                counts: Some(SessionImportCounts {
                    imported,
                    failed,
                    deferred,
                    ..SessionImportCounts::default()
                }),
            }],
            background: true,
            running: false,
            last_run_at: Some(1),
            last_trigger: None,
            last_error: None,
            projects: None,
            started: false,
        };
        assert_eq!(outcome(&status(4, 0, 0)), (4, None));
        assert_eq!(
            outcome(&status(4, 1, 0)).1.as_deref(),
            Some("Imported 4 threads. 1 thread could not be imported.")
        );
        assert_eq!(
            outcome(&status(0, 2, 0)).1.as_deref(),
            Some("2 threads could not be imported.")
        );
        assert!(outcome(&status(1, 0, 3)).1.is_some());
    }
}
