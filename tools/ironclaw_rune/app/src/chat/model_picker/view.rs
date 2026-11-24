//! The model picker's one pill ("model · thinking") and its popover (GPUI
//! `ModelPicker::render`, `render_popover`, `group_header`, `model_row`),
//! with the thinking level as a slider under the models.

use rune::shape::{LineCap, Shape};
use rune::{
    Color, Cx, Cycle, El, Key, Modifier, MotionIntent, Phase, Spring, TextEdit, canvas, col, el,
    row,
};
use rune_kit::{Kit, Placement, RollingText, TextInput, scrim};
use zlo_client::orchestration::RouteProvider;

use super::{
    ModelPickerState, NATIVE, Rail, Slot, cycle_rail, edit_prefs, effort_label, pick, pick_effort,
    pretty_model, refresh_catalog, reorder,
};
use crate::design::kit::compact::{
    ICON_SM, ICON_XS, SM, XS, ghost_icon, ico, panel_surface, spacer, spinner, txt, txt_medium,
};
use crate::design::kit::shortcut_badge;
use crate::design::styles::rem;
use crate::design::{self, Palette};
use crate::icons;
use crate::state::State;

const POPOVER_WIDTH: f32 = 380.;
/// A fixed height on every tab, so switching tabs never resizes the panel;
/// a longer list scrolls.
const POPOVER_HEIGHT: f32 = 344.;
/// Opening below, the panel clears the composer card: the pill's 28 px, the
/// row's 2 px slack, the card's 8 px bottom padding and its rim, then an
/// 8 px gap. Rune anchors from the pill's edge, so the gap is this less 28.
const BELOW_OFFSET: f32 = 47.;
/// Opening above, the gap between the pill and the panel (from its top),
/// until the composer card has a layout to clear.
const ABOVE_OFFSET: f32 = 34.;
/// Opening above, the gap between the composer card's top and the panel.
const ABOVE_GAP: f32 = 8.;
const PILL_H: f32 = 28.;
/// A rail icon plus the rail's gap: one slot of travel when dragging.
const RAIL_STEP: f32 = RAIL_ITEM + 4.;
/// A rail item's square side, and the rail's padding around them.
const RAIL_ITEM: f32 = 30.;
const RAIL_PAD: f32 = 4.;

/// The model pill of the chat showing `thread_id` (the open chat or a side
/// chat), its popover into `layer`.
pub fn pills(s: &State, thread_id: &str, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    pills_for(s, Slot::chat(thread_id), cx, k, layer)
}

/// The pill of the picker `slot` finds (the chat's, or the draft screen's):
/// the model's mark and name, then the thinking level when the model has
/// levels, one control opening one popover.
pub fn pills_for(s: &State, slot: Slot, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let Some(m) = slot.get(s) else { return el() };
    if !m.shows_pill() {
        return el();
    }
    let p = &s.palette;
    let (label, provider) = m.label();
    let (runtime, brand) = m
        .providers()
        .iter()
        .find(|x| Some(&x.provider) == provider.as_ref())
        .map(|x| (x.runtime.clone(), x.brand.clone()))
        .unwrap_or_default();
    let ink = if m.open { p.text } else { p.text_muted };
    let thinking = thinking_label(m);
    let full = match &thinking {
        Some(t) => format!("{label} · {t}"),
        None => label.clone(),
    };
    let mut pill = row()
        .key(slot.key("model-pill"))
        .role("button")
        .label(format!("Model: {full}"))
        .expanded(m.open)
        .gap(6.)
        .px(8.)
        .h(PILL_H)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .fill(if m.open { p.active } else { Color::TRANSPARENT })
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .child(provider_mark(
            s,
            brand.as_deref(),
            &runtime,
            provider.as_deref().unwrap_or(NATIVE),
            14.,
        ));
    if !m.compact {
        // A picked model or level rolls in rather than cutting (the pick's
        // reply asks for motion; the first load lands as it is).
        pill = pill.child(rolling(k, slot.key("model-label"), label.clone(), p.text));
        if let Some(t) = &thinking {
            pill = pill.child(rolling(
                k,
                slot.key("model-thinking-label"),
                format!("· {t}"),
                p.text_faint,
            ));
        }
    }
    if m.busy {
        pill = pill.child(spinner(k, ICON_XS, ink));
    } else if !m.compact {
        // Turns over while the popover is open.
        pill = pill.child(crate::design::kit::chevron(
            "model-chevron",
            m.open,
            crate::design::kit::Turn::Flip,
            ICON_XS,
            ink,
        ));
    }
    if m.compact && !m.open {
        pill = pill.help(full);
    }
    let toggle = slot.clone();
    let pill = pill
        .on_press(move |s: &mut State| {
            if let Some(m) = toggle.get_mut(s) {
                m.toggle();
                if m.open {
                    refresh_catalog(s, toggle.clone());
                }
            }
        })
        .press_scale(design::motion::PRESS);
    let (placement, gap) = placement(m, &slot, cx);
    if m.open {
        let closing = slot.clone();
        layer.push(scrim("model-scrim", move |s: &mut State| {
            close(s, &closing)
        }));
        let popover = model_popover(s, m, &slot, k);
        layer.push(anchor(k, popover, &slot.key("model-pill"), placement, gap));
    }
    // Eases in where the composer first shows it, once the catalog is in
    // (the load's reply asks for motion only for that first appearance).
    row()
        .key(slot.key("model-pills"))
        .center_y()
        .transition(design::motion::pop(4.))
        .child(pill)
}

/// A pill label at the pill's size whose changed characters roll.
fn rolling(k: &Kit, key: String, label: String, color: Color) -> El {
    rune_kit::RollingText::new(key, label)
        .font(rune::TextStyle::new(rem(SM), 400.))
        .color(color)
        .spring(design::motion::ROLL)
        .build(k)
}

/// The popover hung off its pill, as `rune_kit::popover::anchored` hangs a
/// surface (fitted to the window, flipping when it doesn't fit), but without
/// landing every change inside it: the panel pops in and out on its own
/// transition (`panel_surface`), and the rail's pill and the thinking
/// slider's knob glide. The model list lands its changes (its `instant_changes`), so switching tabs
/// or searching swaps it at once.
fn anchor(k: &Kit, content: El, anchor_key: &str, placement: Placement, gap: f32) -> El {
    let side = match placement {
        Placement::TopStart => rune::Side::Top,
        _ => rune::Side::Bottom,
    };
    content
        .anchored_fit(anchor_key, side, rune::Align::Start, gap)
        .motion_intent(rune::MotionIntent::CUT)
        .token_motion(k.motion.overlay)
}

/// The thinking level the pill shows, when the current model has levels.
fn thinking_label(m: &ModelPickerState) -> Option<String> {
    let model = m.current_model().unwrap_or("default");
    if m.efforts_of(m.current_provider(), model).is_empty() {
        return None;
    }
    Some(
        m.effort
            .as_deref()
            .map_or_else(|| "Auto".to_string(), effort_label),
    )
}

fn close(s: &mut State, slot: &Slot) {
    if let Some(m) = slot.get_mut(s) {
        m.close();
    }
}

/// Where a popover hangs off its pill. The new-thread screen centers the
/// composer, so the panel opens below the card and leaves the title visible;
/// in a thread the composer sits at the bottom and it opens above. A
/// compact (side chat) picker opens above and is kept inside the window by
/// the anchor.
///
/// Opening above, the panel clears the whole composer card, not just the
/// pill: hung off the pill alone it covered the text being typed. The card
/// is the chat's (`ChatState::composer_key`); before it has been laid out
/// the panel sits [`ABOVE_OFFSET`] from the pill's top.
fn placement(m: &ModelPickerState, slot: &Slot, cx: &Cx<State>) -> (Placement, f32) {
    if m.compact || m.thread_id.is_some() {
        let pill = cx.window_layout(&slot.key("model-pill"));
        let card = match slot {
            Slot::Chat(id) => cx.window_layout(&format!("composer:{id}")),
            Slot::Fixed(_) => None,
        };
        let gap = match (pill, card) {
            (Some(pill), Some(card)) if card[1] <= pill[1] => pill[1] - card[1] + ABOVE_GAP,
            _ => ABOVE_OFFSET - PILL_H,
        };
        (Placement::TopStart, gap)
    } else {
        (Placement::BottomStart, BELOW_OFFSET - PILL_H)
    }
}

/// The search field's keys: Ctrl+Tab / Ctrl+Shift+Tab walk the rail, Tab /
/// Shift+Tab step the thinking level, ⌘1..⌘9 pick a row, Escape closes, and
/// holding ⌘ numbers the rows.
fn search_key(s: &mut State, slot: &Slot, ev: &rune::KeyEvent) -> bool {
    match &ev.key {
        Key::Tab if ev.mods.ctrl => {
            cycle_rail(s, slot.clone(), !ev.mods.shift);
            true
        }
        Key::Tab if !ev.mods.cmd && !ev.mods.alt => {
            step_thinking(s, slot, !ev.mods.shift);
            true
        }
        Key::Command(c) if !ev.mods.ctrl && !ev.mods.alt && !ev.mods.shift => {
            let Some(ix) = c
                .to_digit(10)
                .filter(|d| (1..=9).contains(d))
                .map(|d| d as usize - 1)
            else {
                return false;
            };
            let (order, recent) = (
                s.prefs.provider_order.clone(),
                s.prefs.recent_models.clone(),
            );
            let picked = slot.get(s).and_then(|m| m.nth_pick(ix, &order, &recent));
            if let Some((provider, model)) = picked {
                pick(s, slot.clone(), provider, model);
            }
            true
        }
        Key::Escape => {
            close(s, slot);
            true
        }
        // ⌘ held here numbers models; releases pass through so a badge
        // shown before the picker opened still clears.
        Key::Modifier {
            modifier: Modifier::SuperLeft | Modifier::SuperRight,
            ..
        } => {
            if let Some(m) = slot.get_mut(s) {
                m.cmd_held = ev.mods.cmd;
            }
            false
        }
        _ => false,
    }
}

/// Move the thinking level one step along the row (Auto, then the model's
/// levels), wrapping; nothing when the model has no levels.
fn step_thinking(s: &mut State, slot: &Slot, forward: bool) {
    let Some(m) = slot.get(s) else { return };
    let choices = thinking_choices(m);
    if choices.len() < 2 {
        return;
    }
    let at = choices.iter().position(|c| *c == m.effort).unwrap_or(0);
    let n = choices.len();
    let next = if forward {
        (at + 1) % n
    } else {
        (at + n - 1) % n
    };
    pick_effort(s, slot.clone(), choices[next].clone());
}

fn model_popover(s: &State, m: &ModelPickerState, slot: &Slot, k: &Kit) -> El {
    let p = &s.palette;
    let order = &s.prefs.provider_order;
    let recent = &s.prefs.recent_models;

    // The selection is one pill gliding behind the rail's icons
    // (`rail_pill`), so the items themselves carry no fill.
    let rail_item = |key: String, mark: El, _on: bool, tip: String| {
        el().key(key)
            .size(30., 30.)
            .center()
            .radius(design::CONTROL_RADIUS)
            .hover(p.hover)
            .cursor(rune::Cursor::Pointer)
            .role("button")
            .label(tip.clone())
            .help(tip)
            .child(mark)
    };
    let tab = |key: &str, i: &'static rune::Icon, rail: Rail, tip: &str| {
        let on = m.rail == rail;
        let slot = slot.clone();
        rail_item(key.into(), ico(i, ICON_SM, p.text), on, tip.into()).on_press(
            move |s: &mut State| {
                if let Some(m) = slot.get_mut(s) {
                    m.rail = rail.clone();
                }
            },
        )
    };
    // The selected item's slot in the rail (Favorites, All, then each
    // shown provider).
    let mut selected = match &m.rail {
        Rail::Favorites => Some(0),
        Rail::All => Some(1),
        Rail::Provider(_) => None,
    };
    let mut slots = 2;
    let mut items = vec![
        tab("rail-favorites", &icons::STAR, Rail::Favorites, "Favorites"),
        tab("rail-all", &icons::LAYOUT_GRID, Rail::All, "All providers"),
    ];
    for provider in m.ordered_providers(order) {
        if m.prefs.provider_hidden(&provider.provider) {
            continue;
        }
        let key = provider.provider.clone();
        let on = m.rail == Rail::Provider(key.clone());
        let tip = format!("{} (drag to reorder)", provider.name);
        let mark = provider_mark(
            s,
            provider.brand.as_deref(),
            &provider.runtime,
            &provider.provider,
            18.,
        );
        if on {
            selected = Some(slots);
        }
        slots += 1;
        let mut item = rail_item(format!("rail-{key}"), mark, on, tip);
        if let Some((dragged, dy)) = &m.rail_drag
            && *dragged == key
        {
            item = item.offset(0., *dy).raise().fill(p.active).shadow(
                4.,
                12.,
                Color::BLACK.alpha(0.3),
            );
        }
        let (k1, k4) = (key.clone(), key);
        let (s1, s4) = (slot.clone(), slot.clone());
        items.push(
            item.on_press(move |s: &mut State| {
                if let Some(m) = s1.get_mut(s) {
                    m.rail = Rail::Provider(k1.clone());
                }
            })
            .on_drag(move |s: &mut State, d, _| rail_drag(s, &s4, &k4, d)),
        );
    }

    // The pill goes first, so the icons draw over it.
    let rail = col()
        .key("model-rail-items")
        .padding(RAIL_PAD)
        .gap(RAIL_STEP - RAIL_ITEM)
        .children(selected.map(|ix| rail_pill(p, ix)))
        .children(items);

    // Holding ⌘ numbers the first nine rows; ⌘N picks row N.
    let mut rows = col().key("model-rows").px(4.).py(4.);
    let mut shown = 0;
    for (title, provider, picks) in m.groups(order, recent) {
        if picks.is_empty() {
            continue;
        }
        // Each group is its own keyed list: a model shows both under Recent
        // and under its provider, and row keys are only unique per group.
        let group_key = provider.map_or_else(
            || format!("model-group-{title}"),
            |pv| format!("model-group-{}", pv.provider),
        );
        let mut group = col().key(group_key).child(group_header(p, title, provider));
        for (provider, model) in picks {
            let badge = (m.cmd_held && shown < 9).then_some(shown);
            shown += 1;
            group = group.child(model_row(s, m, slot, k, provider, model, badge));
        }
        rows = rows.child(group);
    }
    if shown == 0 {
        let msg = if m.route.is_none() {
            "Loading models…"
        } else if m.rail == Rail::Favorites {
            "No favorites yet. Star a model to pin it here."
        } else {
            "No model matches."
        };
        rows = rows.child(el().padding(12.).child(txt(msg, SM, p.text_faint).wrap()));
    }
    let (edit_slot, key_slot) = (slot.clone(), slot.clone());
    let search = TextInput::new(&m.search)
        .placeholder("Search models")
        .width(None)
        .label("Search models")
        .leading_icon(&icons::SEARCH)
        .leading_icon_size(14.)
        .plain()
        .on_edit(move |s: &mut State| search_edit(s, &edit_slot))
        .on_key(move |s: &mut State, ev, _| search_key(s, &key_slot, ev))
        .build(k)
        .key("model-search");

    let mut main = col()
        .key("model-main")
        .grow(1.)
        .min_w(0.)
        .min_h(0.)
        .h_full()
        .child(el().px(4.).py(4.).child(search))
        .child(el().h(1.).w_full().fill(p.border))
        .child(
            col()
                .key("model-list")
                // A new tab or search swaps the rows at once, rather than
                // sliding the few they share across a list that changed.
                .instant_changes()
                .grow(1.)
                .min_h(0.)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_edge_fade(16.)
                .child(rows),
        );
    if let Some(bar) = thinking_row(s, m, slot, k) {
        main = main.child(el().h(1.).w_full().fill(p.border)).child(bar);
    }

    panel_surface(
        p,
        row()
            .key("model-popover")
            .role("dialog")
            .label("Models")
            // A press inside (a thinking level, the rail) leaves the search
            // field focused, so its keys and Escape keep working.
            .preserve_focus_on_press(true)
            .w(POPOVER_WIDTH)
            .h(POPOVER_HEIGHT)
            .items_stretch()
            .clip_rounded(design::PANEL_RADIUS)
            .child(
                col()
                    .key("model-rail")
                    .min_h(0.)
                    .h_full()
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_edge_fade(16.)
                    .child(rail),
            )
            .child(el().w(1.).h_full().fill(p.border))
            .child(main),
    )
}

/// The rail's selection: one keyed plate behind the icon in slot `ix`,
/// gliding to the next pick (a click, or Ctrl+Tab) on the pill spring. It
/// sits where the selected item's own fill did, out of flow and under the
/// icons.
fn rail_pill(p: &Palette, ix: usize) -> El {
    el().key("rail-pill")
        .absolute(RAIL_PAD, RAIL_PAD + ix as f32 * RAIL_STEP)
        .size(RAIL_ITEM, RAIL_ITEM)
        .radius(design::CONTROL_RADIUS)
        .fill(p.active)
        .a11y_hidden()
        .inert()
        .motion(design::motion::PILL)
}

/// The search box's `TextEdit`, wherever the picker lives. A late edit
/// whose picker has gone lands in a scratch field.
fn search_edit<'a>(s: &'a mut State, slot: &Slot) -> &'a mut TextEdit {
    match slot.get_mut(s) {
        Some(m) => &mut m.search,
        None => Box::leak(Box::default()),
    }
}

/// Drag a rail icon over another to move it there.
fn rail_drag(s: &mut State, slot: &Slot, key: &str, d: &mut rune::Drag) {
    match d.phase {
        Phase::Down => {}
        Phase::Move => {
            if let Some(m) = slot.get_mut(s) {
                m.rail_drag = Some((key.to_string(), d.dy));
            }
        }
        Phase::Up => {
            let order = s.prefs.provider_order.clone();
            let Some(m) = slot.get_mut(s) else { return };
            let travel = m.rail_drag.take().map_or(0., |(_, dy)| dy);
            if d.cancelled() || travel.abs() < RAIL_STEP / 2. {
                return;
            }
            let visible: Vec<String> = m
                .ordered_providers(&order)
                .into_iter()
                .filter(|p| !m.prefs.provider_hidden(&p.provider))
                .map(|p| p.provider.clone())
                .collect();
            let Some(from) = visible.iter().position(|p| p == key) else {
                return;
            };
            let to = (from as f32 + (travel / RAIL_STEP).round())
                .clamp(0., (visible.len() - 1) as f32) as usize;
            if let Some(target) = visible.get(to).cloned()
                && to != from
            {
                reorder(s, slot.clone(), key, &target);
            }
        }
    }
}

/// A group's title. Hiding a provider or a model is done in Settings →
/// Providers, so the header is only a label.
fn group_header(p: &Palette, title: String, provider: Option<&RouteProvider>) -> El {
    let key = provider.map_or_else(
        || format!("group-{title}"),
        |pv| format!("group-{}", pv.provider),
    );
    row()
        .key(key)
        .px(8.)
        .pt(8.)
        .pb(4.)
        .h(30.)
        .center_y()
        .child(txt_medium(title, XS, p.text_faint).truncate().grow(1.))
}

/// One pickable model, with a hover action to star it.
fn model_row(
    s: &State,
    m: &ModelPickerState,
    slot: &Slot,
    k: &Kit,
    provider: &RouteProvider,
    model: String,
    badge: Option<usize>,
) -> El {
    let p = &s.palette;
    let on = m.current_provider() == provider.provider && m.current_model() == Some(model.as_str());
    let favorite = m.prefs.favorite(&provider.provider, &model);
    let title = pretty_model(&model);
    let id = format!("{}-{model}", provider.provider);
    // Inside a provider's own tab its name is already the header; only a
    // warning earns the second line.
    let in_tab = matches!(m.rail, Rail::Provider(_));
    let subtitle = if !provider.authenticated {
        Some(format!("{} · not signed in", provider.name))
    } else if in_tab {
        None
    } else {
        Some(provider.name.clone())
    };
    let mut text_col = col()
        .grow(1.)
        .min_w(0.)
        .child(txt(title.clone(), SM, p.text).truncate());
    if let Some(line) = subtitle {
        text_col = text_col.child(txt(line, XS, p.text_faint).truncate());
    }
    let (fav_p, fav_m) = (provider.provider.clone(), model.clone());
    let (pv, md) = (provider.provider.clone(), model);
    let (fav_slot, pick_slot) = (slot.clone(), slot.clone());
    // The list lands its changes (`instant_changes`), so the star swaps; a pop
    // here would never play.
    let star = ghost_icon(
        k,
        format!("fav-{id}"),
        if favorite {
            &icons::STAR_FILL
        } else {
            &icons::STAR
        },
        if favorite { "Unstar" } else { "Star" },
        move |s: &mut State| {
            let (p, m) = (fav_p.clone(), fav_m.clone());
            edit_prefs(s, fav_slot.clone(), move |prefs| {
                prefs.set_favorite(&p, &m, !favorite)
            });
        },
    );
    let mut r = row()
        .key(format!("pick-{id}"))
        .hover_group()
        .role("button")
        .label(title)
        .gap(8.)
        .px(8.)
        .py(4.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .cursor(rune::Cursor::Pointer)
        // The row under the pointer reads stronger than the current one.
        .fill(if on { p.hover } else { Color::TRANSPARENT })
        .hover(p.active)
        .child(provider_mark(
            s,
            provider.brand.as_deref(),
            &provider.runtime,
            &provider.provider,
            16.,
        ))
        .child(text_col);
    if let Some(ix) = badge {
        r = r.child(shortcut_badge(s, ix));
    }
    r.child(
        el().group_opacity(if favorite { 1. } else { 0. }, 1.)
            .child(star),
    )
    // A fixed check column, so picking never shifts the labels.
    .child(
        if on {
            ico(&icons::CHECK, ICON_SM, p.accent)
        } else {
            spacer(16.)
        }
        .w(16.),
    )
    .on_press(move |s: &mut State| pick(s, pick_slot.clone(), pv.clone(), md.clone()))
}

/// The thinking slider's field: a recessed well of dots, `ROWS` high and
/// `COLS` across, with a row of stop ticks under it.
const TRACK_H: f32 = 36.;
const TRACK_RADIUS: f32 = 8.;
/// The first and last stops sit this far in from the well's edges (inside
/// its 1 pt rim).
const INSET: f32 = 10.;
const COLS: usize = 48;
const ROWS: usize = 5;
const DOT: f32 = 2.;
const DOT_PITCH: f32 = 4.5;
const FIELD_TOP: f32 = 6.;
const FIELD_H: f32 = (ROWS - 1) as f32 * DOT_PITCH + DOT;
const TICK_H: f32 = 4.;
/// Ordered-dither thresholds (a 4×4 Bayer matrix): a dot lights once the
/// level's density passes its cell's threshold, so Low is a sparse speckle
/// and the top level a solid block.
const BAYER: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
/// The cursor settling on a level: quick, with a small overshoot.
const SETTLE: Spring = Spring::new(0.34, 0.18);
/// The cursor following the pointer: near-instant, so it never trails.
const FOLLOW: Spring = Spring::new(0.06, 0.);
/// A bright pulse sweeping the lit columns left to right.
static PULSE: [(f32, f32); 4] = [(0., 0.5), (0.08, 1.), (0.24, 0.5), (1., 0.5)];

/// The field's resting dots, all `COLS` × `ROWS` of them in one shape
/// stretched across the field: column `c` sits at `c` of `COLS - 1` units,
/// and each dot is a stroke, whose width stays in points.
fn resting_dots(color: Color) -> El {
    let mut shape = Shape::new();
    for c in 0..COLS {
        for r in 0..ROWS {
            let (x, y) = (c as f32, r as f32 * DOT_PITCH);
            shape = shape.line([x, y], [x, y + DOT], DOT, LineCap::Butt, color);
        }
    }
    canvas(shape.view_box((COLS - 1) as f32, FIELD_H)).h(FIELD_H)
}

/// The popover's thinking slider: Auto (the provider's default) and the
/// current model's levels as stops across a field of dots. Drag anywhere on
/// the field, or press it, and a hairline cursor follows; letting go snaps
/// it to the nearest stop, and the level's name rolls over in the header.
/// Dots left of the cursor light up, densest at the top level, and a pulse
/// sweeps them, quicker the more the model thinks. Picking a level keeps
/// the popover open; Tab / Shift+Tab in the search field step it from the
/// keyboard. `None` when the model has no levels.
fn thinking_row(s: &State, m: &ModelPickerState, slot: &Slot, k: &Kit) -> Option<El> {
    let choices = thinking_choices(m);
    if choices.len() < 2 {
        return None;
    }
    let p = &s.palette;
    let last = (choices.len() - 1) as f32;
    let at = choices.iter().position(|c| *c == m.effort).unwrap_or(0);
    let held = m.thinking_drag.is_some();
    let frac = m.thinking_drag.unwrap_or(at as f32 / last).clamp(0., 1.);
    // While dragging, the header and the stops show the level a release
    // would pick.
    let shown = if held {
        (frac * last).round() as usize
    } else {
        at
    };
    let name = choices[shown]
        .as_deref()
        .map_or_else(|| "Auto".to_string(), effort_label);
    let spring = if held { FOLLOW } else { SETTLE };

    // The field: every column spans the stops' width, so column `c` sits
    // at `c / (COLS - 1)` of it, the same scale as the cursor.
    let reach = frac * (COLS - 1) as f32;
    let period = 2.0 - 1.3 * frac;
    // The resting dots sit under the lit ones, centred on the columns.
    let resting = resting_dots(p.text_faint.alpha(0.28)).pin(
        Some(DOT / 2.),
        Some(FIELD_TOP),
        Some(DOT / 2.),
        None,
    );
    // Each lit dot is its own plain, childless leaf, so its pulse repaints
    // by patching its opacity rather than the scene.
    let field = row()
        .pin(Some(0.), Some(FIELD_TOP), Some(0.), None)
        .h(FIELD_H)
        .justify_between()
        .children((0..COLS).map(|c| {
            let shown = c as f32 <= reach + 0.01;
            // The column or two at the cursor read brightest, and hold still.
            let edge = shown && reach - c as f32 <= 1.5;
            let dots = (0..ROWS)
                .filter(|&r| {
                    let threshold = (f32::from(BAYER[(r % 4) * 4 + c % 4]) + 0.5) / 16.;
                    shown && threshold < frac
                })
                .map(|r| {
                    let dot = el()
                        .absolute(0., r as f32 * DOT_PITCH)
                        .size(DOT, DOT)
                        .fill(if edge { p.text } else { p.accent });
                    if edge {
                        dot
                    } else {
                        dot.cycle(Cycle {
                            period,
                            phase: c as f32 / COLS as f32 * 0.9,
                            stops: &PULSE,
                            still: 0.8,
                            fps: 30.,
                        })
                    }
                });
            el().key(format!("thinking-dots-{c}"))
                .w(DOT)
                .h_full()
                .children(dots)
        }));
    let stops = row()
        .pin(Some(0.), None, Some(0.), Some(4.))
        .h(TICK_H)
        .justify_between()
        .children(choices.iter().enumerate().map(|(i, value)| {
            let key = value.clone().unwrap_or_else(|| "auto".into());
            let full = value
                .as_deref()
                .map_or_else(|| "Auto".to_string(), effort_label);
            let (pick_slot, level) = (slot.clone(), value.clone());
            // Zero wide, so the pointer lands on the field; this press is
            // the stop's accessibility action.
            el().key(format!("thinking-{key}"))
                .role("radio")
                .label(full)
                .checked(i == at)
                .on_press(move |s: &mut State| pick_effort(s, pick_slot.clone(), level.clone()))
                .w(0.)
                .h_full()
                .center()
                .child(
                    el().w(1.)
                        .h(TICK_H)
                        .fill(if i == shown {
                            p.text
                        } else {
                            p.text_faint.alpha(0.5)
                        })
                        .motion(SETTLE),
                )
        }));
    // The cursor: a hairline at the end of an invisible bar as wide as the
    // level, brighter while held.
    let cursor = el()
        .key("thinking-knob")
        .pin(None, Some(2.), Some(-0.5), Some(TICK_H + 6.))
        .w(1.)
        .fill(p.text.alpha(if held { 0.7 } else { 0.3 }))
        .motion(SETTLE);
    let rider = el()
        .key("thinking-rider")
        .pin(Some(0.), Some(0.), None, Some(0.))
        .w_frac(frac)
        .motion(spring)
        .motion_intent(MotionIntent::OVERSHOOT)
        .child(cursor);
    let drag_slot = slot.clone();
    let track = el()
        .key("thinking-track")
        .role("radiogroup")
        .label("Thinking")
        .w_full()
        .h(TRACK_H)
        .px(INSET)
        .radius(TRACK_RADIUS)
        .fill(p.bg)
        .border(1., p.border)
        .cursor(rune::Cursor::Pointer)
        .on_drag(move |s: &mut State, d, tx| thinking_drag(s, &drag_slot, d, tx))
        .child(
            el().w_full()
                .h_full()
                .child(resting)
                .child(field)
                .child(stops)
                .child(rider),
        );
    Some(
        col()
            .key("model-thinking")
            .px(12.)
            .pt(8.)
            .pb(10.)
            .gap(8.)
            .child(
                row()
                    .center_y()
                    .child(txt_medium("Thinking", XS, p.text_faint).grow(1.))
                    .child(
                        RollingText::new("thinking-level", name)
                            .value(shown as f64)
                            .font(rune::TextStyle::new(crate::design::styles::rem(XS), 500.))
                            .color(p.text)
                            .trailing()
                            .build(k),
                    ),
            )
            .child(track),
    )
}

/// Auto, then the current model's levels: the slider's stops.
fn thinking_choices(m: &ModelPickerState) -> Vec<Option<String>> {
    let model = m.current_model().unwrap_or("default");
    std::iter::once(None)
        .chain(
            m.efforts_of(m.current_provider(), model)
                .iter()
                .cloned()
                .map(Some),
        )
        .collect()
}

/// A press or drag on the thinking track: the knob follows the pointer,
/// and letting go picks the nearest stop. Escape (a cancelled drag) puts
/// it back.
fn thinking_drag(s: &mut State, slot: &Slot, d: &mut rune::Drag, tx: &mut rune::Tx) {
    let Some(m) = slot.get_mut(s) else { return };
    // The stops span the field less its insets (and its 1 pt rim).
    let span = (d.w - 2. * INSET - 2.).max(1.);
    let frac = ((d.x - INSET - 1.) / span).clamp(0., 1.);
    match d.phase {
        Phase::Down | Phase::Move => m.thinking_drag = Some(frac),
        Phase::Up => {
            let choices = thinking_choices(m);
            if d.cancelled() || choices.len() < 2 {
                m.thinking_drag = None;
                return;
            }
            // Picked next frame (`model_picker::poll`): this frame may
            // hold the drag's last move, which lands it all at once. The
            // knob holds where it was let go until then.
            let ix = (frac * (choices.len() - 1) as f32).round() as usize;
            m.thinking_release = Some(choices[ix].clone());
            tx.wake_after(std::time::Duration::ZERO);
        }
    }
}

/// A provider's logo; `brand` (the vendor behind a runtime row) wins, then
/// the provider, then the runtime. Falls back to a letter tile.
pub fn provider_mark(
    s: &State,
    brand: Option<&str>,
    runtime: &str,
    provider: &str,
    size: f32,
) -> El {
    let ids: Vec<&str> = [brand, Some(provider), Some(runtime)]
        .into_iter()
        .flatten()
        .filter(|id| !id.is_empty())
        .collect();
    crate::design::brands::provider_logo(s, &ids, size)
}

#[cfg(test)]
mod tests {
    use super::step_thinking;
    use crate::chat::model_picker::{Slot, scene};

    /// Tab and Shift+Tab in the search field step the thinking level along
    /// the row, wrapping past Auto.
    #[test]
    fn tab_steps_the_thinking_level() {
        let mut s = crate::chat::fixtures::test_state();
        scene(&mut s, true);
        let slot = Slot::chat("t1");
        let effort = |s: &crate::state::State| slot.get(s).and_then(|m| m.effort.clone());
        assert_eq!(effort(&s).as_deref(), Some("high"));
        step_thinking(&mut s, &slot, true);
        assert_eq!(effort(&s).as_deref(), Some("xhigh"));
        step_thinking(&mut s, &slot, true);
        assert_eq!(effort(&s), None, "wraps to Auto");
        step_thinking(&mut s, &slot, false);
        assert_eq!(effort(&s).as_deref(), Some("xhigh"));
    }

    /// Dragging the knob along the track follows the pointer and shows the
    /// level a release would pick; letting go picks the nearest stop, and a
    /// press on the track jumps there.
    #[test]
    fn the_thinking_slider_drags_and_snaps() {
        use rune::{Env, Runtime};
        let mut s = crate::chat::fixtures::test_state();
        scene(&mut s, true);
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        let mut t = 0.;
        let mut step = |rt: &mut Runtime<crate::state::State>| {
            for _ in 0..30 {
                t += 1. / 60.;
                rt.frame(t);
            }
        };
        step(&mut rt);
        let slot = Slot::chat("t1");
        let picker = |rt: &Runtime<crate::state::State>| {
            let m = slot.get(&rt.state).expect("picker");
            (m.effort.clone(), m.thinking_drag)
        };
        let (track, _) = rt.presented("thinking-track").expect("track drawn");
        let y = track[1] + track[3] / 2.;
        // Stops: Auto, Low, Medium, High (picked), Extra High.
        let inset = super::INSET + 1.;
        let stop = |i: f32| track[0] + inset + (track[2] - 2. * inset) * i / 4.;
        rt.pointer_move(stop(3.), y);
        rt.pointer_down(stop(3.), y);
        step(&mut rt);
        rt.pointer_move(stop(1.) + 8., y);
        step(&mut rt);
        let (effort, held) = picker(&rt);
        assert_eq!(effort.as_deref(), Some("high"), "nothing picked mid-drag");
        assert!(
            held.is_some_and(|f| (f - 0.25).abs() < 0.05),
            "knob follows: {held:?}"
        );
        // A real pointer's last move and its release land in one frame.
        // The composer pill's level rolls from High to Low on the release:
        // a drag's commits land at once unless the release asks for motion.
        let label = slot.key("model-thinking-label");
        let width = |rt: &Runtime<crate::state::State>| {
            rt.presented(&label).expect("pill level drawn").0[2]
        };
        let before = width(&rt);
        rt.pointer_move(stop(1.) + 3., y);
        rt.pointer_up(stop(1.) + 3., y);
        rt.frame(1. / 60.);
        // The app's poll before the next frame picks the level let go on.
        // A wake's commit lands at once unless the poll asks for motion.
        rt.update(|s, tx| {
            tx.instant();
            crate::chat::model_picker::poll(s, tx);
        });
        rt.frame(1. / 60.);
        rt.frame(1. / 60.);
        let mid = width(&rt);
        step(&mut rt);
        let after = width(&rt);
        assert!(
            (before - after).abs() > 2. && (mid - after).abs() > 0.5,
            "the pill's level eases, not snaps: {before} -> {mid} -> {after}"
        );
        let (effort, held) = picker(&rt);
        assert_eq!(
            effort.as_deref(),
            Some("low"),
            "snapped to the nearest stop"
        );
        assert!(held.is_none());
        // A press at the far end picks the top level.
        rt.pointer_move(stop(4.), y);
        rt.pointer_down(stop(4.), y);
        rt.pointer_up(stop(4.), y);
        rt.frame(1. / 60.);
        rt.update(|s, tx| {
            crate::chat::model_picker::poll(s, tx);
        });
        step(&mut rt);
        assert_eq!(picker(&rt).0.as_deref(), Some("xhigh"));
    }
}
