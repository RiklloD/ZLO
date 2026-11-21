//! A dock's segmented tab switcher: its labeled pills in rows, plugin
//! tabs after them, and the one raised highlight that glides to the active
//! pill.

use rune::{Color, Cursor, El, Transition, col, el, icon, row, text};

use super::super::ui::{self, xs_med, xs_semi};
use super::super::{Dock, Tab, badge, bridge, dock_tab, side};
use super::WIDE_TABS_MIN;
use crate::design::{motion, styles::Style};
use crate::state::State;

/// Full-width tactile segmented tab switcher mounted at the top of each dock.
///
/// - `Dock::Work`: the project tools (`Changes`, `Files`, `Outputs` /
///   `History`, `Console`) with live count badges.
/// - `Dock::Side`: `Activity`, `Browser`, `Agents` / `Routines`, `Machines`,
///   plus a dedicated `Side chat` pill whenever a side chat is open.
///
/// Both lay their five pills out 3 + 2, on one row once the dock is wider
/// than [`WIDE_TABS_MIN`].
pub(super) fn switcher(s: &State, dock: Dock, w: f32) -> El {
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
    // The active pill's place in the well, found as the rows are laid out
    // below: the gliding highlight sits there.
    let mut glide: Option<[f32; 3]> = None;
    let mut rows = 0usize;
    let place = |rows: usize, ix: usize, width: f32| {
        let at = |n: usize, step: f32| WELL_PAD + n as f32 * (step + PILL_GAP);
        [at(ix, width), at(rows, PILL_H), width]
    };
    let tools: &[Tab] = match dock {
        Dock::Work => &Tab::WORK,
        Dock::Side => {
            if let Some(chat) = side(s).and_then(|x| x.chat.as_ref()) {
                let extra =
                    bridge::title(s, &chat.thread_id).filter(|t| !t.is_empty() && t != "Side chat");
                let on = current == Tab::SideChat;
                if on {
                    glide = Some(place(rows, 0, inner));
                }
                well = well.child(tab_pill(s, Tab::SideChat, on, extra, inner));
                rows += 1;
            }
            &Tab::SIDE_TOOLS
        }
    };
    // One row once every pill fits whole, else rows of three.
    let per_row = if w >= WIDE_TABS_MIN { tools.len() } else { 3 };
    for chunk in tools.chunks(per_row) {
        let mut r = row().w_full().gap(PILL_GAP).items_center();
        for (ix, &t) in chunk.iter().enumerate() {
            if t == current {
                glide = Some(place(rows, ix, per(chunk.len())));
            }
            r = r.child(tab_pill(s, t, t == current, None, per(chunk.len())));
        }
        well = well.child(r);
        rows += 1;
    }
    // Plugins' tabs follow the app's own in the right pane, in rows of the
    // same width.
    if dock == Dock::Side {
        let slots: Vec<&zlo_client::plugins::Slot> = s
            .plugins
            .of(zlo_client::plugins::SlotKind::PanelTab)
            .collect();
        let shown = super::super::plugin_tab(s);
        for chunk in slots.chunks(per_row) {
            let mut r = row().w_full().gap(PILL_GAP).items_center();
            for (ix, &slot) in chunk.iter().enumerate() {
                let on = current == Tab::Plugin && shown == Some(slot);
                if on {
                    glide = Some(place(rows, ix, per(chunk.len())));
                }
                r = r.child(plugin_pill(s, slot, on));
            }
            well = well.child(r);
            rows += 1;
        }
    }
    if let Some([x, y, width]) = glide {
        well = well.child(active_pill(s, dock, x, y, width));
    }

    el().w_full().px(10.).pt(4.).pb(8.).child(well)
}

/// Padding inside a switcher's well, the gap between its pills and their
/// height: the active highlight's place is computed from them.
const WELL_PAD: f32 = 3.;
const PILL_GAP: f32 = 3.;
const PILL_H: f32 = 30.;

/// The active tab's raised highlight, one element per dock behind the
/// pills: a switch glides it to the new tab on [`motion::PILL`] (a dock
/// resizing carries it with the pills on the resize's spring) instead of
/// one pill dropping its fill as the next gains it. It comes last in the
/// well (absolute, so the pills keep their places) and paints under them.
fn active_pill(s: &State, dock: Dock, x: f32, y: f32, w: f32) -> El {
    let p = &s.palette;
    el().key(format!("side-{}-tab-glide", dock.key()))
        .absolute(x, y)
        .size(w, PILL_H)
        .radius(7.)
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
        )
        .paint_order(-1)
        .a11y_hidden()
        .transition(Transition::none())
        .motion(motion::PILL)
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
    // The raised highlight is the switcher's gliding `active_pill`.
    if !active {
        pill = pill.hover(p.hover);
    }
    pill.on_press(move |s: &mut State| super::super::show_plugin(s, &ext, &id))
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
    let count = super::super::tab_count(s, tab);
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

    // The raised highlight is the switcher's gliding `active_pill`.
    if !active {
        pill = pill.hover(p.hover);
    }

    pill.on_press(move |s: &mut State| super::super::show(s, tab))
}
