//! Files: the project folder as a tree (GPUI `chat/side/project.rs`),
//! MonoCode Native's `explorer.rs` trimmed to what a reviewer needs:
//! folders open lazily, git colours mark changed files, ignored files hide
//! behind an eye toggle, a file previews in place and can be attached to
//! the next message.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rune::{El, col, icon, row};
use rune_kit::Kit;

use super::preview::{self, Opened};
use super::ui::{self, XS};
use super::{SideState, bridge, git, side, side_mut, side_of, thread_id};
use crate::icons;
use crate::jobs::{self, Generation};
use crate::state::State;

/// Folders this large list their first entries only.
const DIR_LIMIT: usize = 2000;

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
}

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
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

type Listed = (Vec<(PathBuf, Result<Vec<Entry>, String>)>, HashSet<String>, HashMap<String, char>);

/// Reload the open folders and the git marks.
pub(super) fn refresh(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side(s) else { return };
    let Some(root) = x.folder.clone() else { return };
    let repo = x.repo.clone();
    let generation = x.folder_gen;
    let mut dirs: Vec<PathBuf> = vec![root.clone()];
    dirs.extend(x.project.expanded.iter().cloned());
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
            let state = &mut x.project;
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
            state.ignored = ignored;
            state.letters = letters;
        },
    );
}

fn toggle(s: &mut State, dir: PathBuf) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let state = &mut x.project;
    if state.expanded.remove(&dir) {
        return;
    }
    state.expanded.insert(dir.clone());
    if state.children.contains_key(&dir) {
        return;
    }
    let listing = dir.clone();
    jobs::blocking(
        s,
        move || list_dir(&listing),
        move |s, result, _| match result {
            Ok(entries) => {
                if let Some(x) = side_of(s, &thread) {
                    x.project.children.insert(dir, entries);
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
    let Some(x) = side_mut(s) else { return };
    x.project.opened = Some(Opened::loading(title, path.clone()));
    let generation = x.project.open_gen.bump();
    jobs::blocking(
        s,
        move || preview::read(&path),
        move |s, read, _| {
            if let Some(x) = side_of(s, &thread).filter(|x| x.project.open_gen == generation)
                && let Some(opened) = x.project.opened.as_mut()
            {
                opened.preview = read;
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
    let Some(x) = side(s) else { return ui::loading(k) };
    if let Some(opened) = x.project.opened.as_ref() {
        let attach = opened.path.clone();
        return ui::fill_col()
            .child(preview::toolbar(s, k, opened, "project", close_preview))
            .child(
                row().px(12.).pt(8.).child(
                    ui::btn("Attach to next message")
                        .icon(&icons::PAPERCLIP)
                        .on_press(move |s: &mut State| bridge::attach(s, vec![attach.clone()]))
                        .build(k)
                        .key("project-attach"),
                ),
            )
            .child(preview::body(s, k, opened, "project-preview"));
    }
    let Some(root) = x.folder.clone() else { return ui::loading(k) };
    if let Some(e) = x.project.error.clone() {
        return ui::error_line(s, e);
    }
    if !x.project.children.contains_key(&root) {
        return ui::loading(k);
    }
    // Depth-first over the open folders.
    let mut rows: Vec<(Entry, usize)> = Vec::new();
    walk(x, &root, 0, &mut rows);
    let show_ignored = x.project.show_ignored;
    let mut list = col().w_full().px(4.);
    for (ix, (entry, depth)) in rows.into_iter().enumerate() {
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
            if expanded { &icons::FOLDER_OPEN } else { &icons::FOLDER }
        } else {
            &icons::FILE
        };
        let name = ui::line(entry.name.clone(), XS, color).grow(1.).min_w(0.);
        let mut line = ui::hover_row(s, format!("project-row-{}", entry.path.display()))
            .min_h(24.)
            .pl(6. + depth as f32 * 12.)
            .gap(6.)
            .child(icon(glyph, 12.).color(p.text_faint))
            .child(if ignored && !show_ignored { name.opacity(0.6) } else { name });
        if dirty_dir {
            line = line.child(ui::dot(p.warning, 5.));
        }
        if let Some(l) = letter {
            line = line.child(ui::line(l.to_string(), XS, color).w(12.));
        }
        let _ = ix;
        list = list.child(line.on_press(move |s: &mut State| {
            if dir {
                toggle(s, path.clone())
            } else {
                open(s, path.clone())
            }
        }));
    }
    let bar = row()
        .w_full()
        .px(8.)
        .gap(4.)
        .items_center()
        .child(ui::grow())
        .child(
            ui::ibtn(
                if show_ignored { &icons::EYE } else { &icons::EYE_OFF },
                if show_ignored { "Hide ignored files" } else { "Show ignored files" },
            )
            .on_press(|s: &mut State| {
                if let Some(x) = side_mut(s) {
                    x.project.show_ignored = !x.project.show_ignored;
                }
            })
            .build(k)
            .key("project-ignored"),
        )
        .child(
            ui::ibtn(&icons::EXTERNAL_LINK, "Show in Finder")
                .on_press(move |_: &mut State| ui::open_path(&root, true))
                .build(k)
                .key("project-reveal"),
        )
        .child(
            ui::ibtn(&icons::REFRESH_CW, "Refresh")
                .label("Refresh files")
                .on_press(refresh)
                .build(k)
                .key("project-refresh"),
        );
    ui::fill_col().child(bar).child(ui::scroll("project-tree", list))
}

/// Visible rows under `dir`: its entries, and the open folders' below them.
fn walk(x: &SideState, dir: &Path, depth: usize, rows: &mut Vec<(Entry, usize)>) {
    let state = &x.project;
    let Some(entries) = state.children.get(dir) else {
        return;
    };
    for entry in entries {
        if !state.show_ignored && repo_rel(x, &entry.path).is_some_and(|r| state.ignored.contains(&r)) {
            continue;
        }
        rows.push((entry.clone(), depth));
        if entry.dir && state.expanded.contains(&entry.path) {
            walk(x, &entry.path, depth + 1, rows);
        }
    }
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
