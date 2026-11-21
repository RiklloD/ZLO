//! A dock's titlebar strip: the folder or worktree chip, the tab's
//! context and actions, and widen.

use rune::{Cursor, El, Transition, el, icon, row};
use rune_kit::Kit;

use super::super::ui::{self, xs_med, xs_semi};
use super::super::{
    Dock, Tab, browser, dock_drawn, dock_tab, pick_project_folder, session, short_path, side,
    side_mut,
};
use crate::icons;
use crate::state::State;

/// Clickable project folder chip rendered in the titlebar when a coding tab
/// has a folder attached.
fn folder_chip(s: &State, dock: Dock, folder: &std::path::Path) -> El {
    let p = &s.palette;
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| short_path(folder));
    row()
        .key(format!("side-{}-folder", dock.key()))
        .role("button")
        .label(format!("Project folder {name}"))
        .help(format!(
            "{} — click to switch project folder",
            short_path(folder)
        ))
        .min_w(0.)
        .shrink()
        .h(24.)
        .px(7.)
        .gap(5.)
        .items_center()
        .radius(6.)
        .fill(p.hover)
        .hover(p.active)
        .hover_motion(crate::design::motion::HOVER, crate::design::motion::HOVER)
        .press_scale(crate::design::motion::PRESS)
        .border(1., p.border)
        .cursor(Cursor::Pointer)
        .no_window_drag()
        .child(icon(&icons::FOLDER, 12.5).color(p.text_muted))
        .child(ui::line(name, xs_med(), p.text).shrink())
        .child(icon(&icons::CHEVRON_DOWN, 11.).color(p.text_faint))
        .on_press_tx(|_: &mut State, tx| pick_project_folder(tx))
}

/// Compact branch pill for threads bound to their own git worktree.
fn worktree_chip(s: &State, dock: Dock, wt: &zlo_client::orchestration::WorktreeView) -> El {
    let p = &s.palette;
    row()
        .key(format!("side-{}-worktree", dock.key()))
        .min_w(0.)
        .max_w(130.)
        .shrink()
        .h(22.)
        .px(6.)
        .gap(4.)
        .items_center()
        .radius(6.)
        .fill(p.accent_wash)
        .child(icon(&icons::GIT_BRANCH, 11.5).color(p.accent))
        .child(ui::line(wt.branch.clone(), ui::tiny(), p.accent).shrink())
        .help(format!(
            "Own git worktree {} on branch {}",
            wt.path, wt.branch
        ))
}

/// A dock's single unified titlebar header: context on the left (folder chip,
/// branch, active domain, or side-chat controls) and tab actions + widen on
/// the right. Replaces the old duplicate title row inside the pane.
fn header_content(s: &State, k: &Kit, dock: Dock) -> El {
    let p = &s.palette;
    let tab = dock_tab(s, dock);
    let wide = side(s).is_some_and(|x| x.wide[dock.ix()]);

    let widen = ui::ibtn(
        if wide {
            &icons::MINIMIZE
        } else {
            &icons::MAXIMIZE
        },
        if wide { "Narrow pane" } else { "Widen pane" },
    )
    .on_press_tx(move |s: &mut State, tx| {
        super::super::dock_slide(s, tx);
        if let Some(x) = side_mut(s) {
            x.wide[dock.ix()] = !x.wide[dock.ix()];
            // Its native view waits out the resize instead of chasing it.
            x.opened_at[dock.ix()] = Some(std::time::Instant::now());
        }
    })
    .build(k)
    .no_window_drag()
    .key(format!("side-{}-wide", dock.key()));

    let mut actions = row()
        .key(format!("side-{}-actions", dock.key()))
        .gap(2.)
        .items_center()
        .no_window_drag();
    for action in super::super::tab_actions(s, k, tab) {
        actions = actions.child(action.no_window_drag());
    }
    actions = actions.child(widen);

    let mut bar = row()
        .key(format!("side-{}-title", dock.key()))
        .w_full()
        .h_full()
        .pl(10.)
        .pr(6.)
        .gap(6.)
        .items_center();

    if tab == Tab::SideChat {
        bar = bar.child(session::chat_actions(s));
    } else if tab.coding() {
        if let Some(x) = side(s) {
            if let Some(folder) = &x.folder {
                bar = bar.child(folder_chip(s, dock, folder));
                if let Some(wt) = &x.worktree {
                    bar = bar.child(worktree_chip(s, dock, wt));
                }
            } else {
                bar = bar.child(
                    row()
                        .gap(6.)
                        .items_center()
                        .min_w(0.)
                        .shrink()
                        .child(icon(tab.icon(), 13.5).color(p.text_muted))
                        .child(ui::line(
                            match dock {
                                Dock::Work => "Workspace",
                                Dock::Side => tab.label(),
                            },
                            xs_semi(),
                            p.text,
                        ))
                        .child(ui::line("No folder", ui::tiny(), p.text_faint)),
                );
            }
        }
        bar = bar.child(ui::grow());
    } else if tab == Tab::Browser {
        bar = bar.child(browser::bar(s, k));
    } else if let Some((line, tone)) = super::super::tab_status(s, tab) {
        let color = match tone {
            super::super::Tone::Live => p.accent,
            super::super::Tone::Warn => p.warning,
            super::super::Tone::Bad => p.danger,
            super::super::Tone::Good => p.success,
            super::super::Tone::Quiet => p.text_faint,
        };
        let mut dot = ui::dot(color, 7.);
        if tone == super::super::Tone::Live {
            dot = dot.cycle(crate::design::kit::loops::breathe(
                1.2,
                0.,
                &crate::design::kit::loops::BREATH_45,
            ));
        }
        bar = bar
            .child(
                row()
                    .key(format!("side-{}-status", dock.key()))
                    .gap(7.)
                    .items_center()
                    .min_w(0.)
                    .shrink()
                    .child(dot)
                    .child(ui::line(line, xs_semi(), p.text).shrink()),
            )
            .child(ui::grow());
    } else {
        bar = bar
            .child(
                row()
                    .gap(6.)
                    .items_center()
                    .min_w(0.)
                    .shrink()
                    .child(icon(tab.icon(), 13.5).color(p.text_muted))
                    .child(ui::line(tab.label(), xs_semi(), p.text).shrink()),
            )
            .child(ui::grow());
    }

    bar.child(actions)
}

/// A dock's titlebar strip: its context and actions in a clip `width` wide
/// (0 while shut) with its right edge `right` from the block's right. The strip
/// inside keeps its open width `full` and rides the clip's left edge, as the
/// column below rides its own clip.
pub(super) fn header(s: &State, k: &Kit, dock: Dock, right: f32, width: f32, full: f32) -> El {
    let clip = el()
        .key(format!("side-{}-header", dock.key()))
        .pin(None, Some(0.), Some(right), Some(0.))
        .w(width)
        .clip_rounded(0.);
    if !dock_drawn(s, dock) {
        return clip;
    }
    clip.child(
        row()
            .key(format!("side-{}-header-row", dock.key()))
            .absolute(0., 0.)
            .w(full)
            .h_full()
            .items_center()
            .transition(Transition::none())
            .child(header_content(s, k, dock)),
    )
}
