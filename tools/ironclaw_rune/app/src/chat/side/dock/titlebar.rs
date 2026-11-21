//! The thread's end of the window titlebar: the fixed toggle capsule and
//! each open dock's strip beside it.

use rune::{Color, Cx, El, el, row};
use rune_kit::Kit;

use super::super::ui::{self};
use super::super::{Dock, bridge, dock_badge, dock_open, session};
use super::*;
use crate::design::{panels, styles::Style};
use crate::icons;
use crate::state::State;

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
    .on_press_tx(|s: &mut State, tx| {
        super::super::dock_slide(s, tx);
        super::super::toggle_work_panel(s);
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
        super::super::dock_slide(s, tx);
        super::super::toggle_right_pane(s);
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
        .child(super::header::header(
            s,
            k,
            Dock::Work,
            work_right,
            (ww - work_room).max(0.),
            (ww_open - work_room).max(0.),
        ))
        .child(super::header::header(
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
pub(in crate::chat::side) fn active(s: &State, toggle: El, on: bool) -> El {
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
