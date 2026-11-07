//! The model picker's pills and popovers (GPUI `ModelPicker::render`,
//! `render_popover`, `render_effort*`, `group_header`, `model_row`).

use ironclaw_gpui_client::orchestration::RouteProvider;
use rune::{Color, Cx, El, Key, Modifier, Phase, TextEdit, col, el, row};
use rune_kit::{Kit, Placement, TextInput, popover, scrim};

use super::{
    ModelPickerState, NATIVE, Rail, Slot, cycle_rail, edit_prefs, effort_label, hidden_summary, pick,
    pick_effort, pretty_model, reorder, toggle_provider,
};
use crate::design::smooth_scroll::WheelGlide;
use crate::design::kit::compact::{
    ICON_SM, ICON_XS, SM, XS, ghost_icon, ico, panel_surface, spacer, spinner, txt, txt_medium,
};
use crate::design::kit::shortcut_badge;
use crate::design::{self, Palette};
use crate::icons;
use crate::state::State;

const POPOVER_WIDTH: f32 = 380.;
/// A fixed height on every tab, so switching tabs never resizes the panel;
/// a longer list scrolls.
const POPOVER_HEIGHT: f32 = 300.;
/// Opening below, the panel clears the composer card: the pill's 28 px, the
/// row's 2 px slack, the card's 8 px bottom padding and its rim, then an
/// 8 px gap. Rune anchors from the pill's edge, so the gap is this less 28.
const BELOW_OFFSET: f32 = 47.;
/// Opening above, the gap between the pill and the panel (from its top).
const ABOVE_OFFSET: f32 = 34.;
const PILL_H: f32 = 28.;
/// A rail icon plus the rail's gap: one slot of travel when dragging.
const RAIL_STEP: f32 = 34.;

/// The model and thinking pills of the chat showing `thread_id` (the open
/// chat or a side chat), popovers into `layer`.
pub fn pills(s: &State, thread_id: &str, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    pills_for(s, Slot::chat(thread_id), cx, k, layer)
}

/// The pills of the picker `slot` finds (the chat's, or the draft screen's).
pub fn pills_for(s: &State, slot: Slot, _cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let Some(m) = slot.get(s) else { return el() };
    if m.route.as_ref().is_none_or(|r| r.providers.is_empty()) && m.current.is_none() {
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
    let mut pill = row()
        .key(slot.key("model-pill"))
        .role("button")
        .label(format!("Model: {label}"))
        .expanded(m.open)
        .gap(6.)
        .px(8.)
        .h(PILL_H)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .fill(if m.open { p.active } else { Color::TRANSPARENT })
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .child(provider_mark(s, brand.as_deref(), &runtime, provider.as_deref().unwrap_or(NATIVE), 14.));
    if !m.compact {
        pill = pill.child(txt(label.clone(), SM, ink));
    }
    if m.busy {
        pill = pill.child(spinner(k, ICON_XS, ink));
    } else if !m.compact {
        pill = pill.child(ico(&icons::CHEVRON_DOWN, ICON_XS, ink));
    }
    if m.compact && !m.open {
        pill = pill.help(label);
    }
    let toggle = slot.clone();
    let pill = pill.on_press(move |s: &mut State| {
        if let Some(m) = toggle.get_mut(s) {
            m.toggle();
        }
    });
    let (placement, gap) = placement(m);
    if m.open {
        let closing = slot.clone();
        layer.push(scrim("model-scrim", move |s: &mut State| close(s, &closing)));
        let popover = model_popover(s, m, &slot, k);
        layer.push(popover::anchored(k, popover, &slot.key("model-pill"), placement, gap));
    }
    let mut out = row().key(slot.key("model-pills")).gap(2.).center_y().child(pill);
    if let Some(effort) = effort_pill(s, m, &slot, k, layer) {
        out = out.child(effort);
    }
    out
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
fn placement(m: &ModelPickerState) -> (Placement, f32) {
    if m.compact || m.thread_id.is_some() {
        (Placement::TopStart, ABOVE_OFFSET - PILL_H)
    } else {
        (Placement::BottomStart, BELOW_OFFSET - PILL_H)
    }
}

/// The search field's keys: Ctrl+Tab / Ctrl+Shift+Tab walk the rail, ⌘1..⌘9
/// pick a row, Escape closes, and holding ⌘ numbers the rows.
fn search_key(s: &mut State, slot: &Slot, ev: &rune::KeyEvent) -> bool {
    match &ev.key {
        Key::Tab if ev.mods.ctrl => {
            cycle_rail(s, slot.clone(), !ev.mods.shift);
            true
        }
        Key::Command(c) if !ev.mods.ctrl && !ev.mods.alt && !ev.mods.shift => {
            let Some(ix) = c.to_digit(10).filter(|d| (1..=9).contains(d)).map(|d| d as usize - 1) else {
                return false;
            };
            let (order, recent) = (s.prefs.provider_order.clone(), s.prefs.recent_models.clone());
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
        Key::Modifier { modifier: Modifier::SuperLeft | Modifier::SuperRight, .. } => {
            if let Some(m) = slot.get_mut(s) {
                m.cmd_held = ev.mods.cmd;
            }
            false
        }
        _ => false,
    }
}

fn model_popover(s: &State, m: &ModelPickerState, slot: &Slot, k: &Kit) -> El {
    let p = &s.palette;
    let order = &s.prefs.provider_order;
    let recent = &s.prefs.recent_models;

    let rail_item = |key: String, mark: El, on: bool, tip: String| {
        el().key(key)
            .size(30., 30.)
            .center()
            .radius(design::CONTROL_RADIUS)
            .fill(if on { p.active } else { Color::TRANSPARENT })
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
        rail_item(key.into(), ico(i, ICON_SM, p.text), on, tip.into()).on_press(move |s: &mut State| {
            if let Some(m) = slot.get_mut(s) {
                m.rail = rail.clone();
            }
        })
    };
    let mut rail = col()
        .key("model-rail-items")
        .padding(4.)
        .gap(4.)
        .child(tab("rail-favorites", &icons::STAR, Rail::Favorites, "Favorites"))
        .child(tab("rail-all", &icons::LAYOUT_GRID, Rail::All, "All providers"));
    for provider in m.ordered_providers(order) {
        let hidden = m.prefs.provider_hidden(&provider.provider);
        if hidden && !m.show_hidden {
            continue;
        }
        let key = provider.provider.clone();
        let on = m.rail == Rail::Provider(key.clone());
        let tip = format!(
            "{} (drag to reorder, double-click to {})",
            provider.name,
            if hidden { "show" } else { "hide" }
        );
        let mark = provider_mark(s, provider.brand.as_deref(), &provider.runtime, &provider.provider, 18.);
        let mut item = rail_item(format!("rail-{key}"), mark, on, tip);
        if hidden {
            item = item.opacity(0.4);
        }
        if let Some((dragged, dy)) = &m.rail_drag
            && *dragged == key
        {
            item = item.offset(0., *dy).raise().fill(p.active).shadow(4., 12., Color::BLACK.alpha(0.3));
        }
        let (k1, k2, k3, k4) = (key.clone(), key.clone(), key.clone(), key);
        let (s1, s2, s3, s4) = (slot.clone(), slot.clone(), slot.clone(), slot.clone());
        rail = rail.child(
            item.on_press(move |s: &mut State| {
                if let Some(m) = s1.get_mut(s) {
                    m.rail = Rail::Provider(k1.clone());
                }
            })
            .on_double_press(move |s: &mut State| toggle_provider(s, s2.clone(), k2.clone()))
            .on_secondary_press(move |s: &mut State, _, _| toggle_provider(s, s3.clone(), k3.clone()))
            .on_drag(move |s: &mut State, d, _| rail_drag(s, &s4, &k4, d)),
        );
    }

    // Holding ⌘ numbers the first nine rows; ⌘N picks row N.
    let mut rows = col().key("model-rows").px(4.).py(4.);
    let mut shown = 0;
    for (title, provider, picks) in m.groups(order, recent) {
        if picks.is_empty() {
            continue;
        }
        rows = rows.child(group_header(p, m, slot, k, title, provider));
        for (provider, model) in picks {
            let badge = (m.cmd_held && shown < 9).then_some(shown);
            shown += 1;
            rows = rows.child(model_row(s, m, slot, k, provider, model, badge));
        }
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
    let hidden_providers = m.prefs.hidden_providers.len();
    let hidden = m.hidden_count();

    let (edit_slot, key_slot, hidden_slot) = (slot.clone(), slot.clone(), slot.clone());
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
                .grow(1.)
                .min_h(0.)
                .scroll(rune::ScrollSpec { stick_bottom: false }).wheel_glide("model-list")
                .child(rows),
        );
    if hidden > 0 {
        let label = if m.show_hidden {
            "Done showing hidden".to_string()
        } else {
            hidden_summary(hidden_providers, hidden - hidden_providers)
        };
        main = main.child(el().h(1.).w_full().fill(p.border)).child(
            row()
                .key("model-show-hidden")
                .px(12.)
                .py(6.)
                .role("button")
                .cursor(rune::Cursor::Pointer)
                .child(txt(label, XS, p.text_faint))
                .on_press(move |s: &mut State| {
                    if let Some(m) = hidden_slot.get_mut(s) {
                        m.show_hidden = !m.show_hidden;
                    }
                }),
        );
    }

    panel_surface(
        p,
        row()
            .key("model-popover")
            .role("dialog")
            .label("Models")
            .w(POPOVER_WIDTH)
            .h(POPOVER_HEIGHT)
            .items_stretch()
            .clip_rounded(design::PANEL_RADIUS)
            .child(
                col()
                    .key("model-rail")
                    .min_h(0.)
                    .h_full()
                    .scroll(rune::ScrollSpec { stick_bottom: false }).wheel_glide("model-rail")
                    .child(rail),
            )
            .child(el().w(1.).h_full().fill(p.border))
            .child(main),
    )
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
                .filter(|p| m.show_hidden || !m.prefs.provider_hidden(&p.provider))
                .map(|p| p.provider.clone())
                .collect();
            let Some(from) = visible.iter().position(|p| p == key) else { return };
            let to = (from as f32 + (travel / RAIL_STEP).round()).clamp(0., (visible.len() - 1) as f32) as usize;
            if let Some(target) = visible.get(to).cloned()
                && to != from
            {
                reorder(s, slot.clone(), key, &target);
            }
        }
    }
}

/// A provider group's title; hovering shows a button that hides (or, while
/// hidden ones are shown, restores) the whole provider.
fn group_header(
    p: &Palette,
    m: &ModelPickerState,
    slot: &Slot,
    k: &Kit,
    title: String,
    provider: Option<&RouteProvider>,
) -> El {
    let key = provider.map_or_else(|| format!("group-{title}"), |pv| format!("group-{}", pv.provider));
    let header = row()
        .key(key)
        .px(8.)
        .pt(8.)
        .pb(4.)
        .gap(4.)
        .center_y()
        .child(txt_medium(title, XS, p.text_faint).truncate().grow(1.));
    let Some(provider) = provider else { return header };
    let pk = provider.provider.clone();
    let slot = slot.clone();
    let hidden = m.prefs.provider_hidden(&pk);
    let tip = if hidden { "Show this provider" } else { "Hide this provider" };
    let button = ghost_icon(
        k,
        format!("hide-provider-{pk}"),
        if hidden { &icons::EYE } else { &icons::EYE_OFF },
        tip,
        move |s: &mut State| toggle_provider(s, slot.clone(), pk.clone()),
    );
    header
        .hover_group()
        .h(30.)
        .child(el().group_opacity(if hidden { 1. } else { 0. }, 1.).child(button))
}

/// One pickable model, with hover actions to star and hide it.
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
    let hidden = m.prefs.model_hidden(&provider.provider, &model);
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
    let mut text_col = col().grow(1.).min_w(0.).child(txt(title.clone(), SM, p.text).truncate());
    if let Some(line) = subtitle {
        text_col = text_col.child(txt(line, XS, p.text_faint).truncate());
    }
    let (fav_p, fav_m) = (provider.provider.clone(), model.clone());
    let (hide_p, hide_m) = (provider.provider.clone(), model.clone());
    let (pv, md) = (provider.provider.clone(), model);
    let (fav_slot, hide_slot, pick_slot) = (slot.clone(), slot.clone(), slot.clone());
    let star = ghost_icon(
        k,
        format!("fav-{id}"),
        if favorite { &icons::STAR_FILL } else { &icons::STAR },
        if favorite { "Unstar" } else { "Star" },
        move |s: &mut State| {
            let (p, m) = (fav_p.clone(), fav_m.clone());
            edit_prefs(s, fav_slot.clone(), |prefs| prefs.toggle_favorite(&p, &m));
        },
    );
    let hide = ghost_icon(
        k,
        format!("hide-{id}"),
        if hidden { &icons::EYE } else { &icons::EYE_OFF },
        if hidden { "Show" } else { "Hide" },
        move |s: &mut State| {
            let (p, m) = (hide_p.clone(), hide_m.clone());
            edit_prefs(s, hide_slot.clone(), |prefs| prefs.toggle_model(&p, &m));
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
        .child(provider_mark(s, provider.brand.as_deref(), &provider.runtime, &provider.provider, 16.))
        .child(text_col);
    if hidden {
        r = r.opacity(0.45);
    }
    if let Some(ix) = badge {
        r = r.child(shortcut_badge(s, ix));
    }
    r.child(
        row()
            .gap(2.)
            .child(el().group_opacity(if favorite { 1. } else { 0. }, 1.).child(star))
            .child(el().group_opacity(0., 1.).child(hide)),
    )
    // A fixed check column, so picking never shifts the labels.
    .child(if on { ico(&icons::CHECK, ICON_SM, p.text) } else { spacer(16.) }.w(16.))
    .on_press(move |s: &mut State| pick(s, pick_slot.clone(), pv.clone(), md.clone()))
}

/// The thinking pill, when the current model has levels.
fn effort_pill(s: &State, m: &ModelPickerState, slot: &Slot, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let model = m.current_model().unwrap_or("default");
    let efforts = m.efforts_of(m.current_provider(), model).to_vec();
    if efforts.is_empty() {
        return None;
    }
    let p = &s.palette;
    let label = m.effort.as_deref().map_or_else(|| "Default".to_string(), effort_label);
    let ink = if m.effort_open { p.text } else { p.text_muted };
    let mut pill = row()
        .key(slot.key("effort-pill"))
        .role("button")
        .label(format!("Thinking: {label}"))
        .expanded(m.effort_open)
        .gap(4.)
        .px(8.)
        .h(PILL_H)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .fill(if m.effort_open { p.active } else { Color::TRANSPARENT })
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .child(ico(&icons::SPARKLES, ICON_XS, ink));
    if !m.compact {
        pill = pill.child(txt(label.clone(), SM, ink)).child(ico(&icons::CHEVRON_DOWN, ICON_XS, ink));
    } else if !m.effort_open {
        pill = pill.help(format!("Thinking: {label}"));
    }
    let toggle = slot.clone();
    let pill = pill.on_press(move |s: &mut State| {
        if let Some(m) = toggle.get_mut(s) {
            m.open = false;
            m.effort_open = !m.effort_open;
        }
    });
    if m.effort_open {
        let (placement, gap) = placement(m);
        let closing = slot.clone();
        layer.push(scrim("effort-scrim", move |s: &mut State| close(s, &closing)));
        let popover = effort_popover(p, m, slot, &efforts);
        layer.push(popover::anchored(k, popover, &slot.key("effort-pill"), placement, gap));
    }
    Some(pill)
}

fn effort_popover(p: &Palette, m: &ModelPickerState, slot: &Slot, efforts: &[String]) -> El {
    let item = |key: String, title: String, value: Option<String>| {
        let on = m.effort == value;
        let slot = slot.clone();
        row()
            .key(key)
            .role("button")
            .label(title.clone())
            .gap(8.)
            .px(8.)
            .py(6.)
            .center_y()
            .radius(design::CONTROL_RADIUS)
            .cursor(rune::Cursor::Pointer)
            .fill(if on { p.hover } else { Color::TRANSPARENT })
            .hover(p.active)
            .child(txt(title, SM, p.text).grow(1.))
            .child(if on { ico(&icons::CHECK, ICON_SM, p.text) } else { spacer(16.) }.w(16.))
            .on_press(move |s: &mut State| pick_effort(s, slot.clone(), value.clone()))
    };
    let mut rows = col()
        .key("effort-popover")
        .role("menu")
        .label("Thinking")
        .w(180.)
        .padding(4.)
        .child(el().px(8.).pt(4.).pb(4.).child(txt_medium("Thinking", XS, p.text_faint)))
        .child(item("effort-default".into(), "Default".into(), None));
    for effort in efforts {
        rows = rows.child(item(format!("effort-{effort}"), effort_label(effort), Some(effort.clone())));
    }
    panel_surface(p, rows)
}

/// A provider's logo; `brand` (the vendor behind a runtime row) wins, then
/// the provider, then the runtime. Falls back to a letter tile.
pub fn provider_mark(s: &State, brand: Option<&str>, runtime: &str, provider: &str, size: f32) -> El {
    let ids: Vec<&str> = [brand, Some(provider), Some(runtime)]
        .into_iter()
        .flatten()
        .filter(|id| !id.is_empty())
        .collect();
    crate::design::brands::provider_logo(s, &ids, size)
}
