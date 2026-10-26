//! Files: the project folder as a tree, MonoCode Native's `explorer.rs`
//! trimmed to what a reviewer needs: folders open lazily, git colors mark
//! changed files, ignored files hide behind an eye toggle, a file previews
//! in place and can be attached to the next message.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::preview::{self, Opened};
use super::{ChatView, error_line, git, loading, scroll};
use crate::assets::ExtraIcon;
use crate::design::{Palette, kit};
use crate::ui::*;

/// Folders this large list their first entries only.
const DIR_LIMIT: usize = 2000;

#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    dir: bool,
}

#[derive(Default)]
pub(super) struct State {
    children: HashMap<PathBuf, Vec<Entry>>,
    expanded: HashSet<PathBuf>,
    /// Paths relative to the repository top, as git prints them.
    ignored: HashSet<String>,
    letters: HashMap<String, char>,
    show_ignored: bool,
    error: Option<String>,
    opened: Option<Opened>,
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
    out.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

impl ChatView {
    /// Reload the open folders and the git marks.
    pub(super) fn project_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.side.folder.clone() else {
            return;
        };
        let repo = self.side.repo.clone();
        let mut dirs: Vec<PathBuf> = vec![root.clone()];
        dirs.extend(self.side.project.expanded.iter().cloned());
        let task = cx.background_spawn(async move {
            let listed: Vec<(PathBuf, Result<Vec<Entry>, String>)> = dirs
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
        });
        cx.spawn(async move |this, cx| {
            let (listed, ignored, letters) = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.side.project;
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
                cx.notify();
            });
        })
        .detach();
    }

    fn project_toggle(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let state = &mut self.side.project;
        if state.expanded.remove(&dir) {
            cx.notify();
            return;
        }
        state.expanded.insert(dir.clone());
        if state.children.contains_key(&dir) {
            cx.notify();
            return;
        }
        let task = cx.background_spawn({
            let dir = dir.clone();
            async move { list_dir(&dir) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        this.side.project.children.insert(dir, entries);
                    }
                    Err(e) => this.set_error(e, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn project_open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.side.project.opened = Some(Opened::loading(title, path.clone()));
        cx.notify();
        let task = cx.background_spawn(async move { preview::read(&path) });
        cx.spawn(async move |this, cx| {
            let read = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(opened) = this.side.project.opened.as_mut() {
                    opened.preview = read;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The path relative to the repository top, as git keys it.
    fn repo_rel(&self, path: &Path) -> Option<String> {
        let repo = self.side.repo.as_ref()?;
        path.strip_prefix(repo)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    }

    pub(super) fn render_project(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        if let Some(opened) = self.side.project.opened.as_ref() {
            let weak = cx.weak_entity();
            let key = opened.path.display().to_string();
            let attach = opened.path.clone();
            return v_flex()
                .flex_1()
                .min_h_0()
                .child(preview::toolbar(
                    opened,
                    "project",
                    move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.side.project.opened = None;
                            cx.notify();
                        });
                    },
                    cx,
                ))
                .child(
                    h_flex().px_3().pt_2().child(
                        Button::new("project-attach")
                            .label("Attach to next message")
                            .icon(ExtraIcon::Paperclip)
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.attach(vec![attach.clone()], window, cx)
                            })),
                    ),
                )
                .child(
                    scroll("project-preview")
                        .p_3()
                        .child(preview::render(opened, &key, cx)),
                )
                .into_any_element();
        }
        let Some(root) = self.side.folder.clone() else {
            return loading();
        };
        if let Some(e) = self.side.project.error.clone() {
            return error_line(e, cx);
        }
        if !self.side.project.children.contains_key(&root) {
            return loading();
        }
        // Depth-first over the open folders.
        let mut rows: Vec<(Entry, usize)> = Vec::new();
        self.walk(&root, 0, &mut rows);
        let show_ignored = self.side.project.show_ignored;
        let list =
            v_flex()
                .px_1()
                .children(rows.into_iter().enumerate().map(|(ix, (entry, depth))| {
                    let rel = self.repo_rel(&entry.path);
                    let ignored = rel
                        .as_ref()
                        .is_some_and(|r| self.side.project.ignored.contains(r));
                    let letter = rel
                        .as_ref()
                        .and_then(|r| self.side.project.letters.get(r))
                        .copied();
                    // A folder holding changes gets a dot.
                    let dirty_dir = entry.dir
                        && rel.as_ref().is_some_and(|r| {
                            let prefix = format!("{r}/");
                            self.side
                                .project
                                .letters
                                .keys()
                                .any(|k| k.starts_with(&prefix))
                        });
                    let color = match letter {
                        Some('M') => p.warning,
                        Some('A') | Some('U') => p.success,
                        Some('D') => p.danger,
                        Some(_) => p.accent,
                        None if ignored => p.text_faint,
                        None => p.text.opacity(0.85),
                    };
                    let expanded = self.side.project.expanded.contains(&entry.path);
                    let path = entry.path.clone();
                    let dir = entry.dir;
                    let icon = if dir {
                        Icon::new(if expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                    } else {
                        Icon::new(IconName::File)
                    };
                    kit::row(format!("project-row-{ix}"), false, cx)
                        .min_h(px(24.))
                        .pl(px(6. + depth as f32 * 12.))
                        .gap_1p5()
                        .text_xs()
                        .child(icon.xsmall().flex_none().text_color(p.text_faint))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(color)
                                .when(ignored && !show_ignored, |d| d.opacity(0.6))
                                .child(entry.name.clone()),
                        )
                        .when(dirty_dir, |d| {
                            d.child(div().size(px(5.)).rounded_full().bg(p.warning))
                        })
                        .when_some(letter, |d, l| {
                            d.child(
                                div()
                                    .flex_none()
                                    .w(px(12.))
                                    .text_color(color)
                                    .child(l.to_string()),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if dir {
                                this.project_toggle(path.clone(), cx)
                            } else {
                                this.project_open(path.clone(), cx)
                            }
                        }))
                }));
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1())
                    .child(
                        Button::new("project-ignored")
                            .icon(if show_ignored {
                                IconName::Eye
                            } else {
                                IconName::EyeOff
                            })
                            .ghost()
                            .xsmall()
                            .tooltip(if show_ignored {
                                "Hide ignored files"
                            } else {
                                "Show ignored files"
                            })
                            .accessibility_label("Show ignored files")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.side.project.show_ignored = !this.side.project.show_ignored;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("project-reveal")
                            .icon(IconName::ExternalLink)
                            .ghost()
                            .xsmall()
                            .tooltip("Show in Finder")
                            .accessibility_label("Show in Finder")
                            .on_click(move |_, _, cx| cx.reveal_path(&root)),
                    )
                    .child(
                        Button::new("project-refresh")
                            .icon(IconName::RefreshCw)
                            .ghost()
                            .xsmall()
                            .tooltip("Refresh")
                            .accessibility_label("Refresh files")
                            .on_click(cx.listener(|this, _, _, cx| this.project_refresh(cx))),
                    ),
            )
            .child(scroll("project-tree").child(list))
            .into_any_element()
    }

    /// Visible rows under `dir`: its entries, and the open folders' below them.
    fn walk(&self, dir: &Path, depth: usize, rows: &mut Vec<(Entry, usize)>) {
        let state = &self.side.project;
        let Some(entries) = state.children.get(dir) else {
            return;
        };
        for entry in entries {
            if !state.show_ignored
                && self
                    .repo_rel(&entry.path)
                    .is_some_and(|r| state.ignored.contains(&r))
            {
                continue;
            }
            rows.push((entry.clone(), depth));
            if entry.dir && state.expanded.contains(&entry.path) {
                self.walk(&entry.path, depth + 1, rows);
            }
        }
    }
}
