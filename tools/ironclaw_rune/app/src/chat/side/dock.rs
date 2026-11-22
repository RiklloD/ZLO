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

use rune::{Color, Cursor, Cx, El, Phase, Spring, SpringPair, Transition, col, el};
use rune_kit::Kit;

use super::ui::{self};
use super::{
    Dock, KEEP, Tab, WIDTH_DEFAULT, WIDTH_MAX, WIDTH_MIN, browser, dock_drawn, dock_open, dock_tab,
    side, side_mut,
};
use crate::design::{panels, styles::Style};
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

/// The widest column a window gives: an expanded dock on a 2560 pt screen.
pub(super) const WIDEST: f32 = 2560.;

/// How long a closed dock's column stays drawn: until its clip is within
/// half a point of shut. The clip rides [`panels::REVEAL`] whoever closes
/// it: the shell's split view sets that spring on the whole main column
/// (`workspace/shell/frame.rs`), and the chat's own buttons name it too
/// (`side::dock_slide`). Measured for the widest column a window gives
/// (an expanded dock across a 2560 pt screen). Holding longer only draws
/// frames nobody can see; less would drop the column while its clip still
/// shows.
pub(super) fn close_hold() -> f64 {
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

/// The width the conversation and both docks share: the `chat-root` row's
/// last layout (the window less the sidebar). The first frame has none and
/// assumes the sidebar's default width.
fn chat_room(cx: &Cx<State>, viewport: f32) -> f32 {
    cx.window_layout("chat-root")
        .map_or(viewport - 240., |r| r[2])
}

/// The dock expanded over the conversation, if any. Only one is: the work
/// panel wins a stale pair.
pub fn expanded(s: &State) -> Option<Dock> {
    let x = side(s)?;
    [Dock::Work, Dock::Side]
        .into_iter()
        .find(|d| x.wide[d.ix()] && dock_open(s, *d))
}

/// `dock`'s width at rest: saved or default, leaving the conversation its
/// room (beside the right pane, for the work panel); or, expanded, all of
/// `chat` (the conversation's and both docks' row) the other dock leaves.
fn rest_width(s: &State, dock: Dock, viewport: f32, chat: f32) -> f32 {
    if expanded(s) != Some(dock) {
        return normal_width(s, dock, viewport);
    }
    let other = match dock {
        Dock::Work => Dock::Side,
        Dock::Side => Dock::Work,
    };
    let taken = if dock_open(s, other) {
        dragged_or(s, other, || normal_width(s, other, viewport))
    } else {
        0.
    };
    (chat - taken).max(WIDTH_MIN)
}

/// `dock`'s width when not expanded: saved or default, leaving the
/// conversation its room (beside the right pane's own normal width, for
/// the work panel). Never reads an expanded width, so an expanded dock
/// can size itself from its neighbour's.
fn normal_width(s: &State, dock: Dock, viewport: f32) -> f32 {
    let room = match dock {
        Dock::Work => {
            let side = if dock_open(s, Dock::Side) {
                dragged_or(s, Dock::Side, || normal_width(s, Dock::Side, viewport))
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
    normal.clamp(min, WIDTH_MAX.min(room.max(min)))
}

/// The dragged width while `dock`'s edge is dragged, else `rest`.
fn dragged_or(s: &State, dock: Dock, rest: impl FnOnce() -> f32) -> f32 {
    match side(s).and_then(|x| x.drag) {
        Some((d, w)) if d == dock => w,
        _ => rest(),
    }
}

/// A dock's laid-out width: the dragged width while its edge is dragged.
fn width(s: &State, dock: Dock, viewport: f32, chat: f32) -> f32 {
    dragged_or(s, dock, || rest_width(s, dock, viewport, chat))
}

/// The right pane's and the work panel's widths, 0 when closed, for the
/// wallpaper's conversation area.
pub fn dock_widths(s: &State, viewport: f32, chat: f32) -> (f32, f32) {
    let w = |dock| {
        if dock_open(s, dock) {
            width(s, dock, viewport, chat)
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
    let chat = chat_room(cx, viewport);
    let open = dock_open(s, dock) && side(s).is_some();
    let drawn = dock_drawn(s, dock) && side(s).is_some();
    let w = width(s, dock, viewport, chat);
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
        .child(edge(s, dock, w, dragging, viewport, chat));
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
fn edge(s: &State, dock: Dock, w: f32, dragging: bool, viewport: f32, chat: f32) -> El {
    let p = &s.palette;
    // The work panel's right edge is the right pane's left one.
    let right = match dock {
        Dock::Work if dock_open(s, Dock::Side) => viewport - width(s, Dock::Side, viewport, chat),
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
                        // Resizing leaves expanded mode, either dock's: an
                        // expanded neighbour leaves no conversation to
                        // resize against.
                        x.wide = [false; 2];
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

mod empty;
mod header;
mod switcher;
mod titlebar;
pub(super) use empty::{no_folder, remote_unavailable};
use switcher::switcher;
pub(super) use titlebar::active;
pub use titlebar::titlebar_actions;

#[cfg(test)]
mod tests;
