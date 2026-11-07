//! The chat's main column (GPUI `chat.rs`, `impl Render for ChatView`): the
//! reconnect banner, the transcript list with its outline and jump button,
//! computer use, the error line, the orchestration panel (with the plan
//! strip), the `/` `@` menu and the composer; the right pane's docks beside
//! it.
//!
//! The transcript is a virtualized growth stack in a scroll viewport that
//! follows its end. Zeron's turn layout comes from the reserve: after a
//! send, the room under the newest prompt is exactly what keeps that prompt
//! at the top of the viewport, so following the end shows the prompt at the
//! top while its answer streams into the room below.

use ironclaw_gpui_client::StreamStatus;
use ironclaw_gpui_client::transcript::{PREFIX_KEY, Row};
use rune::{Cx, El, GrowthSpec, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::Kit;

use super::{ChatState, Item, chat_for, composer, flow, menu, outline, question_form, rows};
use crate::icons;
use crate::state::State;

/// The transcript viewport of `thread`'s chat, for scroll requests.
pub fn transcript_key(thread: &str) -> String {
    format!("transcript:{thread}")
}

/// A transcript row's element key, for scroll requests.
pub fn row_key(item_key: &str) -> String {
    format!("row:{item_key}")
}

/// Estimated height of a row the virtual list has not measured yet.
const ROW_ESTIMATE: f32 = 72.;

/// The open chat with its docks, for `Page::Chat` (the workspace draws the
/// new-thread screen itself).
pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let Some(chat) = &s.chat else {
        return col()
            .w_full()
            .h_full()
            .center()
            .child(text("No thread open").color(s.palette.text_muted));
    };
    let main = column(s, chat, cx, k, layer);
    // The column grows and may shrink to nothing, so the docks keep their
    // widths at the right edge.
    let mut out = row().key("chat-root").w_full().h_full().min_h(0.).child(main);
    for dock in super::side::docks(s, cx, k, layer) {
        out = out.child(dock);
    }
    out
}

/// One chat's column: what a side chat draws inside its pane too.
pub(crate) fn column(s: &State, chat: &ChatState, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let running = chat.running_run().is_some();
    let id = chat.thread_id.clone();
    let mut main = col()
        .key(format!("chat:{id}"))
        .grow(1.)
        .min_w(0.)
        .h_full()
        .min_h(0.);
    if chat.drop_over || crate::workspace::thread_drag::over_chat(s, &id) {
        main = main.fill(p.accent_wash);
    }
    let (drop_id, hover_id, rect_id) = (id.clone(), id.clone(), id.clone());
    main = main
        // A sidebar thread dropped here attaches its transcript.
        .on_drawn_layout_tx(move |s: &mut State, r, tx| {
            crate::workspace::thread_drag::saw_chat(s, &rect_id, r, tx)
        })
        .on_drop_files(move |s: &mut State, files, _tx| {
            super::actions::attach(s, &drop_id, files.to_vec());
            if let Some(c) = chat_for(s, &drop_id) {
                c.drop_over = false;
            }
        })
        .on_drop_hover(move |s: &mut State, over, _tx| {
            if let Some(c) = chat_for(s, &hover_id) {
                c.drop_over = over;
            }
        });

    if let StreamStatus::Reconnecting { delay_secs, .. } = chat.status {
        main = main.child(
            row()
                .key("reconnect-banner")
                .w_full()
                .px(16.)
                .py(4.)
                .fill(p.warning)
                .child(
                    text(format!("Connection lost. Reconnecting in {delay_secs}s"))
                        .font(flow::type_at(s, 12., 16., 400.))
                        .color(p.on_solid),
                ),
        );
    }

    // Room for the floating header; rows fade under its upper edge.
    let top = if chat.embedded { 4. } else { 30. };
    let mut area = col()
        .key("transcript-area")
        .w_full()
        .grow(1.)
        .min_h(0.)
        .pt(top)
        .child(transcript(s, chat, cx, k, layer));
    if let Some(rail) = (!chat.embedded).then(|| outline::view(s, chat)).flatten() {
        area = area.child(rail);
    }
    // The reserve reads as "scrolled up"; no jump arrow for empty room.
    if chat.far_from_bottom && chat.reserve.is_none() {
        area = area.child(jump_button(s, chat));
    }
    main = main.child(area);

    // The extras (area H) belong to the open chat, not a side chat.
    if !chat.embedded
        && let Some(screen) = super::computer::view(s, cx, k, layer)
    {
        main = main.child(screen);
    }
    if let Some(error) = &chat.error {
        main = main.child(centered(
            s,
            text(error.clone())
                .key("chat-error")
                .font(flow::type_at(s, 13., 19., 400.))
                .color(p.danger)
                .wrap()
                .selectable(true),
        ));
    }
    let plan = super::plan_strip::view(s, chat);
    let panel = match chat.embedded {
        // The orchestration panel centres itself and puts the plan first.
        false => super::orchestration::composer(s, k, plan),
        true => plan.map(|plan| centered(s, plan).pt(8.)),
    };
    if let Some(panel) = panel {
        main = main.child(panel);
    }
    let items = chat.menu.items(&chat.composer.text);
    if !items.is_empty() {
        let selected = chat.menu.selected(items.len());
        let pick_id = id.clone();
        let on_pick: menu::OnPick = std::rc::Rc::new(move |s: &mut State, item, tx| pick_item(s, &pick_id, item, tx));
        main = main.child(centered(s, menu::render(s, &items, selected, on_pick)));
    }
    main.child(
        row()
            .key("composer-row")
            .w_full()
            .justify_center()
            .px(16.)
            .pb(16.)
            .pt(8.)
            .child(composer::view(s, chat, running, cx, k, layer)),
    )
}

/// `body` in the reading column, centered in the chat.
fn centered(s: &State, body: El) -> El {
    row().w_full()
        .justify_center()
        .px(16.)
        .child(el().w_full().max_w(super::content_width(s)).child(body))
}

fn pick_item(s: &mut State, id: &str, item: &ironclaw_gpui_client::commands::MenuItem, tx: &mut Tx) {
    let Some(c) = chat_for(s, id) else { return };
    if let menu::MenuKey::Command(command) = menu::pick(item, &mut c.composer) {
        super::actions::pick_command(s, id, &command, tx);
    }
    composer::on_change(s, id);
}

/// "Jump to latest", naming what waits below the fold when it can.
fn jump_button(s: &State, chat: &ChatState) -> El {
    let p = &s.palette;
    let id = chat.thread_id.clone();
    let mut pill = row()
        .key("jump-latest")
        .role("button")
        .label("Jump to latest")
        .h(28.)
        .px(10.)
        .gap(6.)
        .center_y()
        .radius(14.)
        .fill(p.surface_overlay)
        .border(1., p.border_strong)
        .shadow(4., 12., rune::Color::BLACK.alpha(0.2))
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .transition(flow::rise(6.))
        .child(icon(&icons::CHEVRON_DOWN, 13.).color(p.text_muted));
    if let Some(summary) = (!chat.embedded).then(|| super::orchestration::tail_summary(s)).flatten() {
        pill = pill.child(
            text(summary)
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.text_muted),
        );
    }
    row().pin(Some(0.), None, Some(0.), Some(12.))
        .justify_center()
        .child(pill.on_press_tx(move |s: &mut State, tx| {
            if let Some(c) = chat_for(s, &id) {
                c.far_from_bottom = false;
            }
            tx.scroll_to_bottom(transcript_key(&id));
        }))
}

/// The reserve's height: while held, exactly what keeps the newest prompt
/// at the top of the list and no more. It stays after the run ends (the view
/// keeps its place, as in Zeron).
fn reserve_height(chat: &ChatState) -> f32 {
    let Some(reserve) = &chat.reserve else {
        return 0.;
    };
    if !reserve.held {
        return 0.;
    }
    let turn: f32 = chat
        .items
        .iter()
        .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
        .map(|start| {
            chat.items[start..]
                .iter()
                .filter(|i| !matches!(i, Item::Reserve))
                .filter_map(|i| chat.turn_heights.get(i.key()))
                .sum()
        })
        .unwrap_or(0.);
    // Under the reserve: the last row's own bottom padding.
    let below = flow::SPACE_LG + 24.;
    (chat.list_height - turn - below).max(0.)
}

fn transcript(s: &State, chat: &ChatState, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let items = chat.items.clone();
    let id = chat.thread_id.clone();
    let turn_start = items
        .iter()
        .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
        .filter(|_| chat.reserve.is_some());
    // The run each row belongs to (1-based), for "fork from here".
    let run_of: Vec<u32> = items
        .iter()
        .scan(0u32, |run, item| {
            if let Item::Row(Row::User { key, .. }) = item
                && !key.starts_with(PREFIX_KEY)
            {
                *run += 1;
            }
            Some(*run)
        })
        .collect();
    let seed = super::pending::seed(&id);
    let outputs = chat.transcript.tool_outputs();
    let reserve_h = reserve_height(chat);
    let column = super::content_width(s);
    let mut stack = col()
        .key("transcript-rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtualize(ROW_ESTIMATE);
    for (ix, item) in items.iter().enumerate() {
        let top = flow::top_gap(ix.checked_sub(1).map(|i| &items[i]), item);
        let last = ix + 1 == items.len();
        let body = match item {
            Item::Row(row_ @ Row::Assistant { key, text, streaming: false }) => rows::with_actions(
                chat,
                key,
                text,
                false,
                Some(run_of[ix]).filter(|r| *r > 0 && !key.starts_with(PREFIX_KEY)),
                rows::render_row(s, chat, row_, k),
                k,
            ),
            Item::Row(row_ @ Row::User { key, text, pending: false, .. }) if !text.is_empty() => {
                rows::with_actions(chat, key, text, true, None, rows::render_row(s, chat, row_, k), k)
            }
            // The bubble's own key: text that turns into narration keeps its
            // element, so it neither re-parses nor fades in again.
            Item::Row(Row::Narration { text, .. }) => {
                super::visual::prose(s, &id, item.key(), text, false, k)
            }
            Item::Row(Row::Question {
                key,
                run_id,
                gate_ref,
                form,
                state,
            }) => question_form::render_question(s, chat, key, run_id, gate_ref, form, *state, k),
            Item::Row(row_) => rows::render_row(s, chat, row_, k),
            Item::Activity { key, rows: group, live } => {
                let open = chat.activity_open(key, *live);
                flow::render_activity(s, &id, key, group, *live, open, &chat.toggles, &outputs, k)
            }
            Item::Pending { stopping } => super::pending::render_pending(s, *stopping, chat.run_started, seed, k),
            Item::Tail => (!chat.embedded)
                .then(|| super::orchestration::tail(s, k))
                .flatten()
                .unwrap_or_else(el),
            Item::Reserve => el().h(reserve_h),
        };
        let key = item.key().to_string();
        let mut line = row()
            .key(row_key(&key))
            .w_full()
            .justify_center()
            .px(16.)
            .pt(top)
            .child(el().w_full().max_w(column).min_w(0.).child(body));
        if last {
            line = line.pb(flow::SPACE_LG + 24.);
        }
        // Rows of the newest turn report their height to size the reserve.
        if turn_start.is_some_and(|s| ix >= s) && !matches!(item, Item::Reserve) {
            let (thread, key) = (id.clone(), key.clone());
            line = line.on_layout_tx(move |s: &mut State, r, tx| {
                let h = r[3];
                let Some(c) = chat_for(s, &thread) else { return };
                let old = c.turn_heights.insert(key.clone(), h);
                if old.is_some_and(|o| (o - h).abs() <= 0.5) {
                    tx.live_only();
                }
            });
        } else if let Item::Row(Row::User { key: user_key, .. }) = item {
            // Prompt rows tell the outline where they are.
            let (thread, user_key) = (id.clone(), user_key.clone());
            line = line.on_layout_tx(move |s: &mut State, r, tx| {
                outline::saw(s, &thread, &user_key, r[1], tx)
            });
        }
        stack = stack.child(line);
    }
    let (layout_id, far_id) = (id.clone(), id.clone());
    col().key(transcript_key(&id))
        .w_full()
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(24.)
        .on_layout_tx(move |s: &mut State, r, tx| {
            let Some(c) = chat_for(s, &layout_id) else { return };
            let [_, y, w, h] = r;
            if (c.list_height - h).abs() <= 0.5 && c.outline.viewport == [y, w, h] {
                tx.live_only();
                return;
            }
            c.list_height = h;
            c.outline.viewport = [y, w, h];
        })
        .on_far_from_bottom(160., move |s: &mut State, far| {
            if let Some(c) = chat_for(s, &far_id) {
                c.far_from_bottom = far;
            }
        })
        .child(stack)
}
