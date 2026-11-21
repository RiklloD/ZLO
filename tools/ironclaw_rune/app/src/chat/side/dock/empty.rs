//! What a coding tab shows before it can work: no project folder yet, or
//! a backend on another computer that does not offer it.

use std::collections::HashSet;
use std::path::PathBuf;

use rune::{Cursor, El, col, el, icon, row, text};
use rune_kit::Kit;

use super::super::ui::{self, xs_semi};
use super::super::{Tab, bridge, pick_project_folder, short_path};
use crate::icons;
use crate::state::State;

/// A git or shell tab while the backend is on another computer.
pub(in crate::chat::side) fn remote_unavailable(s: &State, tab: Tab) -> El {
    ui::fill_col()
        .key("side-remote-unavailable")
        .child(ui::empty_card(
            s,
            &icons::FOLDER_GIT,
            format!("{} runs locally", tab.label()),
            remote_line(tab, &crate::remote::computer_label(s)),
            None,
        ))
}

/// "Changes: Runs on studio — not available remotely yet".
pub(in crate::chat::side) fn remote_line(tab: Tab, computer: &str) -> String {
    format!(
        "{}: {}",
        tab.label(),
        crate::remote::unavailable_line(computer)
    )
}

/// Collect unique recent project folders for one-click attachment:
/// current profile folder, last picked project, and folders used in other
/// threads (up to 5).
fn recent_folders(s: &State) -> Vec<(PathBuf, Option<&'static str>)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |path: PathBuf, tag: Option<&'static str>| {
        if out.len() < 5 && !path.as_os_str().is_empty() && seen.insert(path.clone()) {
            out.push((path, tag));
        }
    };
    if let Some((_, folder, _)) = bridge::profile(s)
        && !folder.trim().is_empty()
    {
        push(PathBuf::from(folder), Some("Profile"));
    }
    if let Some(last) = s.prefs.last_project.clone() {
        push(last, Some("Last used"));
    }
    let mut others: Vec<PathBuf> = s.prefs.project_folders.values().cloned().collect();
    others.sort();
    for path in others {
        push(path, None);
    }
    out
}

/// A coding tab before the thread has a project folder: structured hero card
/// plus one-click recent project cards.
pub(in crate::chat::side) fn no_folder(s: &State, k: &Kit, tab: Tab) -> El {
    let p = &s.palette;
    let pick_btn = ui::primary("Choose folder…")
        .icon(&icons::FOLDER_PLUS)
        .on_press_tx(|_: &mut State, tx| pick_project_folder(tx))
        .build(k)
        .key("side-pick-folder");

    let mut stack = col().w_full().gap(12.).child(ui::empty_card(
        s,
        tab.icon(),
        "Connect a project folder",
        format!(
            "{} reads a folder on this Mac. Pick the project for this thread to inspect diffs, files, git history, and run commands.",
            tab.label()
        ),
        Some(pick_btn),
    ));

    let recent = recent_folders(s);
    if !recent.is_empty() {
        let mut group = col()
            .w_full()
            .px(12.)
            .gap(6.)
            .child(ui::section_label(s, "Quick connect"));
        for (ix, (path, tag)) in recent.into_iter().enumerate() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| short_path(&path));
            let display = short_path(&path);
            let is_last = s.prefs.last_project.as_ref() == Some(&path);
            let key = if is_last {
                "side-last-folder".to_string()
            } else {
                format!("side-recent-folder-{ix}")
            };
            let mut title_row = row()
                .gap(6.)
                .items_center()
                .child(ui::line(name, xs_semi(), p.text).shrink());
            if let Some(tag) = tag {
                title_row = title_row.child(
                    el().h(16.)
                        .px(5.)
                        .radius(5.)
                        .center()
                        .fill(p.hover)
                        .child(text(tag).font(ui::tiny()).color(p.text_muted)),
                );
            }
            let target = path.clone();
            group = group.child(
                row()
                    .key(key)
                    .role("button")
                    .label(format!("Use {display}"))
                    .w_full()
                    .min_h(44.)
                    .px(10.)
                    .py(6.)
                    .gap(10.)
                    .items_center()
                    .radius(10.)
                    .fill(p.surface_card.alpha(if p.light { 0.8 } else { 0.55 }))
                    .border(1., p.border)
                    .hover(p.hover)
                    .hover_group()
                    .cursor(Cursor::Pointer)
                    .child(
                        el().size(28., 28.).radius(7.).center().fill(p.hover).child(
                            icon(&icons::FOLDER, 14.)
                                .color(p.text_muted)
                                .group_color(p.accent),
                        ),
                    )
                    .child(
                        col()
                            .grow(1.)
                            .min_w(0.)
                            .gap(1.)
                            .child(title_row)
                            .child(ui::line(display, ui::tiny(), p.text_faint)),
                    )
                    .child(
                        icon(&icons::ARROW_RIGHT, 14.)
                            .color(p.text_faint)
                            .group_color(p.text),
                    )
                    .on_press(move |s: &mut State| {
                        super::super::set_folder(s, target.clone(), true)
                    }),
            );
        }
        stack = stack.child(group);
    }

    ui::scroll("side-no-folder", stack)
}
