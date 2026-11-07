//! Drawing the two docks and their titlebar strip (GPUI `side.rs`'s
//! `render_docks`, `render_dock`, `render_titlebar_actions`,
//! `render_dock_header`, `render_dock_tabs`, `render_no_folder`).
//!
//! GPUI eased each dock open and shut by hand (`panels::reveal`); here the
//! dock's clip changes width and Rune's layout spring does the slide, the
//! column inside keeping its full width so it doesn't rewrap.

use rune::{Color, Cursor, Cx, El, Phase, col, el, icon, row};
use rune_kit::Kit;

use super::ui::{self, XS, XS_MED};
use super::{
    Dock, KEEP, Tab, WIDE_SHARE, WIDTH_DEFAULT, WIDTH_MAX, WIDTH_MIN, badge, bridge, dock_open,
    dock_tab, pick_project_folder, session, short_path, side, side_mut,
};
use crate::icons;
use crate::state::State;

/// Width of one titlebar toggle's slot (GPUI `panels::TOGGLE_SIZE + 16`).
const TOGGLE: f32 = 28.;
const SLOT: f32 = TOGGLE + 16.;

/// The window's width, read from the main column's last layout (it reaches
/// the window's right edge). The first frame has none and assumes the
/// default window.
// PORT-STUB(area B): reads the workspace's `main` key; keep it or hand the
// viewport width in.
fn viewport(cx: &Cx<State>) -> f32 {
    cx.window_layout("window").map_or(1280., |r| r[2])
}

/// `dock`'s width at rest: saved or default, or widened, leaving the
/// conversation its room (beside the right pane, for the work panel).
fn rest_width(s: &State, dock: Dock, viewport: f32) -> f32 {
    let room = match dock {
        Dock::Work => {
            let side = if dock_open(s, Dock::Side) { width(s, Dock::Side, viewport) } else { 0. };
            viewport - KEEP - side
        }
        // An open work panel keeps at least its minimum.
        Dock::Side => {
            let work = if dock_open(s, Dock::Work) { WIDTH_MIN } else { 0. };
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
    if side(s).is_some_and(|x| x.wide[dock.ix()]) { wide } else { normal }
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
    let w = |dock| if dock_open(s, dock) { width(s, dock, viewport) } else { 0. };
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
    let w = width(s, dock, viewport);
    // The column keeps its full width inside a clip that opens and shuts
    // around it, so it slides in without rewrapping.
    let clip = el()
        .key(format!("side-{}-dock", dock.key()))
        .h_full()
        .w(if open { w } else { 0. })
        .clip_rounded(0.);
    if !open {
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
        // A tinted sheet, a step darker than the transcript, so the column
        // reads as its own layer.
        .fill(p.surface.alpha(0.45))
        .child(title(s, dock, tab))
        .child(ui::fill_col().child(body))
        .child(el().pin(Some(0.), Some(0.), None, Some(0.)).w(1.).fill(p.border))
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
        .child(el().w(1.).h_full().fill(if dragging { p.accent.alpha(0.6) } else { Color::TRANSPARENT }))
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
        .child(ui::line(tab.label(), XS_MED, p.text));
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
                    .child(ui::line(wt.branch.clone(), XS, p.text_muted))
                    .help(format!("Own git worktree {} on branch {}", wt.path, wt.branch)),
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
                    .child(ui::line(short_path(folder), XS, p.text_faint).shrink())
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
                ui::SM,
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
    let mut thread = row().gap(4.).pr(8.).items_center();
    if let Some(shown) = bridge::computer_share(s) {
        thread = thread.child(ui::selected(
            s,
            ui::ibtn(&icons::MONITOR, "Computer use screen share")
                .on_press(bridge::toggle_computer)
                .build(k),
            shown,
        ));
    }
    thread = thread.child(session::toggle(s, k, layer));
    let tint = |right: f32, width: f32| {
        el().pin(None, Some(0.), Some(right), Some(0.))
            .w(width)
            .fill(p.surface.alpha(0.45))
            .child(el().pin(Some(0.), Some(0.), None, Some(0.)).w(1.).fill(p.border))
    };
    let work_open = dock_open(s, Dock::Work);
    let side_open = dock_open(s, Dock::Side);
    let work_toggle = toggle(
        s,
        "side-work-toggle",
        work_open,
        if work_open { "Hide changes and files" } else { "Changes, files and outputs" },
        icon(&icons::FOLDER_TREE, 15.).color(if work_open { p.text } else { p.text_muted }),
    )
    .on_press(|s: &mut State| super::toggle_work_panel(s));
    let side_toggle = toggle(
        s,
        "side-pane-toggle",
        side_open,
        if side_open { "Hide pane" } else { "Activity, browser and side chat" },
        pane_glyph(if side_open { p.text } else { p.text_muted }),
    )
    .on_press(|s: &mut State| super::toggle_right_pane(s));
    let toggle_at = |right: f32, t: El| {
        row().pin(None, Some(0.), Some(right + 8.), Some(0.)).items_center().child(t)
    };
    let mut strip = el().key("side-titlebar-block").h_full().w(block).clip_rounded(0.);
    if ww > SLOT {
        strip = strip.child(tint(sw, ww));
    }
    if sw > SLOT {
        strip = strip.child(tint(0., sw));
    }
    if ww > 0. {
        let width = (sw + ww - work_right - SLOT).max(0.);
        strip = strip.child(header(s, k, Dock::Work, sw + ww - width, width));
    }
    if sw > 0. {
        strip = strip.child(header(s, k, Dock::Side, SLOT, (sw - SLOT).max(0.)));
    }
    strip = strip
        .child(toggle_at(work_right, work_toggle))
        .child(toggle_at(0., side_toggle));
    row().h_full().items_center().child(thread).child(strip)
}

/// A titlebar toggle (GPUI `panels::toggle`): a square ghost button that
/// holds the active fill while its panel shows.
pub(super) fn toggle(s: &State, key: &str, on: bool, tip: &str, glyph: El) -> El {
    let p = &s.palette;
    row()
        .key(key.to_string())
        .w(TOGGLE)
        .h(TOGGLE)
        .center()
        .radius(crate::design::CONTROL_RADIUS)
        .fill(if on { p.active } else { Color::TRANSPARENT })
        .hover(p.hover)
        .cursor(Cursor::Pointer)
        .role("button")
        .label(tip.to_string())
        .help(tip.to_string())
        .child(glyph)
}

/// The right-pane glyph: a window outline with its right column filled.
fn pane_glyph(ink: Color) -> El {
    row()
        .w(15.)
        .h(13.)
        .radius(3.)
        .border(1.5, ink)
        .justify_end()
        .child(el().w(5.).h_full().fill(ink.alpha(0.6)))
}

/// A dock's titlebar strip: its tabs and widen button, `width` wide with its
/// left edge `right + width` from the block's right.
fn header(s: &State, k: &Kit, dock: Dock, right: f32, width: f32) -> El {
    let wide = side(s).is_some_and(|x| x.wide[dock.ix()]);
    row()
        .key(format!("side-{}-header", dock.key()))
        .pin(None, Some(0.), Some(right), Some(0.))
        .w(width)
        .pl(8.)
        .gap(4.)
        .items_center()
        .child(
            el().key(format!("side-{}-tabs", dock.key()))
                .grow(1.)
                .min_w(0.)
                .scroll_x()
                .child(tabs(s, k, dock)),
        )
        .child(
            ui::ibtn(
                if wide { &icons::MINIMIZE } else { &icons::MAXIMIZE },
                if wide { "Narrow" } else { "Widen" },
            )
            .on_press(move |s: &mut State| {
                if let Some(x) = side_mut(s) {
                    x.wide[dock.ix()] = !x.wide[dock.ix()];
                }
            })
            .build(k)
            .key(format!("side-{}-wide", dock.key())),
        )
}

/// A dock's tab icons, each with its attention dot. The selection fill
/// moves between them on Rune's colour spring (Zeron's tab slide).
fn tabs(s: &State, k: &Kit, dock: Dock) -> El {
    let p = &s.palette;
    let current = dock_tab(s, dock);
    let has_chat = side(s).is_some_and(|x| x.chat.is_some());
    let mut strip = row().gap(2.).items_center();
    for t in dock.tabs().iter().copied().filter(|t| *t != Tab::SideChat || has_chat) {
        let mut cell = el()
            .key(format!("side-tab-{}", t.id()))
            .radius(crate::design::CONTROL_RADIUS)
            .fill(if t == current { p.active } else { Color::TRANSPARENT })
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
