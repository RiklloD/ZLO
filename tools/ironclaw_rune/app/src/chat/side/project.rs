//! Files: the project folder as a tree, MonoCode Native's `explorer.rs` trimmed
//! to what a reviewer needs: folders open lazily, git colours mark changed
//! files, ignored files hide behind an eye toggle, a file previews in place and
//! can be attached to the next message.
//!
//! With the backend on another computer the tree is that thread's project
//! workspace, listed and read through the gateway (`thread_files`,
//! `thread_file_bytes`): no local folder, no git marks, no Finder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rune::{El, el, icon, row};
use rune_kit::Kit;

use super::drill::{self, Level};
use super::flash;
use super::preview::{self, Opened};
use super::ui::{self, xs};
use super::{SideState, bridge, git, side, side_mut, side_of, thread_id};
use crate::design::kit::{self, Turn};
use crate::design::motion;
use crate::icons;
use crate::jobs::{self, Generation};
use crate::state::State;
use zlo_client::ClientError;
use zlo_client::workspace_views::{ProjectFsEntry, ProjectFsEntryKind};

/// Folders this large list their first entries only.
const DIR_LIMIT: usize = 2000;
/// The backend's project workspace, the tree's root when it is remote.
const REMOTE_ROOT: &str = "/workspace";

/// The folder the tree starts at: the backend's workspace when remote,
/// else the thread's project folder on this Mac.
fn root(s: &State, x: &SideState) -> Option<PathBuf> {
    if crate::remote::backend_is_remote(s) {
        Some(PathBuf::from(REMOTE_ROOT))
    } else {
        x.folder.clone()
    }
}

/// A gateway listing as tree entries, folders first (dot-files are the
/// gateway's own bookkeeping, as in Outputs).
fn remote_entries(list: Vec<ProjectFsEntry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = list
        .into_iter()
        .filter(|e| !e.name.starts_with('.'))
        .take(DIR_LIMIT)
        .map(|e| Entry {
            dir: matches!(e.kind, ProjectFsEntryKind::Directory),
            path: PathBuf::from(e.path),
            name: e.name,
        })
        .collect();
    sort(&mut out);
    out
}

/// List `dirs` through the gateway, for the remote tree.
async fn list_remote(
    c: zlo_client::Client,
    thread: String,
    dirs: Vec<PathBuf>,
) -> Vec<(PathBuf, Result<Vec<Entry>, String>)> {
    let mut out = Vec::new();
    for dir in dirs {
        let asked = dir.to_string_lossy().into_owned();
        let listed = c
            .thread_files(&thread, Some(&asked))
            .await
            .map(|l| remote_entries(l.entries))
            .map_err(|e| format!("Couldn't read {asked}: {e}"));
        out.push((dir, listed));
    }
    out
}

#[derive(Clone)]
pub(super) struct Entry {
    name: String,
    path: PathBuf,
    dir: bool,
}

#[derive(Default)]
pub(super) struct ProjectState {
    children: HashMap<PathBuf, Vec<Entry>>,
    expanded: HashSet<PathBuf>,
    /// Paths relative to the repository top, as git prints them.
    ignored: HashSet<String>,
    letters: HashMap<String, char>,
    show_ignored: bool,
    error: Option<String>,
    pub(super) opened: Option<Opened>,
    open_gen: Generation,
    /// The folder the person last opened, and when: its rows reveal for
    /// [`REVEAL_FOR`] after, and never again as they scroll back in.
    revealed: Option<(PathBuf, Instant)>,
}

/// How long after a folder opens its rows still mount with a reveal.
const REVEAL_FOR: Duration = Duration::from_millis(500);
/// A tree row's height (rows are fixed, so the tree is virtual rows: a
/// folder can list up to [`DIR_LIMIT`] entries, and only those near the
/// viewport are built).
const TREE_ROW_H: f32 = 24.;

fn list_dir(dir: &Path) -> Result<Vec<Entry>, String> {
    let mut out: Vec<Entry> = std::fs::read_dir(dir)
        .map_err(|e| format!("Couldn't read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_name() != ".git")
        .take(DIR_LIMIT)
        .map(|e| {
            let path = e.path();
            Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                dir: path.is_dir(),
                path,
            }
        })
        .collect();
    sort(&mut out);
    Ok(out)
}

fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

type Listed = (
    Vec<(PathBuf, Result<Vec<Entry>, String>)>,
    HashSet<String>,
    HashMap<String, char>,
);

/// Reload the open folders and the git marks.
pub(super) fn refresh(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side(s) else { return };
    let Some(root) = root(s, x) else { return };
    let repo = x.repo.clone();
    let generation = x.folder_gen;
    let mut dirs: Vec<PathBuf> = vec![root.clone()];
    dirs.extend(x.project.expanded.iter().cloned());
    if crate::remote::backend_is_remote(s) {
        let asked = thread.clone();
        jobs::run(
            s,
            "project",
            move |c| async move { Ok::<_, ClientError>(list_remote(c, asked, dirs).await) },
            move |s, listed, _| {
                let Ok(listed) = listed else { return };
                if let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) {
                    land(&mut x.project, &root, listed);
                }
            },
        );
        return;
    }
    jobs::blocking(
        s,
        move || -> Listed {
            let listed = dirs
                .into_iter()
                .map(|d| {
                    let listed = list_dir(&d);
                    (d, listed)
                })
                .collect();
            let (ignored, letters) = match &repo {
                Some(repo) => (
                    git::ignored(repo),
                    git::status(repo)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| (c.path.clone(), c.letter()))
                        .collect(),
                ),
                None => Default::default(),
            };
            (listed, ignored, letters)
        },
        move |s, (listed, ignored, letters), _| {
            let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) else {
                return;
            };
            land(&mut x.project, &root, listed);
            x.project.ignored = ignored;
            x.project.letters = letters;
        },
    );
}

/// Put a refresh's listings in the tree; a folder that went away closes.
fn land(state: &mut ProjectState, root: &Path, listed: Vec<(PathBuf, Result<Vec<Entry>, String>)>) {
    state.error = None;
    for (dir, result) in listed {
        match result {
            Ok(entries) => {
                state.children.insert(dir, entries);
            }
            Err(e) if dir == root => state.error = Some(e),
            Err(_) => {
                state.expanded.remove(&dir);
            }
        }
    }
}

fn toggle(s: &mut State, dir: PathBuf) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let state = &mut x.project;
    if state.expanded.remove(&dir) {
        return;
    }
    state.expanded.insert(dir.clone());
    state.revealed = Some((dir.clone(), Instant::now()));
    if state.children.contains_key(&dir) {
        return;
    }
    let listing = dir.clone();
    if crate::remote::backend_is_remote(s) {
        let asked = thread.clone();
        jobs::run(
            s,
            "project",
            move |c| async move { Ok::<_, ClientError>(list_remote(c, asked, vec![listing]).await) },
            move |s, listed, tx| {
                let Ok(listed) = listed else { return };
                for (dir, result) in listed {
                    match result {
                        Ok(entries) => {
                            if let Some(x) = side_of(s, &thread) {
                                x.project.children.insert(dir, entries);
                                // The folder the person opened: its rows
                                // reveal (a job's commit lands at once).
                                tx.animated();
                            }
                        }
                        Err(e) => bridge::set_error(s, e),
                    }
                }
            },
        );
        return;
    }
    jobs::blocking(
        s,
        move || list_dir(&listing),
        move |s, result, tx| match result {
            Ok(entries) => {
                if let Some(x) = side_of(s, &thread) {
                    x.project.children.insert(dir, entries);
                    // The folder the person opened: its rows reveal.
                    tx.animated();
                }
            }
            Err(e) => bridge::set_error(s, e),
        },
    );
}

/// Preview a file of the project in the tab.
pub(super) fn open(s: &mut State, path: PathBuf) {
    let Some(thread) = thread_id(s) else { return };
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if crate::remote::backend_is_remote(s) {
        return open_remote(s, thread, title, path);
    }
    let Some(x) = side_mut(s) else { return };
    x.project.opened = Some(Opened::loading(title, path.clone()));
    let generation = x.project.open_gen.bump();
    jobs::blocking(
        s,
        move || preview::read(&path),
        move |s, read, tx| {
            if let Some(x) = side_of(s, &thread).filter(|x| x.project.open_gen == generation)
                && let Some(opened) = x.project.opened.as_mut()
            {
                preview::land(opened, read, tx);
            }
        },
    );
}

/// Preview a file of the remote workspace: fetch it into the app's temp
/// folder (one folder per remote path, as Outputs does) and read that copy.
fn open_remote(s: &mut State, thread: String, title: String, path: PathBuf) {
    let remote = path.to_string_lossy().into_owned();
    let file = crate::chat::downloads::file_name(&remote);
    let dir = crate::chat::downloads::dir_for("project", &[&thread, &remote]);
    let Some(x) = side_mut(s) else { return };
    x.project.opened = Some(Opened::loading(title, dir.join(&file)));
    let generation = x.project.open_gen.bump();
    let asked = thread.clone();
    jobs::run(
        s,
        "project",
        move |c| async move {
            let bytes = c.thread_file_bytes(&asked, &remote).await?;
            let local = crate::chat::downloads::save_async(dir, file, bytes)
                .await
                .map_err(|e| ClientError::Decode(format!("Couldn't save the file: {e}")))?;
            Ok::<_, ClientError>(tokio::task::spawn_blocking(move || preview::read(&local)).await)
        },
        move |s, result, tx| {
            let read = match result {
                Ok(Ok(read)) => read,
                Ok(Err(e)) => preview::Preview::Failed(e.to_string()),
                Err(e) => preview::Preview::Failed(e.to_string()),
            };
            if let Some(x) = side_of(s, &thread).filter(|x| x.project.open_gen == generation)
                && let Some(opened) = x.project.opened.as_mut()
            {
                preview::land(opened, read, tx);
            }
        },
    );
}

/// The path relative to the repository top, as git keys it.
fn repo_rel(x: &SideState, path: &Path) -> Option<String> {
    let repo = x.repo.as_ref()?;
    path.strip_prefix(repo)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn close_preview(s: &mut State) {
    if let Some(x) = side_mut(s) {
        x.project.opened = None;
    }
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    if let Some(opened) = x.project.opened.as_ref() {
        let attach = opened.path.clone();
        let attached = flash::shows(s, "project-attach");
        let mut bar = row().px(12.).pt(8.).gap(6.).items_center().child(
            ui::btn("Attach to next message")
                .icon(&icons::PAPERCLIP)
                .on_press_tx(move |s: &mut State, tx| {
                    bridge::attach(s, vec![attach.clone()]);
                    flash::flash(s, "project-attach", tx);
                })
                .build(k)
                .key("project-attach")
                .press_scale(motion::PRESS),
        );
        // "Attached ✓" beside the button for a moment, popping in and out.
        if attached {
            bar = bar.child(
                row()
                    .key("project-attached")
                    .gap(4.)
                    .items_center()
                    .transition(motion::glyph_pop())
                    .child(icon(&icons::CHECK, 12.).color(p.success))
                    .child(ui::line("Attached", xs(), p.text_muted)),
            );
        }
        return drill::level(
            "project",
            Level::Detail,
            ui::fill_col()
                .child(preview::toolbar(s, k, opened, "project", close_preview))
                .child(bar)
                .child(preview::body(s, k, opened, "project-preview")),
        );
    }
    drill::level("project", Level::List, tree(s, k, x))
}

/// The tree, or its loading / error line.
fn tree(s: &State, k: &Kit, x: &SideState) -> El {
    let Some(root) = root(s, x) else {
        return ui::loading(k);
    };
    if let Some(e) = x.project.error.clone() {
        return ui::error_line(s, e);
    }
    if !x.project.children.contains_key(&root) {
        return ui::loading(k);
    }
    // Depth-first over the open folders.
    let mut rows: Vec<(Entry, usize)> = Vec::new();
    walk(x, &root, 0, &mut rows);
    let rows = Rc::new(rows);
    let keys = rows.clone();
    ui::virtual_scroll(
        "project-tree",
        rows.len(),
        TREE_ROW_H,
        move |_, i| {
            keys.get(i)
                .map(|(e, _)| format!("project-row-{}", e.path.display()))
                .unwrap_or_default()
        },
        move |s, i| match (side(s), rows.get(i)) {
            (Some(x), Some((entry, depth))) => {
                el().w_full().px(4.).child(tree_row(s, x, entry, *depth))
            }
            _ => el(),
        },
    )
}

/// One row of the tree: disclosure chevron (folders), glyph, name, git
/// marks.
fn tree_row(s: &State, x: &SideState, entry: &Entry, depth: usize) -> El {
    let p = &s.palette;
    let show_ignored = x.project.show_ignored;
    let rel = repo_rel(x, &entry.path);
    let ignored = rel.as_ref().is_some_and(|r| x.project.ignored.contains(r));
    let letter = rel.as_ref().and_then(|r| x.project.letters.get(r)).copied();
    // A folder holding changes gets a dot.
    let dirty_dir = entry.dir
        && rel.as_ref().is_some_and(|r| {
            let prefix = format!("{r}/");
            x.project.letters.keys().any(|k| k.starts_with(&prefix))
        });
    let color = match letter {
        Some('M') => p.warning,
        Some('A') | Some('U') => p.success,
        Some('D') => p.danger,
        Some(_) => p.accent,
        None if ignored => p.text_faint,
        None => p.text.alpha(0.85),
    };
    let expanded = x.project.expanded.contains(&entry.path);
    let path = entry.path.clone();
    let dir = entry.dir;
    let glyph = if dir {
        if expanded {
            &icons::FOLDER_OPEN
        } else {
            &icons::FOLDER
        }
    } else {
        &icons::FILE
    };
    let name = ui::line(entry.name.clone(), xs(), color).grow(1.).min_w(0.);
    let disclosure = if dir {
        kit::chevron("chevron", expanded, Turn::Disclose, 10., p.text_faint)
    } else {
        el().w(10.)
    };
    let mut line = ui::hover_row(s, format!("project-row-{}", entry.path.display()))
        .min_h(TREE_ROW_H)
        .pl(6. + depth as f32 * 12.)
        .gap(6.)
        .child(disclosure)
        .child(icon(glyph, 12.).color(p.text_faint))
        .child(if ignored && !show_ignored {
            name.opacity(0.6)
        } else {
            name
        });
    if dir {
        line = line.expanded(expanded);
    }
    if dirty_dir {
        line = line.child(ui::dot(p.warning, 5.));
    }
    if let Some(l) = letter {
        line = line.child(ui::line(l.to_string(), xs(), color).w(12.));
    }
    // The rows of the folder the person just opened wipe open under it.
    if x.project.revealed.as_ref().is_some_and(|(opened, at)| {
        entry.path.parent() == Some(opened.as_path()) && at.elapsed() < REVEAL_FOR
    }) {
        line = line.transition(motion::reveal());
    }
    line.on_press(move |s: &mut State| {
        if dir {
            toggle(s, path.clone())
        } else {
            open(s, path.clone())
        }
    })
}

/// The Files tab's buttons, for the dock's title row: show ignored files,
/// show the folder in Finder (not for a remote workspace), refresh. None
/// while a file is open (its preview has its own toolbar) or the tree has
/// not loaded.
pub(super) fn actions(s: &State, k: &Kit) -> Vec<El> {
    let Some(x) = side(s) else { return Vec::new() };
    if x.project.opened.is_some() || x.project.error.is_some() {
        return Vec::new();
    }
    let Some(root) = root(s, x) else {
        return Vec::new();
    };
    let show_ignored = x.project.show_ignored;
    let mut out = vec![
        ui::ibtn(
            if show_ignored {
                &icons::EYE
            } else {
                &icons::EYE_OFF
            },
            if show_ignored {
                "Hide ignored files"
            } else {
                "Show ignored files"
            },
        )
        .on_press(|s: &mut State| {
            if let Some(x) = side_mut(s) {
                x.project.show_ignored = !x.project.show_ignored;
            }
        })
        .build(k)
        .key("project-ignored"),
    ];
    // A remote workspace is not on this Mac's disk.
    if !crate::remote::backend_is_remote(s) {
        out.push(
            ui::ibtn(&icons::EXTERNAL_LINK, "Show in Finder")
                .on_press(move |_: &mut State| ui::open_path(&root, true))
                .build(k)
                .key("project-reveal"),
        );
    }
    out.push(
        ui::ibtn(&icons::REFRESH_CW, "Refresh")
            .label("Refresh files")
            .on_press(refresh)
            .build(k)
            .key("project-refresh"),
    );
    out
}

/// Visible rows under `dir`: its entries, and the open folders' below them.
fn walk(x: &SideState, dir: &Path, depth: usize, rows: &mut Vec<(Entry, usize)>) {
    let state = &x.project;
    let Some(entries) = state.children.get(dir) else {
        return;
    };
    for entry in entries {
        if !state.show_ignored
            && repo_rel(x, &entry.path).is_some_and(|r| state.ignored.contains(&r))
        {
            continue;
        }
        rows.push((entry.clone(), depth));
        if entry.dir && state.expanded.contains(&entry.path) {
            walk(x, &entry.path, depth + 1, rows);
        }
    }
}

/// Fill the remote tree's root with `entries` (`(name, is_folder)`), as
/// the gateway would list them (scenes only).
pub(super) fn fixture_remote(x: &mut SideState, entries: &[(&str, bool)]) {
    let list = entries
        .iter()
        .map(|(name, dir)| ProjectFsEntry {
            name: (*name).to_string(),
            path: format!("{REMOTE_ROOT}/{name}"),
            kind: if *dir {
                ProjectFsEntryKind::Directory
            } else {
                ProjectFsEntryKind::File
            },
        })
        .collect();
    x.project
        .children
        .insert(PathBuf::from(REMOTE_ROOT), remote_entries(list));
}

/// Fill the tree from a real folder synchronously (scenes only).
pub(super) fn fixture_load(x: &mut SideState, expand: &[&str]) {
    let Some(root) = x.folder.clone() else { return };
    let mut dirs = vec![root.clone()];
    dirs.extend(expand.iter().map(|d| root.join(d)));
    for d in dirs {
        if let Ok(entries) = list_dir(&d) {
            if d != root {
                x.project.expanded.insert(d.clone());
            }
            x.project.children.insert(d, entries);
        }
    }
    if let Some(repo) = &x.repo {
        x.project.ignored = git::ignored(repo);
        x.project.letters = git::status(repo)
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c.path.clone(), c.letter()))
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remote_tree_reads_the_backend_workspace() {
        let mut s = crate::chat::fixtures::test_state();
        let mut x = SideState {
            folder: Some(PathBuf::from("/Users/you/code/zlo")),
            ..Default::default()
        };
        assert_eq!(root(&s, &x), x.folder.clone());
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
        assert_eq!(root(&s, &x), Some(PathBuf::from(REMOTE_ROOT)));

        fixture_remote(
            &mut x,
            &[("b.md", false), (".ironclaw", true), ("src", true)],
        );
        let names: Vec<_> = x.project.children[Path::new(REMOTE_ROOT)]
            .iter()
            .map(|e| (e.name.as_str(), e.dir, e.path.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("src", true, PathBuf::from("/workspace/src")),
                ("b.md", false, PathBuf::from("/workspace/b.md")),
            ],
            "folders first, the gateway's dot-files hidden"
        );
    }

    /// A folder lists up to DIR_LIMIT entries: the tree builds only the
    /// rows near its viewport, not one element per entry.
    #[test]
    fn a_huge_folder_builds_only_the_rows_in_view() {
        let mut s = crate::chat::fixtures::test_state();
        crate::chat::fixtures::chat(&mut s);
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
        s.prefs.work_open = true;
        s.prefs.work_tab = super::super::Tab::Files.id().into();
        let names: Vec<String> = (0..DIR_LIMIT).map(|i| format!("f{i:04}.txt")).collect();
        let entries: Vec<(&str, bool)> = names.iter().map(|n| (n.as_str(), false)).collect();
        if let Some(x) = side_mut(&mut s) {
            fixture_remote(x, &entries);
        }
        let mut rt = rune::Runtime::new(s, crate::view, (1280., 820.), rune::Env::new(true, 1.));
        rt.frame(0.);
        for _ in 0..10 {
            rt.frame(1. / 60.);
        }
        let built: HashSet<String> = rt
            .elements()
            .into_iter()
            .filter_map(|e| e.key)
            .filter(|k| k.starts_with("project-row-"))
            .collect();
        let built = built.len();
        assert!(built > 0, "the tree draws");
        // The viewport holds about 30; overscan builds some more.
        assert!(built < 150, "{built} rows built for {DIR_LIMIT} entries");
    }
}
