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

use rune::{El, col, el, row, text};
use rune_kit::{Checkbox, Kit};
use zlo_client::session_import::{
    ImportTrigger, ProjectSelection, SessionImportProject, SessionImportProjects,
    SessionImportRequest, SessionImportStatus,
};

use crate::design::brands::provider_logo;
use crate::jobs::{self, Generation};
use crate::pages::common::controls::{Look, Size, button_sized, muted, spinner};
use crate::pages::common::ts::{MONO_XS, sm, sm_medium, xs};
use crate::state::State;

use super::Finish;
use super::view::step_shell;

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
        move |s, result, _| {
            let p = &mut s.onboarding.projects;
            if p.scan_gen != generation {
                return;
            }
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
        |s, result, _| skip_saved(s, result.map(|_| ())),
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
        move |s, result, _| {
            let waiting = match &result {
                Ok(status) if status.started => Waiting::Ours,
                _ => Waiting::Other { retries },
            };
            landed(s, generation, result, waiting, again);
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
) {
    if s.onboarding.projects.import_gen != generation {
        return;
    }
    let status = match result {
        Ok(status) => status,
        Err(e) => {
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
                    move |s, result, _| landed(s, generation, result, waiting, request),
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
            s.onboarding.projects.importing = false;
            let (imported, warning) = outcome(&status);
            super::finish(s, finish_with(imported, warning));
        }
        Next::GiveUp => {
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

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.onboarding.projects;
    let busy = ps.importing;
    match &ps.scan {
        None | Some(Scan::Pending) => {
            return col()
                .key("projects-loading")
                .w_full()
                .min_h(160.)
                .child(
                    text("Your projects")
                        .font(rune::TextStyle::new(22., 600.))
                        .color(p.text),
                )
                .child(
                    col()
                        .w_full()
                        .grow(1.)
                        .py(24.)
                        .gap(12.)
                        .center()
                        .child(spinner(k))
                        .child(
                            text("Looking for projects from Claude Code and Codex…")
                                .font(sm())
                                .color(p.text_muted),
                        ),
                )
                .child(row().w_full().justify_end().child(skip_button(k, busy)));
        }
        _ => {}
    }
    let mut body = step_shell(
        s,
        "Import your projects",
        "Import projects and conversations from Claude Code and Codex on your selected computer.",
    );
    let projects = listed(s);
    let pick = selected(s);
    if !projects.is_empty() {
        let all = pick.len() == projects.len();
        let all_keys: Vec<String> = projects.iter().map(|p| p.path.clone()).collect();
        body = body.child(
            row()
                .w_full()
                .pt(20.)
                .gap(12.)
                .center_y()
                .child(
                    text(format!("{} of {} selected", pick.len(), projects.len()))
                        .font(xs())
                        .color(p.text_muted)
                        .grow(1.),
                )
                .child(
                    button_sized("Select all", Look::Ghost, Size::XSmall)
                        .disabled(busy || all)
                        .on_press(move |s: &mut State| set_keys(s, all_keys.clone(), true))
                        .build(k)
                        .key("projects-select-all"),
                )
                .child(
                    button_sized("Select none", Look::Ghost, Size::XSmall)
                        .disabled(busy || pick.is_empty())
                        .on_press(|s: &mut State| {
                            s.onboarding.projects.selection = Some(BTreeSet::new())
                        })
                        .build(k)
                        .key("projects-select-none"),
                ),
        );
    }
    let mut list = col().w_full().gap(2.).pr(12.);
    match &ps.scan {
        Some(Scan::Failed(e)) => {
            list = list.child(
                row()
                    .w_full()
                    .gap(12.)
                    .center_y()
                    .child(
                        text(format!("Could not check projects. {e}"))
                            .font(sm())
                            .color(p.text_muted)
                            .wrap()
                            .grow(1.),
                    )
                    .child(
                        button_sized("Retry", Look::Ghost, Size::Small)
                            .on_press(scan)
                            .build(k)
                            .key("projects-retry"),
                    ),
            );
        }
        Some(Scan::Unsupported) => {
            list = list.child(muted(
                p,
                "This IronClaw cannot list projects yet. Skip this step, or update IronClaw on that computer.",
            ));
        }
        Some(Scan::Ready(scan)) => {
            if projects.is_empty() {
                list = list.child(
                    text("No existing Claude Code or Codex projects found.")
                        .font(sm())
                        .color(p.text_muted)
                        .py(8.),
                );
            }
            if scan.truncated {
                list = list.child(muted(p, SCAN_LIMIT_MESSAGE));
            }
            list = list.child(candidate_list(s, k, projects, &pick));
        }
        _ => {}
    }
    body = body.child(
        el().key("projects-scroll")
            .w_full()
            .pt(8.)
            .max_h(320.)
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .child(list),
    );
    if let Some(e) = &ps.error {
        body = body.child(text(e.clone()).font(sm()).color(p.danger).pt(12.).wrap());
    }
    let n = pick.len();
    body.child(
        row()
            .w_full()
            .pt(24.)
            .gap(12.)
            .center_y()
            .justify_end()
            .child(skip_button(k, busy))
            .child(
                button_sized(
                    if busy {
                        "Importing…".to_string()
                    } else {
                        format!("Import {n} {}", if n == 1 { "project" } else { "projects" })
                    },
                    Look::Primary,
                    Size::Small,
                )
                .disabled(busy || n == 0)
                .on_press(run_import)
                .build(k)
                .key("projects-import"),
            ),
    )
}

fn skip_button(k: &Kit, busy: bool) -> El {
    button_sized("Do not import projects", Look::Ghost, Size::Small)
        .disabled(busy)
        .on_press(skip)
        .build(k)
        .key("projects-skip")
}

fn group_open(s: &State, key: &str, default: bool) -> bool {
    default != s.onboarding.projects.flipped.contains(key)
}

fn flip(s: &mut State, key: String) {
    let f = &mut s.onboarding.projects.flipped;
    if !f.remove(&key) {
        f.insert(key);
    }
}

fn candidate_list(
    s: &State,
    k: &Kit,
    projects: &[SessionImportProject],
    pick: &BTreeSet<String>,
) -> El {
    let (groups, other) = group(projects);
    let mut c = col().w_full().gap(2.);
    for g in &groups {
        if let [only] = g.candidates.as_slice() {
            c = c.child(candidate_row(s, k, only, Some(&g.label), false, pick));
            continue;
        }
        let keys: Vec<String> = g.candidates.iter().map(|p| p.path.clone()).collect();
        let open = group_open(s, &g.key, true);
        let sources: BTreeSet<&str> = g
            .candidates
            .iter()
            .flat_map(|p| p.sources.iter().map(String::as_str))
            .collect();
        c = c.child(group_header(
            s,
            k,
            &g.key,
            &g.label,
            keys,
            open,
            pick,
            Some(sources.into_iter().collect()),
            g.thread_count,
            g.last_active_at,
        ));
        if open {
            for p in &g.candidates {
                c = c.child(candidate_row(s, k, p, None, true, pick));
            }
        }
    }
    if !other.is_empty() {
        let keys: Vec<String> = other.iter().map(|p| p.path.clone()).collect();
        let open = group_open(s, "other", false);
        let label = format!(
            "Other folders · {} {}",
            other.len(),
            if other.len() == 1 {
                "folder"
            } else {
                "folders"
            }
        );
        c = c.child(group_header(
            s, k, "other", &label, keys, open, pick, None, 0, None,
        ));
        if open {
            for p in &other {
                c = c.child(candidate_row(s, k, p, None, true, pick));
            }
        }
    }
    c
}

// arch-exempt: too_many_args, one row's parts; a view-only helper, plan #onboarding-port
#[allow(clippy::too_many_arguments)]
fn group_header(
    s: &State,
    k: &Kit,
    key: &str,
    label: &str,
    keys: Vec<String>,
    open: bool,
    pick: &BTreeSet<String>,
    sources: Option<Vec<&str>>,
    threads: u32,
    last: Option<i64>,
) -> El {
    let p = &s.palette;
    let chosen = keys.iter().filter(|k| pick.contains(*k)).count();
    let all = chosen == keys.len();
    let busy = s.onboarding.projects.importing;
    let toggle_keys = keys.clone();
    let flip_key = key.to_string();
    let mut line = row()
        .key(format!("group-{key}"))
        .w_full()
        .gap(10.)
        .px(8.)
        .py(6.)
        .radius(6.)
        .center_y()
        .hover(p.hover)
        .child(
            Checkbox::new(all)
                .indeterminate(chosen > 0 && !all)
                .label(label.to_string())
                .hide_label()
                .disabled(busy)
                .on_toggle(move |s: &mut State| set_keys(s, toggle_keys.clone(), !all))
                .build(k)
                .key(format!("group-check-{key}")),
        )
        .child(
            row()
                .grow(1.)
                .min_w(0.)
                .gap(6.)
                .center_y()
                .cursor(rune::Cursor::Pointer)
                .on_press(move |s: &mut State| flip(s, flip_key.clone()))
                .child(
                    rune::icon(
                        if open {
                            &crate::icons::CHEVRON_DOWN
                        } else {
                            &crate::icons::CHEVRON_RIGHT
                        },
                        14.,
                    )
                    .color(p.text_muted),
                )
                .child(
                    text(label.to_string())
                        .font(sm_medium())
                        .color(if sources.is_some() {
                            p.text
                        } else {
                            p.text_muted
                        })
                        .truncate()
                        .min_w(0.),
                ),
        );
    if let Some(sources) = sources {
        line = line.child(meta(s, Some(sources), threads, last));
    }
    line
}

fn candidate_row(
    s: &State,
    k: &Kit,
    project: &SessionImportProject,
    label: Option<&str>,
    nested: bool,
    pick: &BTreeSet<String>,
) -> El {
    let p = &s.palette;
    let on = pick.contains(&project.path);
    let path = project.path.clone();
    let (name, parent) = split_path(&project.path);
    let mut text_part = row().grow(1.).min_w(0.).gap(8.).center_y();
    if let Some(label) = label {
        text_part = text_part.child(
            text(label.to_string())
                .font(sm_medium())
                .color(p.text)
                .truncate()
                .min_w(0.),
        );
    }
    let path_ink = if label.is_some() {
        p.text_faint
    } else {
        p.text
    };
    text_part = text_part.child(
        row()
            .min_w(0.)
            .child(
                text(parent.to_string())
                    .font(MONO_XS)
                    .color(p.text_faint)
                    .truncate()
                    .min_w(0.),
            )
            .child(
                text(name.to_string())
                    .font(MONO_XS)
                    .color(path_ink)
                    .single_line(),
            ),
    );
    let sources: Vec<&str> = project.sources.iter().map(String::as_str).collect();
    row()
        .key(format!("project-{}", project.path))
        .w_full()
        .gap(10.)
        .pl(if nested { 32. } else { 8. })
        .pr(8.)
        .py(6.)
        .radius(6.)
        .center_y()
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .help(project.path.clone())
        .on_press(move |s: &mut State| {
            if !s.onboarding.projects.importing {
                set_keys(s, vec![path.clone()], !on)
            }
        })
        .child(
            Checkbox::new(on)
                .label(project.title.clone())
                .hide_label()
                .disabled(s.onboarding.projects.importing)
                .build(k)
                .key(format!("project-check-{}", project.path)),
        )
        .child(text_part)
        .child(meta(
            s,
            (!nested).then_some(sources),
            project.thread_count,
            project.last_active_at,
        ))
}

/// t3code `ImportRowMeta`: source marks, the thread count and the age, in
/// fixed columns so rows line up.
fn meta(s: &State, sources: Option<Vec<&str>>, threads: u32, last: Option<i64>) -> El {
    let p = &s.palette;
    let now = chrono::Utc::now().timestamp_millis();
    let mut marks = row().w(40.).gap(4.).center_y();
    for (source, logo) in [("claude_code", "claude-cli"), ("codex", "codex")] {
        let shown = sources.as_ref().is_some_and(|list| list.contains(&source));
        marks = marks.child(if shown {
            el().w(16.).center().child(provider_logo(s, &[logo], 12.))
        } else {
            el().w(16.)
        });
    }
    row()
        .gap(4.)
        .center_y()
        .child(marks)
        .child(
            text(threads.to_string())
                .font(xs())
                .color(p.text_muted)
                .w(32.)
                .align(rune::Align::End),
        )
        .child(
            text(last.map(|at| short_age(at, now)).unwrap_or_default())
                .font(xs())
                .color(p.text_muted)
                .w(36.)
                .align(rune::Align::End),
        )
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
