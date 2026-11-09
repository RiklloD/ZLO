//! Drawing the two docks and their titlebar strip (GPUI `side.rs`'s
//! `render_docks`, `render_dock`, `render_titlebar_actions`,
//! `render_dock_header`, `render_dock_tabs`, `render_no_folder`).
//!
//! GPUI eased each dock open and shut by hand (`panels::reveal`); here the
//! dock's clip changes width and Rune's layout spring does the slide, the
//! column inside keeping its full width so it doesn't rewrap. The dock's
//! titlebar tabs sit in a clip of their own that opens and shuts with the
//! column's, so header and body slide as one. Closing, both stay drawn at
//! full opacity until the clips have shut ([`CLOSE_HOLD`], `side::poll`):
//! the clip is the reveal, nothing fades.
//!
//! Everything in the titlebar block is keyed and always mounted, so opening
//! a dock never shifts a sibling's identity (an unkeyed wrapper after a
//! conditional child would remount, and the toggle inside it fly in anew).

use rune::{Color, Cursor, Cx, El, Phase, Transition, col, el, icon, row};
use rune_kit::Kit;

use super::ui::{self, xs, xs_med};
use super::{
    Dock, KEEP, Tab, WIDE_SHARE, WIDTH_DEFAULT, WIDTH_MAX, WIDTH_MIN, badge, bridge, dock_drawn,
    dock_open, dock_tab, pick_project_folder, session, short_path, side, side_mut,
};
use crate::design::{motion, panels};
use crate::icons;
use crate::state::State;

/// Width of one titlebar toggle's slot (GPUI `panels::TOGGLE_SIZE + 16`).
const SLOT: f32 = panels::TOGGLE_SIZE + 16.;

/// How long a closed dock's column stays drawn: the layout spring
/// (`tok::motion::DEFAULT`, 0.3 s) has the clip within a point of shut by
/// then.
pub(super) const CLOSE_HOLD: f64 = 0.6;

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
    let normal = if saved > 0. { saved } else { WIDTH_DEFAULT }
        .clamp(WIDTH_MIN, WIDTH_MAX.min(room.max(WIDTH_MIN)));
    let wide = (viewport * WIDE_SHARE).clamp(WIDTH_MIN, room.max(WIDTH_MIN));
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

/// The right pane's and the work panel's widths, 0 when closed (GPUI
/// `dock_sizes`, settled), for the wallpaper's conversation area.
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
        // The clip reveals it: whole the frame it mounts, and gone only
        // once the clip has shut.
        .transition(Transition::none())
        // A tinted sheet, a step darker than the transcript, so the column
        // reads as its own layer.
        .fill(p.surface.alpha(0.45))
        .child(title(s, dock, tab))
        .child(ui::fill_col().child(body))
        .child(
            el().pin(Some(0.), Some(0.), None, Some(0.))
                .w(1.)
                .fill(p.border),
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
            p.accent.alpha(0.6)
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
                    let max = (right - KEEP).clamp(WIDTH_MIN, WIDTH_MAX);
                    let to = (right - d.window_point()[0]).clamp(WIDTH_MIN, max);
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

/// The dock's title row: the tab's name, and for coding tabs the worktree
/// branch and the project folder (click to pick another).
fn title(s: &State, dock: Dock, tab: Tab) -> El {
    let p = &s.palette;
    let mut title = row()
        .w_full()
        .px(12.)
        .pt(8.)
        .pb(4.)
        .gap(8.)
        .items_center()
        .child(ui::line(tab.label(), xs_med(), p.text));
    let Some(x) = side(s) else { return title };
    if tab.coding() {
        if let Some(wt) = &x.worktree {
            title = title.child(
                row()
                    .key(format!("side-{}-worktree", dock.key()))
                    .gap(2.)
                    .items_center()
                    .max_w(160.)
                    .child(icon(&icons::GIT_BRANCH, 12.).color(p.text_muted))
                    .child(ui::line(wt.branch.clone(), xs(), p.text_muted))
                    .help(format!(
                        "Own git worktree {} on branch {}",
                        wt.path, wt.branch
                    )),
            );
        }
        if let Some(folder) = &x.folder {
            title = title.child(
                row()
                    .key(format!("side-{}-folder", dock.key()))
                    .min_w(0.)
                    .shrink()
                    .gap(4.)
                    .items_center()
                    .cursor(Cursor::Pointer)
                    .child(ui::line(short_path(folder), xs(), p.text_faint).shrink())
                    .child(icon(&icons::CHEVRON_DOWN, 12.).color(p.text_faint))
                    .help("Choose the project folder")
                    .on_press_tx(|_: &mut State, tx| pick_project_folder(tx)),
            );
        }
    }
    if tab == Tab::SideChat {
        title = title.child(session::chat_actions(s));
    }
    title
}

/// A git or shell tab while the backend is on another computer.
pub(super) fn remote_unavailable(s: &State, tab: Tab) -> El {
    let p = &s.palette;
    ui::fill_col()
        .items_center()
        .justify_center()
        .gap(12.)
        .padding(24.)
        .child(icon(&icons::FOLDER_GIT, 28.).color(p.text_faint))
        .child(
            ui::para(
                remote_line(tab, &crate::remote::computer_label(s)),
                ui::sm(),
                p.text_muted,
            )
            .align(rune::Align::Center),
        )
        .key("side-remote-unavailable")
}

/// "Changes: Runs on studio — not available remotely yet".
pub(super) fn remote_line(tab: Tab, computer: &str) -> String {
    format!(
        "{}: {}",
        tab.label(),
        crate::remote::unavailable_line(computer)
    )
}

/// A coding tab before the thread has a project folder.
pub(super) fn no_folder(s: &State, k: &Kit, tab: Tab) -> El {
    let p = &s.palette;
    let mut col = ui::fill_col()
        .items_center()
        .justify_center()
        .gap(12.)
        .padding(24.)
        .child(icon(&icons::FOLDER_GIT, 28.).color(p.text_faint))
        .child(
            ui::para(
                format!(
                    "{} works on a folder on this Mac. Pick the project this conversation is about.",
                    tab.label()
                ),
                ui::sm(),
                p.text_muted,
            )
            .align(rune::Align::Center),
        )
        .child(
            ui::primary("Choose folder…")
                .icon(&icons::FOLDER_PLUS)
                .on_press_tx(|_: &mut State, tx| pick_project_folder(tx))
                .build(k)
                .key("side-pick-folder"),
        );
    if let Some(last) = s.prefs.last_project.clone() {
        let label = format!("Use {}", short_path(&last));
        col = col.child(
            ui::btn(label)
                .on_press(move |s: &mut State| super::set_folder(s, last.clone(), true))
                .build(k)
                .key("side-last-folder"),
        );
    }
    col
}

/// The thread's end of the window titlebar, Zeron's three toggles: the
/// session card, the work panel, the right pane. Each dock's toggle sits at
/// its column's top right, and when it opens its tabs slide in to the
/// toggle's left on the column's tint, so the column reads as one
/// full-height sheet. Closed, the toggles sit side by side.
pub fn titlebar_actions(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let viewport = viewport(cx);
    let (sw, ww) = dock_widths(s, viewport);
    // The block spans both columns, and always room for both toggles.
    let block = (ww + sw).max(sw.max(SLOT) + SLOT);
    // The work toggle sits at the work column's right, never on the right
    // pane's toggle.
    let work_right = sw.max(SLOT);
    let mut thread = row()
        .key("side-titlebar-thread")
        .gap(4.)
        .pr(8.)
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
    thread = thread.child(session::toggle(s, k, layer));
    // A column's tint, `width` wide (0 while shut) and `right` from the
    // block's right edge.
    let tint = |dock: Dock, right: f32, width: f32| {
        el().key(format!("side-{}-tint", dock.key()))
            .pin(None, Some(0.), Some(right), Some(0.))
            .w(width)
            .clip_rounded(0.)
            .fill(p.surface.alpha(0.45))
            .child(
                el().pin(Some(0.), Some(0.), None, Some(0.))
                    .w(1.)
                    .fill(p.border),
            )
    };
    let work_open = dock_open(s, Dock::Work);
    let side_open = dock_open(s, Dock::Side);
    let work_toggle = panels::toggle_with(
        s,
        "side-work-toggle",
        if work_open {
            "Hide changes and files"
        } else {
            "Changes, files and outputs"
        },
        icon(&icons::FOLDER_TREE, panels::GLYPH_PX)
            .color(p.text_muted)
            .group_color(p.text),
    )
    .on_press(|s: &mut State| super::toggle_work_panel(s));
    let side_toggle = panels::toggle(
        s,
        "side-pane-toggle",
        side_open,
        true,
        if side_open {
            "Hide pane"
        } else {
            "Activity, browser and side chat"
        },
    )
    .on_press(|s: &mut State| super::toggle_right_pane(s));
    let toggle_at = |key: &str, right: f32, t: El| {
        row()
            .key(key.to_string())
            .pin(None, Some(0.), Some(right + 8.), Some(0.))
            .items_center()
            .child(t)
    };
    // The tabs' widths open, for the strips still drawn while they shut.
    let sw_open = width(s, Dock::Side, viewport);
    let ww_open = width(s, Dock::Work, viewport);
    let work_head = (sw + ww - work_right - SLOT).max(0.);
    let work_full = (sw + ww_open - work_right - SLOT).max(0.);
    let side_head = (sw - SLOT).max(0.);
    let side_full = (sw_open - SLOT).max(0.);
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
            work_right + SLOT,
            work_head,
            work_full,
        ))
        .child(header(s, k, Dock::Side, SLOT, side_head, side_full))
        .child(toggle_at(
            "side-work-toggle-slot",
            work_right,
            active(s, work_toggle, work_open),
        ))
        .child(toggle_at(
            "side-pane-toggle-slot",
            0.,
            active(s, side_toggle, side_open),
        ));
    row()
        .key("side-titlebar")
        .h_full()
        .items_center()
        .child(thread)
        .child(strip)
}

/// A titlebar toggle's active wash while its panel shows (the workspace's
/// sidebar button does the same over `panels::toggle`).
pub(super) fn active(s: &State, toggle: El, on: bool) -> El {
    toggle.fill(if on {
        s.palette.active
    } else {
        Color::TRANSPARENT
    })
}

/// A dock's titlebar strip: its tabs and widen button in a clip `width`
/// wide (0 while shut) with its right edge `right` from the block's right.
/// The strip inside keeps its open width `full` and rides the clip's left
/// edge, as the column below rides its own clip.
fn header(s: &State, k: &Kit, dock: Dock, right: f32, width: f32, full: f32) -> El {
    let clip = el()
        .key(format!("side-{}-header", dock.key()))
        .pin(None, Some(0.), Some(right), Some(0.))
        .w(width)
        .clip_rounded(0.);
    if !dock_drawn(s, dock) {
        return clip;
    }
    let wide = side(s).is_some_and(|x| x.wide[dock.ix()]);
    clip.child(
        row()
            .key(format!("side-{}-header-row", dock.key()))
            .absolute(0., 0.)
            .w(full)
            .h_full()
            .pl(8.)
            .gap(4.)
            .items_center()
            .transition(Transition::none())
            .child(
                el().key(format!("side-{}-tabs", dock.key()))
                    .grow(1.)
                    .min_w(0.)
                    .scroll_x()
                    .child(tabs(s, k, dock)),
            )
            .child(
                ui::ibtn(
                    if wide {
                        &icons::MINIMIZE
                    } else {
                        &icons::MAXIMIZE
                    },
                    if wide { "Narrow" } else { "Widen" },
                )
                .on_press(move |s: &mut State| {
                    if let Some(x) = side_mut(s) {
                        x.wide[dock.ix()] = !x.wide[dock.ix()];
                    }
                })
                .build(k)
                .key(format!("side-{}-wide", dock.key())),
            ),
    )
}

/// A dock's tab icons, each with its attention dot. One selection pill
/// glides between them on [`motion::PILL`] (GPUI's `flip` pill, Zeron's
/// tab slide); the cells carry no fill of their own.
fn tabs(s: &State, k: &Kit, dock: Dock) -> El {
    /// A small icon button's side (rune-kit `ControlSize::Small`) and the
    /// strip's gap: where each cell starts.
    const CELL: f32 = 20.;
    const GAP: f32 = 2.;
    let p = &s.palette;
    let current = dock_tab(s, dock);
    let has_chat = side(s).is_some_and(|x| x.chat.is_some());
    let shown: Vec<Tab> = dock
        .tabs()
        .iter()
        .copied()
        .filter(|t| *t != Tab::SideChat || has_chat)
        .collect();
    let mut strip = row().gap(GAP).items_center();
    if let Some(ix) = shown.iter().position(|t| *t == current) {
        strip = strip.child(
            el().key(format!("side-{}-tab-pill", dock.key()))
                .absolute(ix as f32 * (CELL + GAP), 0.)
                .size(CELL, CELL)
                .radius(crate::design::CONTROL_RADIUS)
                .fill(p.active)
                .a11y_hidden()
                .motion(motion::PILL),
        );
    }
    for t in shown {
        let mut cell = el()
            .key(format!("side-tab-{}", t.id()))
            .radius(crate::design::CONTROL_RADIUS)
            .child(
                ui::ibtn(t.icon(), t.label())
                    .on_press(move |s: &mut State| super::show(s, t))
                    .build(k),
            );
        if badge(s, t) {
            cell = cell.child(ui::dot(p.accent, 6.).absolute(20., 3.));
        }
        strip = strip.child(cell);
    }
    strip
}

#[cfg(test)]
mod tests {
    use rune::{Env, Runtime};

    use super::super::{Dock, dock_drawn, poll, toggle_right_pane};
    use crate::state::State;

    fn blank(_: &State, _: &mut rune::Cx<State>) -> rune::El {
        rune::el()
    }

    /// While the backend is remote, the git and shell tabs say where they
    /// run, Files stays (read through the gateway), and no folder on this
    /// Mac is adopted for the thread.
    #[test]
    fn remote_backend_disables_the_local_only_tabs() {
        use super::super::{Tab, set_folder};
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
