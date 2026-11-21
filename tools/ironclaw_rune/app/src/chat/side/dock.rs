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

use rune::{Color, Cursor, Cx, El, Phase, Transition, col, el, icon, row, text};
use rune_kit::Kit;

use super::ui::{self, xs_med, xs_semi};
use super::{
    Dock, KEEP, Tab, WIDE_SHARE, WIDTH_DEFAULT, WIDTH_MAX, WIDTH_MIN, badge, bridge, browser,
    dock_badge, dock_drawn, dock_open, dock_tab, pick_project_folder, session, short_path, side,
    side_mut,
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

/// How long a closed dock's column stays drawn: the layout spring
/// (`tok::motion::DEFAULT`) has the clip within half a point of shut by
/// then — its `settling_duration(0.5 / SIDE_MIN)`, about 0.43 s. Holding
/// longer only draws frames nobody can see.
pub(super) const CLOSE_HOLD: f64 = 0.43;

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
        .child(
            el().w_full()
                .h(1.)
                .fill(p.border)
                .transition(Transition::none()),
        )
        .child(ui::fill_col().transition(Transition::none()).child(body))
        .child(
            el().pin(Some(0.), Some(0.), None, Some(0.))
                .w(1.)
                .fill(p.border)
                .transition(Transition::none()),
        )
        .child(edge(s, dock, w, dragging, viewport));
    clip.child(pane)
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
    el().key(format!("side-{}-edge", dock.key()))
        .pin(Some(0.), Some(0.), None, Some(0.))
        .w(6.)
        .cursor(Cursor::ResizeColumn)
        .child(el().w(1.).h_full().fill(if dragging {
            p.accent.alpha(0.75)
        } else {
            Color::TRANSPARENT
        }))
        .on_drag(move |s: &mut State, d, tx| {
            tx.instant();
            match d.phase {
                Phase::Down if d.clicks >= 2 => {
                    if let Some(x) = side_mut(s) {
                        x.drag = None;
                    }
                    s.update_prefs(|p| match dock {
                        Dock::Work => p.work_width = 0.,
                        Dock::Side => p.side_width = 0.,
                    });
                }
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

/// Full-width tactile segmented tab switcher mounted at the top of each dock.
///
/// - `Dock::Work`: the project tools (`Changes`, `Files`, `Outputs` /
///   `History`, `Console`) with live count badges.
/// - `Dock::Side`: `Activity`, `Browser`, `Agents` / `Routines`, `Machines`,
///   plus a dedicated `Side chat` pill whenever a side chat is open.
///
/// Both lay their five pills out 3 + 2, on one row once the dock is wider
/// than [`WIDE_TABS_MIN`].
fn switcher(s: &State, dock: Dock, w: f32) -> El {
    let p = &s.palette;
    let current = dock_tab(s, dock);
    let well_bg = if p.light {
        Color::BLACK.alpha(0.045)
    } else {
        Color::BLACK.alpha(0.24)
    };
    let mut well = col()
        .key(format!("side-{}-tabs", dock.key()))
        .w_full()
        .padding(3.)
        .gap(3.)
        .radius(10.)
        .fill(well_bg)
        .border(1., p.border);

    // Outer px(10) + well padding(3) on both sides, 3pt gaps between pills.
    let inner = (w - 26.).max(0.);
    let per = |n: usize| (inner - 3. * (n as f32 - 1.)) / n as f32;
    let tools: &[Tab] = match dock {
        Dock::Work => &Tab::WORK,
        Dock::Side => {
            if let Some(chat) = side(s).and_then(|x| x.chat.as_ref()) {
                let extra =
                    bridge::title(s, &chat.thread_id).filter(|t| !t.is_empty() && t != "Side chat");
                well = well.child(tab_pill(
                    s,
                    Tab::SideChat,
                    current == Tab::SideChat,
                    extra,
                    inner,
                ));
            }
            &Tab::SIDE_TOOLS
        }
    };
    // One row once every pill fits whole, else rows of three.
    let per_row = if w >= WIDE_TABS_MIN { tools.len() } else { 3 };
    for chunk in tools.chunks(per_row) {
        let mut r = row().w_full().gap(3.).items_center();
        for &t in chunk {
            r = r.child(tab_pill(s, t, t == current, None, per(chunk.len())));
        }
        well = well.child(r);
    }
    // Plugins' tabs follow the app's own in the right pane, in rows of the
    // same width.
    if dock == Dock::Side {
        let slots: Vec<&zlo_client::plugins::Slot> = s
            .plugins
            .of(zlo_client::plugins::SlotKind::PanelTab)
            .collect();
        let shown = super::plugin_tab(s);
        for chunk in slots.chunks(per_row) {
            let mut r = row().w_full().gap(3.).items_center();
            for &slot in chunk {
                let on = current == Tab::Plugin && shown == Some(slot);
                r = r.child(plugin_pill(s, slot, on));
            }
            well = well.child(r);
        }
    }

    el().w_full().px(10.).pt(4.).pb(8.).child(well)
}

/// A plugin's `panel.tab` slot as a pill in the right dock's switcher: the
/// same shape and states as [`tab_pill`], named by the slot's own title.
fn plugin_pill(s: &State, slot: &zlo_client::plugins::Slot, active: bool) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let (ext, id) = (slot.extension_id.clone(), slot.slot_id.clone());
    let mut pill = row()
        .key(format!("side-tab-plugin-{ext}-{id}"))
        .role("button")
        .label(slot.title.clone())
        .help(slot.title.clone())
        .focusable(true)
        .focus_ring(2., 2.5, crate::design::kit(p).c.border_focus)
        .grow(1.)
        .min_w(0.)
        .h(30.)
        .px(7.)
        .gap(5.)
        .items_center()
        .justify_center()
        .radius(7.)
        .cursor(Cursor::Pointer)
        .hover_group()
        .hover_motion(st.hover, st.hover)
        .child(
            icon(crate::icons::by_name(slot.icon.as_deref()), 14.5)
                .color(if active { p.accent } else { p.text_muted })
                .group_color(if active { p.accent } else { p.text }),
        )
        .child(
            text(slot.title.clone())
                .font(if active { xs_semi() } else { xs_med() })
                .color(if active { p.text } else { p.text_muted })
                .group_color(p.text)
                .truncate()
                .shrink()
                .min_w(0.),
        );
    if active {
        pill = pill
            .fill(if p.light {
                p.surface_card
            } else {
                p.surface_raised
            })
            .border(1., p.border_strong)
            .shadow(
                1.,
                3.,
                Color::BLACK.alpha(if p.light { 0.08 } else { 0.22 }),
            );
    } else {
        pill = pill.hover(p.hover);
    }
    pill.on_press(move |s: &mut State| super::show_plugin(s, &ext, &id))
}

/// Below this pill width a count badge replaces the icon, so the label
/// ("Outputs 4") stays whole instead of truncating to "Out…".
const PILL_ICON_WITH_COUNT_MIN: f32 = 118.;

/// One labeled, keyboard-focusable pill inside a dock's segmented switcher.
/// `width` is the pill's laid-out width.
fn tab_pill(s: &State, tab: Tab, active: bool, subtitle: Option<String>, width: f32) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let label_text = match subtitle {
        Some(sub) => format!("{} · {sub}", tab.label()),
        None => tab.label().to_string(),
    };
    let count = super::tab_count(s, tab);
    let has_badge = badge(s, tab);
    let icon_color = if active { p.accent } else { p.text_muted };
    let text_color = if active { p.text } else { p.text_muted };

    let mut pill = row()
        .key(format!("side-tab-{}", tab.id()))
        .role("button")
        .label(tab.label())
        .help(tab.label())
        .focusable(true)
        .focus_ring(2., 2.5, crate::design::kit(p).c.border_focus)
        .grow(1.)
        .min_w(0.)
        .h(30.)
        .px(7.)
        .gap(5.)
        .items_center()
        .justify_center()
        .radius(7.)
        .cursor(Cursor::Pointer)
        .hover_group()
        .hover_motion(st.hover, st.hover);
    if count.is_none() || width >= PILL_ICON_WITH_COUNT_MIN {
        pill = pill.child(
            icon(tab.icon(), 14.5)
                .color(icon_color)
                .group_color(if active { p.accent } else { p.text }),
        );
    }
    pill = pill.child(
        text(label_text)
            .font(if active { xs_semi() } else { xs_med() })
            .color(text_color)
            .group_color(p.text)
            .truncate()
            .shrink()
            .min_w(0.),
    );

    if let Some(n) = count {
        pill = pill.child(ui::count_pill(s, n, active, has_badge));
    } else if has_badge {
        pill = pill.child(ui::dot(p.accent, 6.));
    }

    if active {
        pill = pill
            .fill(if p.light {
                p.surface_card
            } else {
                p.surface_raised
            })
            .border(1., p.border_strong)
            .shadow(
                1.,
                3.,
                Color::BLACK.alpha(if p.light { 0.08 } else { 0.22 }),
            );
    } else {
        pill = pill.hover(p.hover);
    }

    pill.on_press(move |s: &mut State| super::show(s, tab))
}

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
    .on_press(move |s: &mut State| {
        if let Some(x) = side_mut(s) {
            x.wide[dock.ix()] = !x.wide[dock.ix()];
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
            "Hide workspace (⌘J)"
        } else {
            "Workspace · Changes, Files, Outputs (⌘J)"
        },
        panels::work_glyph(
            "side-work-toggle-glyph",
            work_open,
            panels::GLYPH_PX,
            if work_open { p.text } else { p.text_muted },
        )
        .group_color(p.text),
    )
    .on_press(|s: &mut State| super::toggle_work_panel(s));
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
    .on_press(|s: &mut State| super::toggle_right_pane(s));
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

/// A titlebar toggle's active highlight while its panel shows.
pub(super) fn active(s: &State, toggle: El, on: bool) -> El {
    let p = &s.palette;
    if on {
        toggle
            .fill(if p.light { p.surface_card } else { p.active })
            .border(1., p.border_strong)
    } else {
        toggle.fill(Color::TRANSPARENT)
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
mod tests {
    use rune::{Env, Runtime};

    use super::super::{Dock, Tab, dock_drawn, poll, toggle_right_pane, toggle_work_panel};
    use crate::state::State;

    fn blank(_: &State, _: &mut rune::Cx<State>) -> rune::El {
        rune::el()
    }

    /// While the backend is remote, the git and shell tabs say where they
    /// run, Files stays (read through the gateway), and no folder on this
    /// Mac is adopted for the thread.
    #[test]
    fn remote_backend_disables_the_local_only_tabs() {
        use super::super::set_folder;
        let off: Vec<Tab> = [
            Tab::Changes,
            Tab::Files,
            Tab::Outputs,
            Tab::History,
            Tab::Console,
        ]
        .into_iter()
        .filter(|t| t.local_only())
        .collect();
        assert_eq!(off, vec![Tab::Changes, Tab::History, Tab::Console]);
        assert_eq!(
            super::remote_line(Tab::Console, "studio"),
            "Console: Runs on studio — not available remotely yet"
        );

        let mut s = crate::chat::fixtures::test_state();
        s.chat = Some(crate::chat::ChatState::new("t1".into()));
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
        set_folder(
            &mut s,
            std::path::PathBuf::from("/Users/you/code/zlo"),
            false,
        );
        assert!(s.chat.as_ref().is_some_and(|c| c.side.folder.is_none()));
    }

    /// The titlebar's toggles hold their place whichever columns are open:
    /// a click that opens or closes one never moves another out from under
    /// the pointer. Each column's header stops short of the toggles.
    #[test]
    fn the_titlebar_toggles_never_move() {
        const TOGGLES: [&str; 3] = [
            "side-session-toggle",
            "side-work-toggle",
            "side-pane-toggle",
        ];
        let mut s = crate::chat::fixtures::test_state();
        crate::scenes::chat(&mut s);
        s.prefs.side_open = false;
        s.prefs.work_open = false;
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        let mut t = 0.;
        let mut settle = |rt: &mut Runtime<State>| {
            for _ in 0..90 {
                t += 1. / 60.;
                rt.frame(t);
                rt.update(|s, tx| {
                    poll(s, t, tx);
                });
            }
        };
        let place = |rt: &Runtime<State>| -> Vec<[f32; 2]> {
            TOGGLES
                .iter()
                .map(|k| {
                    let (r, _) = rt.presented(k).expect("toggle drawn");
                    [r[0], r[1]]
                })
                .collect()
        };
        settle(&mut rt);
        let closed = place(&rt);
        assert!(
            closed.windows(2).all(|w| w[0][0] < w[1][0]),
            "session, work, pane from left to right: {closed:?}"
        );
        for (step, flip) in [
            ("pane open", toggle_right_pane as fn(&mut State)),
            ("both open", toggle_work_panel),
            ("work only", toggle_right_pane),
            ("both closed", toggle_work_panel),
        ] {
            rt.update(|s, _| flip(s));
            settle(&mut rt);
            assert_eq!(place(&rt), closed, "{step}: the toggles moved");
            if let Some((tabs, _)) = rt.presented("side-side-header") {
                let (pane, _) = rt.presented("side-session-toggle").expect("drawn");
                assert!(
                    tabs[0] + tabs[2] <= pane[0],
                    "{step}: the pane's header runs under the toggles"
                );
            }
        }
        // The tabs and toggles are buttons the keyboard reaches.
        rt.update(|s, _| toggle_right_pane(s));
        settle(&mut rt);
        let elements = rt.elements();
        for key in TOGGLES.iter().copied().chain([
            "side-tab-activity",
            "side-tab-browser",
            "side-tab-agents",
            "side-tab-routines",
            "side-tab-machines",
        ]) {
            let e = elements
                .iter()
                .find(|e| e.key.as_deref() == Some(key))
                .expect("drawn");
            assert!(e.focusable, "{key} is not focusable");
        }
    }

    /// Both docks' segmented tab switchers draw every tab pill with generous
    /// width inside their own dock bounds and with zero overlap.
    #[test]
    fn both_docks_draw_non_overlapping_labeled_tab_pills() {
        let mut s = crate::chat::fixtures::test_state();
        crate::scenes::chat(&mut s);
        s.prefs.work_open = true;
        s.prefs.side_open = true;
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..90 {
            t += 1. / 60.;
            rt.frame(t);
            rt.update(|s, tx| {
                poll(s, t, tx);
            });
        }
        let (work_box, _) = rt.presented("side-work-pane").expect("work pane drawn");
        let (side_box, _) = rt.presented("side-side-pane").expect("side pane drawn");
        // Each dock: five pills, three on the first row and two below.
        let docks = [
            (
                work_box,
                ["side-tab-changes", "side-tab-files", "side-tab-outputs"],
                ["side-tab-history", "side-tab-console"],
            ),
            (
                side_box,
                ["side-tab-activity", "side-tab-browser", "side-tab-agents"],
                ["side-tab-routines", "side-tab-machines"],
            ),
        ];
        for (pane, top, bottom) in docks {
            let row1: Vec<[f32; 4]> = top.iter().map(|k| rt.presented(k).expect(k).0).collect();
            let row2: Vec<[f32; 4]> = bottom.iter().map(|k| rt.presented(k).expect(k).0).collect();
            for row in [&row1, &row2] {
                for r in row.iter() {
                    assert!(r[2] >= 70., "pill should be generously wide: {r:?}");
                    assert!(
                        r[0] >= pane[0] && r[0] + r[2] <= pane[0] + pane[2] + 0.5,
                        "pill {r:?} escapes its pane {pane:?}"
                    );
                    assert!(
                        (r[2] - row[0][2]).abs() <= 1.0 && (r[1] - row[0][1]).abs() <= 0.5,
                        "a row's pills share width and line: {row:?}"
                    );
                }
                for pair in row.windows(2) {
                    assert!(
                        pair[0][0] + pair[0][2] <= pair[1][0] + 0.5,
                        "pills overlap: {row:?}"
                    );
                }
            }
            assert!(
                row1[0][1] + row1[0][3] <= row2[0][1] + 0.5,
                "the 2 rows do not overlap"
            );
        }

        // Widening the side dock collapses its 5 tool pills onto a single row.
        rt.update(|s, _| {
            if let Some(x) = super::side_mut(s) {
                x.wide[Dock::Side.ix()] = true;
            }
        });
        for _ in 0..90 {
            t += 1. / 60.;
            rt.frame(t);
            rt.update(|s, tx| {
                poll(s, t, tx);
            });
        }
        let wide_pills: Vec<[f32; 4]> = [
            "side-tab-activity",
            "side-tab-browser",
            "side-tab-agents",
            "side-tab-routines",
            "side-tab-machines",
        ]
        .into_iter()
        .map(|k| rt.presented(k).expect(k).0)
        .collect();
        for pair in wide_pills.windows(2) {
            assert!(
                (pair[0][1] - pair[1][1]).abs() <= 0.5,
                "wide pills should share one row: {wide_pills:?}"
            );
            assert!(
                pair[0][0] + pair[0][2] <= pair[1][0] + 0.5,
                "wide pills overlap: {wide_pills:?}"
            );
        }
    }

    /// In a narrow window with both columns open the right pane gives up
    /// its roomy minimum before it pushes the toggles past the window's
    /// edge.
    #[test]
    fn a_narrow_window_keeps_the_toggles_inside() {
        let mut s = crate::chat::fixtures::test_state();
        crate::scenes::chat(&mut s);
        s.prefs.side_open = true;
        s.prefs.work_open = true;
        let width = 820.;
        let mut rt = Runtime::new(s, crate::view, (width, 760.), Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..90 {
            t += 1. / 60.;
            rt.frame(t);
            rt.update(|s, tx| {
                poll(s, t, tx);
            });
        }
        for key in [
            "side-session-toggle",
            "side-work-toggle",
            "side-pane-toggle",
        ] {
            let (r, _) = rt.presented(key).expect("toggle drawn");
            assert!(
                r[0] >= 0. && r[0] + r[2] <= width,
                "{key} at {r:?} is outside the {width} pt window"
            );
        }
    }

    /// A closed dock stays drawn (so the clip, not a fade, hides it) until
    /// `CLOSE_HOLD` has passed on the app clock, and a reopen mid-hold
    /// keeps it.
    #[test]
    fn a_closed_dock_is_drawn_until_its_clip_has_shut() {
        let mut s = crate::chat::fixtures::test_state();
        s.chat = Some(crate::chat::ChatState::new("t1".into()));
        s.prefs.side_open = true;
        let mut rt = Runtime::new(s, blank, (100., 100.), Env::new(true, 1.));
        rt.update(|s, tx| {
            poll(s, 0., tx);
        });
        rt.update(|s, _| toggle_right_pane(s));
        assert!(!rt.state.prefs.side_open);
        rt.update(|s, tx| {
            poll(s, 1., tx);
        });
        assert!(dock_drawn(&rt.state, Dock::Side), "held while sliding");
        rt.update(|s, tx| {
            poll(s, 1. + super::CLOSE_HOLD / 2., tx);
        });
        assert!(dock_drawn(&rt.state, Dock::Side), "still sliding");
        let mut changed = false;
        rt.update(|s, tx| changed = poll(s, 1. + super::CLOSE_HOLD, tx));
        assert!(changed, "the drop commits");
        assert!(!dock_drawn(&rt.state, Dock::Side), "gone once shut");
        // Closed, reopened mid-hold and closed again: the first close's
        // deadline must not drop the second close's slide.
        rt.update(|s, _| toggle_right_pane(s));
        rt.update(|s, tx| {
            poll(s, 5., tx);
        });
        rt.update(|s, _| toggle_right_pane(s));
        rt.update(|s, tx| {
            poll(s, 5.1, tx);
        });
        rt.update(|s, _| toggle_right_pane(s));
        rt.update(|s, tx| {
            poll(s, 5.2, tx);
        });
        rt.update(|s, _| toggle_right_pane(s));
        rt.update(|s, tx| {
            poll(s, 5.3, tx);
        });
        rt.update(|s, tx| {
            poll(s, 5.1 + super::CLOSE_HOLD + 0.05, tx);
        });
        assert!(
            dock_drawn(&rt.state, Dock::Side),
            "a reopen restarts the hold"
        );
    }
}
