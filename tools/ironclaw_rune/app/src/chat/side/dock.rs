//! Drawing the two docks and their titlebar strip: the work panel
//! (`Changes`, `Files`, `Outputs`, `History`, `Console`) beside the
//! conversation and the right pane (`Activity`, `Browser`, `Agents`,
//! `Routines`, `Machines`, plus an open side chat) at the window's edge.
//!
//! Layout design:
//! - The titlebar holds one fixed top-right toggle capsule (`Session`, `Work`,
//!   `Side`) whose buttons never move as columns open or close, and beside it a
//!   single context/actions strip for each open dock (project folder chip,
//!   active branch/host, tab actions, widen). That eliminates the old duplicate
//!   title row and frees the titlebar from tiny unlabeled tab icons.
//! - Each dock opens directly with a full-width, tactile segmented tab switcher
//!   (five labeled pills with live counts per dock, 3 + 2 or one row when
//!   wide), giving 1-click direct access with zero wasted space.
//! - Each dock slides via a clipped width animation (`panels::REVEAL`) while
//!   its inner column keeps its full width so nothing rewraps mid-slide.

use std::collections::HashSet;
use std::path::PathBuf;

use rune::{
    Color, Cursor, Cx, El, Phase, Spring, SpringPair, Transition, col, el, icon, row, text,
};
use rune_kit::Kit;

use super::ui::{self, xs_med, xs_semi};
use super::{
    Dock, KEEP, Tab, WIDE_SHARE, WIDTH_DEFAULT, WIDTH_MAX, WIDTH_MIN, bridge, browser, dock_badge,
    dock_drawn, dock_open, dock_tab, pick_project_folder, session, short_path, side, side_mut,
};
use crate::design::{panels, styles::Style};
use crate::icons;
use crate::state::State;

/// Gap between buttons inside the toggle capsule.
const TAB_GAP: f32 = 2.;
/// Outer margin around the toggle capsule and leading header padding.
const CLUSTER_PAD: f32 = 8.;
/// Inner horizontal padding of the toggle capsule.
const CAPSULE_PAD: f32 = 3.;
/// Width of the rounded toggle capsule holding the three dock toggles.
const CAPSULE_W: f32 = 3. * panels::TOGGLE_SIZE + 2. * TAB_GAP + 2. * CAPSULE_PAD;
/// Total horizontal space reserved on the right of the titlebar for toggles.
const CLUSTER: f32 = CAPSULE_W + 2. * CLUSTER_PAD;

/// Minimum width of the right pane when the window has room, keeping all 3
/// labeled pills per row crisp without truncation.
const SIDE_MIN: f32 = 300.;
/// Width above which a dock's five tool tabs flow onto a single row.
const WIDE_TABS_MIN: f32 = 560.;

/// How long a closed dock's column stays drawn: until its clip is within
/// half a point of shut. The clip rides [`panels::REVEAL`] whoever closes
/// it: the shell's split view sets that spring on the whole main column
/// (`workspace/shell/frame.rs`), and the chat's own buttons name it too
/// (`side::dock_slide`). Measured for the widest column a window gives
/// ([`WIDE_SHARE`] of a 2560 pt screen). Holding longer only draws frames
/// nobody can see; less would drop the column while its clip still shows.
pub(super) fn close_hold() -> f64 {
    const WIDEST: f32 = 2560. * WIDE_SHARE;
    f64::from(panels::REVEAL.settling_duration(0.5 / WIDEST))
}

/// A dock's narrowest width with `room` left for it.
fn min_width(dock: Dock, room: f32) -> f32 {
    match dock {
        Dock::Work => WIDTH_MIN,
        Dock::Side => SIDE_MIN.min(room).max(WIDTH_MIN),
    }
}

/// The window's width, read from the workspace shell's `window` element's
/// last layout. The first frame has none and assumes the default window.
fn viewport(cx: &Cx<State>) -> f32 {
    cx.window_layout("window").map_or(1280., |r| r[2])
}

/// `dock`'s width at rest: saved or default, or widened, leaving the
/// conversation its room (beside the right pane, for the work panel).
fn rest_width(s: &State, dock: Dock, viewport: f32) -> f32 {
    let room = match dock {
        Dock::Work => {
            let side = if dock_open(s, Dock::Side) {
                width(s, Dock::Side, viewport)
            } else {
                0.
            };
            viewport - KEEP - side
        }
        // An open work panel keeps at least its minimum.
        Dock::Side => {
            let work = if dock_open(s, Dock::Work) {
                WIDTH_MIN
            } else {
                0.
            };
            viewport - KEEP - work
        }
    };
    let saved = match dock {
        Dock::Work => s.prefs.work_width,
        Dock::Side => s.prefs.side_width,
    };
    let normal = if saved > 0. { saved } else { WIDTH_DEFAULT };
    let min = min_width(dock, room);
    let normal = normal.clamp(min, WIDTH_MAX.min(room.max(min)));
    let wide = (viewport * WIDE_SHARE).clamp(min, room.max(min));
    if side(s).is_some_and(|x| x.wide[dock.ix()]) {
        wide
    } else {
        normal
    }
}

/// A dock's laid-out width: the dragged width while its edge is dragged.
fn width(s: &State, dock: Dock, viewport: f32) -> f32 {
    match side(s).and_then(|x| x.drag) {
        Some((d, w)) if d == dock => w,
        _ => rest_width(s, dock, viewport),
    }
}

/// The right pane's and the work panel's widths, 0 when closed, for the
/// wallpaper's conversation area.
pub fn dock_widths(s: &State, viewport: f32) -> (f32, f32) {
    let w = |dock| {
        if dock_open(s, dock) {
            width(s, dock, viewport)
        } else {
            0.
        }
    };
    (w(Dock::Side), w(Dock::Work))
}

/// High-contrast layered surface fill shared by a dock's titlebar tint and
/// body sheet so the column reads clearly over both plain themes and wallpapers.
fn dock_fill(s: &State) -> Color {
    let st = Style::of(&s.prefs);
    let alpha = if st.vibrancy { 0.76 } else { 0.94 };
    s.palette.surface.alpha(alpha)
}

/// Both docks, work panel first, for the chat's row.
pub fn docks(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    vec![work_view(s, cx, k, layer), view(s, cx, k, layer)]
}

/// The right pane at the window's edge.
pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    dock(s, cx, k, layer, Dock::Side)
}

/// The work panel beside the conversation.
pub fn work_view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    dock(s, cx, k, layer, Dock::Work)
}

fn dock(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>, dock: Dock) -> El {
    let viewport = viewport(cx);
    let open = dock_open(s, dock) && side(s).is_some();
    let drawn = dock_drawn(s, dock) && side(s).is_some();
    let w = width(s, dock, viewport);
    // The column keeps its full width inside a clip that opens and shuts
    // around it, so it slides in without rewrapping.
    let clip = el()
        .key(format!("side-{}-dock", dock.key()))
        .h_full()
        .w(if open { w } else { 0. })
        .clip_rounded(0.);
    if !drawn {
        return clip;
    }
    let p = &s.palette;
    let tab = dock_tab(s, dock);
    let body = super::body(s, cx, k, layer, tab);
    let dragging = side(s).and_then(|x| x.drag).is_some_and(|(d, _)| d == dock);
    let pane = col()
        .key(format!("side-{}-pane", dock.key()))
        .absolute(0., 0.)
        .w(w)
        .h_full()
        .transition(Transition::none())
        .fill(dock_fill(s))
        // The pane's own parts never fade: they leave with the clip that
        // shuts over them, not inside it after it is already closed.
        .child(switcher(s, dock, w).transition(Transition::none()))
        .child({
            let rule = el()
                .w_full()
                .h(1.)
                .fill(p.border)
                .transition(Transition::none());
            // The Browser's load bar rides this rule, above the page (a
            // native view would hide it anywhere over the page itself).
            match browser::progress_bar(s, w).filter(|_| tab == Tab::Browser) {
                Some(bar) => rule.child(bar),
                None => rule,
            }
        })
        .child(
            ui::fill_col().transition(Transition::none()).child(
                // Keyed by tab, so a switch fades the new tab's body in
                // over the old one's quicker exit, and nothing re-fires
                // while the tab stays.
                ui::fill_col()
                    .key(format!("side-{}-body-{}", dock.key(), tab.id()))
                    .transition(body_fade())
                    .child(body),
            ),
        )
        .child(
            el().pin(Some(0.), Some(0.), None, Some(0.))
                .w(1.)
                .fill(p.border)
                .transition(Transition::none()),
        )
        .child(edge(s, dock, w, dragging, viewport));
    clip.child(pane)
}

/// A tab body's entrance and exit on a switch: a short fade in over a
/// shorter fade out. Rune turns it into its reduced fade under reduced
/// motion.
fn body_fade() -> Transition {
    Transition::opacity().springs(SpringPair::new(Spring::new(0.18, 0.), Spring::new(0.1, 0.)))
}

/// The resize strip along the dock's left border. A double click resets
/// the saved width.
fn edge(s: &State, dock: Dock, w: f32, dragging: bool, viewport: f32) -> El {
    let p = &s.palette;
    // The work panel's right edge is the right pane's left one.
    let right = match dock {
        Dock::Work if dock_open(s, Dock::Side) => viewport - width(s, Dock::Side, viewport),
        _ => viewport,
    };
    let st = Style::of(&s.prefs);
    // The 1 pt line fades in under the pointer as a hint that the border
    // drags, and holds the accent while it does.
    el().key(format!("side-{}-edge", dock.key()))
        .pin(Some(0.), Some(0.), None, Some(0.))
        .w(6.)
        .cursor(Cursor::ResizeColumn)
        .hover_group()
        .hover_motion(st.hover, st.hover)
        .child(
            el().key(format!("side-{}-edge-line", dock.key()))
                .w(1.)
                .h_full()
                .fill(if dragging {
                    p.accent.alpha(0.75)
                } else {
                    p.border_strong
                })
                .group_opacity(if dragging { 1. } else { 0. }, 1.),
        )
        .on_drag(move |s: &mut State, d, tx| {
            // A double click springs back to the default width with the
            // columns beside it; the drag itself follows the pointer.
            if d.phase == Phase::Down && d.clicks >= 2 {
                super::dock_slide(s, tx);
                if let Some(x) = side_mut(s) {
                    x.drag = None;
                    x.opened_at[dock.ix()] = Some(std::time::Instant::now());
                }
                s.update_prefs(|p| match dock {
                    Dock::Work => p.work_width = 0.,
                    Dock::Side => p.side_width = 0.,
                });
                return;
            }
            tx.instant();
            match d.phase {
                Phase::Down => {
                    if let Some(x) = side_mut(s) {
                        x.wide[dock.ix()] = false;
                        x.drag = Some((dock, w));
                    }
                }
                Phase::Move => {
                    // The same room `rest_width` leaves, so release does not jump.
                    let min = min_width(dock, right - KEEP);
                    let max = (right - KEEP).clamp(min, WIDTH_MAX);
                    let to = (right - d.window_point()[0]).clamp(min, max);
                    if let Some(x) = side_mut(s)
                        && x.drag.is_some()
                    {
                        x.drag = Some((dock, to));
                    }
                }
                Phase::Up => {
                    if let Some((d, to)) = side_mut(s).and_then(|x| x.drag.take()) {
                        s.update_prefs(|p| match d {
                            Dock::Work => p.work_width = to,
                            Dock::Side => p.side_width = to,
                        });
                    }
                }
            }
        })
}

mod switcher;
use switcher::switcher;

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
        super::dock_slide(s, tx);
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
    for action in super::tab_actions(s, k, tab) {
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
    } else if let Some((line, tone)) = super::tab_status(s, tab) {
        let color = match tone {
            super::Tone::Live => p.accent,
            super::Tone::Warn => p.warning,
            super::Tone::Bad => p.danger,
            super::Tone::Good => p.success,
            super::Tone::Quiet => p.text_faint,
        };
        let mut dot = ui::dot(color, 7.);
        if tone == super::Tone::Live {
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

/// A git or shell tab while the backend is on another computer.
pub(super) fn remote_unavailable(s: &State, tab: Tab) -> El {
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
pub(super) fn remote_line(tab: Tab, computer: &str) -> String {
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
pub(super) fn no_folder(s: &State, k: &Kit, tab: Tab) -> El {
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
                    .on_press(move |s: &mut State| super::set_folder(s, target.clone(), true)),
            );
        }
        stack = stack.child(group);
    }

    ui::scroll("side-no-folder", stack)
}

/// The thread's end of the window titlebar. Its three toggles (the session
/// card, the work panel, the right pane) sit inside one framed glass capsule at
/// the window's top right and never move: opening or closing a column leaves
/// every toggle under the pointer. Each open column's context/actions strip
/// rides the titlebar above it on the column's sheet fill.
pub fn titlebar_actions(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let viewport = viewport(cx);
    let window_h = cx.window_layout("window").map_or(820., |r| r[3]);
    let (sw, ww) = dock_widths(s, viewport);
    // The block spans both columns, and always has room for the toggles.
    let block = (ww + sw).max(CLUSTER);
    let mut thread = row()
        .key("side-titlebar-thread")
        .gap(TAB_GAP)
        .pr(4.)
        .items_center();
    if let Some(shown) = bridge::computer_share(s) {
        thread = thread.child(
            ui::selected(
                s,
                ui::ibtn(&icons::MONITOR, "Computer use screen share")
                    .on_press(bridge::toggle_computer)
                    .build(k),
                shown,
            )
            .key("side-computer-share"),
        );
    }
    // A column's tint, `width` wide (0 while shut) and `right` from the
    // block's right edge.
    let sheet = dock_fill(s);
    let tint = |dock: Dock, right: f32, width: f32| {
        el().key(format!("side-{}-tint", dock.key()))
            .pin(None, Some(0.), Some(right), Some(0.))
            .w(width)
            .clip_rounded(0.)
            .fill(sheet)
            .child(
                el().pin(Some(0.), Some(0.), None, Some(0.))
                    .w(1.)
                    .fill(p.border),
            )
    };
    let work_open = dock_open(s, Dock::Work);
    let side_open = dock_open(s, Dock::Side);
    let mut work_toggle = panels::toggle_with(
        s,
        "side-work-toggle",
        if work_open {
            "Hide workspace (⌘E)"
        } else {
            "Workspace · Changes, Files, Outputs (⌘E)"
        },
        panels::work_glyph(
            "side-work-toggle-glyph",
            work_open,
            panels::GLYPH_PX,
            if work_open { p.text } else { p.text_muted },
        )
        .group_color(p.text),
    )
    .on_press_tx(|s: &mut State, tx| {
        super::dock_slide(s, tx);
        super::toggle_work_panel(s);
    });
    if dock_badge(s, Dock::Work) {
        work_toggle = work_toggle.child(ui::dot(p.accent, 6.).absolute(19., 3.));
    }

    let mut side_toggle = panels::toggle_with(
        s,
        "side-pane-toggle",
        if side_open {
            "Hide tools pane (⌥⌘B)"
        } else {
            "Tools · Activity, Browser, Agents, Console (⌥⌘B)"
        },
        panels::glyph(
            "side-pane-toggle-glyph",
            side_open,
            true,
            panels::GLYPH_PX,
            if side_open { p.text } else { p.text_muted },
        )
        .group_color(p.text),
    )
    .on_press_tx(|s: &mut State, tx| {
        super::dock_slide(s, tx);
        super::toggle_right_pane(s);
    });
    if dock_badge(s, Dock::Side) {
        side_toggle = side_toggle.child(ui::dot(p.accent, 6.).absolute(19., 3.));
    }

    let capsule_bg = if p.light {
        Color::BLACK.alpha(0.04)
    } else {
        Color::WHITE.alpha(0.05)
    };
    let toggles = row()
        .key("side-toggles")
        .pin(None, Some(0.), Some(0.), Some(0.))
        .px(CLUSTER_PAD)
        .items_center()
        .child(
            row()
                .h(panels::TOGGLE_SIZE + 6.)
                .px(CAPSULE_PAD)
                .gap(TAB_GAP)
                .items_center()
                .radius(10.)
                .fill(capsule_bg)
                .border(1., p.border)
                .no_window_drag()
                .child(session::toggle(s, k, layer, window_h))
                .child(active(s, work_toggle, work_open))
                .child(active(s, side_toggle, side_open)),
        );

    // The headers' widths open, for the strips still drawn while they shut.
    let sw_open = width(s, Dock::Side, viewport);
    let ww_open = width(s, Dock::Work, viewport);
    // The work panel's strip ends at the right pane, or at the toggles when
    // it is the column at the window's edge.
    let (work_right, work_room) = if sw > 0. {
        (sw, 0.)
    } else {
        (CLUSTER, CLUSTER)
    };
    let strip = el()
        .key("side-titlebar-block")
        .h_full()
        .w(block)
        .clip_rounded(0.)
        .child(tint(Dock::Work, sw, ww))
        .child(tint(Dock::Side, 0., sw))
        .child(header(
            s,
            k,
            Dock::Work,
            work_right,
            (ww - work_room).max(0.),
            (ww_open - work_room).max(0.),
        ))
        .child(header(
            s,
            k,
            Dock::Side,
            CLUSTER,
            (sw - CLUSTER).max(0.),
            (sw_open - CLUSTER).max(0.),
        ))
        .child(toggles);
    row()
        .key("side-titlebar")
        .h_full()
        // Its toggles are all pinned: this keeps the row a button tall.
        .min_h(panels::TOGGLE_SIZE + 6.)
        .items_center()
        .child(thread)
        .child(strip)
}

/// A titlebar toggle's active highlight while its panel shows. It fades
/// in and out on the selection spring rather than snapping; the off state
/// keeps a clear ring so the border fades instead of vanishing.
pub(super) fn active(s: &State, toggle: El, on: bool) -> El {
    let p = &s.palette;
    let toggle = toggle.motion(Style::of(&s.prefs).slide);
    if on {
        toggle
            .fill(if p.light { p.surface_card } else { p.active })
            .border(1., p.border_strong)
    } else {
        toggle
            .fill(Color::TRANSPARENT)
            .border(1., Color::TRANSPARENT)
    }
}

/// A dock's titlebar strip: its context and actions in a clip `width` wide
/// (0 while shut) with its right edge `right` from the block's right. The strip
/// inside keeps its open width `full` and rides the clip's left edge, as the
/// column below rides its own clip.
fn header(s: &State, k: &Kit, dock: Dock, right: f32, width: f32, full: f32) -> El {
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

#[cfg(test)]
mod tests;
