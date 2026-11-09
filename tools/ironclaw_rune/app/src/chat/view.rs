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

use zlo_client::StreamStatus;

use rune::{Cx, El, Tx, col, el, icon, row, text};
use rune_kit::Kit;

use super::{ChatState, chat_for, composer, flow, menu, outline};

use crate::icons;
use crate::state::State;

mod transcript;

pub(crate) use transcript::PROMPT_RISE_SPRING;

/// The transcript viewport of `thread`'s chat, for scroll requests.
pub fn transcript_key(thread: &str) -> String {
    format!("transcript:{thread}")
}

/// A transcript row's element key, for scroll requests.
pub fn row_key(item_key: &str) -> String {
    format!("row:{item_key}")
}

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
    // The conversation's wallpaper covers its column, up over the
    // breadcrumb as GPUI's panel-high layer did; the right pane's covers
    // the pane (GPUI computed both rects from the dock sizes).
    let mut conversation = row()
        .key("conversation")
        .grow(1.)
        .min_w(0.)
        .h_full()
        .min_h(0.)
        .child(main);
    if let Some(over) = crate::design::look::wallpaper_layer(s, "conversation") {
        conversation = conversation.child(up_to_panel_top(over));
    }
    // The column grows and may shrink to nothing, so the docks keep their
    // widths at the right edge.
    let mut out = row()
        .key("chat-root")
        .w_full()
        .h_full()
        .min_h(0.)
        .child(conversation);
    let docks = super::side::docks(s, cx, k, layer);
    let last = docks.len().saturating_sub(1);
    for (ix, dock) in docks.into_iter().enumerate() {
        if ix < last {
            out = out.child(dock);
            continue;
        }
        // The right pane, at the window's edge.
        let mut pane = el().key("pane-host").h_full().child(dock);
        if let Some(over) = crate::design::look::wallpaper_layer(s, "pane") {
            pane = pane.child(up_to_panel_top(over));
        }
        out = out.child(pane);
    }
    out
}

/// A wallpaper layer over its region of the chat, reaching up over the
/// breadcrumb to the panel's top as GPUI's panel-high layers did.
fn up_to_panel_top(over: El) -> El {
    el().pin(
        Some(0.),
        Some(-crate::design::TITLEBAR_HEIGHT),
        Some(0.),
        Some(0.),
    )
    .inert()
    .child(over)
}

/// One chat's column: what a side chat draws inside its pane too.
pub(crate) fn column(
    s: &State,
    chat: &ChatState,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
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
        .child(transcript::transcript(s, chat, k));
    if let Some(rail) = (!chat.embedded).then(|| outline::view(s, chat)).flatten() {
        area = area.child(rail);
    }
    // The reserve reads as "scrolled up"; no jump arrow for empty room.
    if chat.far_from_bottom && chat.reserve.is_none() {
        area = area.child(jump_button(s, chat));
    }
    main = main.child(area);

    // Every chat, side chats too, shows its own extras (area H).
    if let Some(screen) = super::computer::view(s, chat, cx, k, layer) {
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
    // The orchestration panel centres itself and puts the plan first.
    let plan = super::plan_strip::view(s, chat);
    if let Some(panel) = super::orchestration::composer(s, chat, k, plan) {
        main = main.child(panel);
    }
    let items = chat.menu.items(&chat.composer.text);
    if !items.is_empty() {
        let selected = chat.menu.selected(items.len());
        let pick_id = id.clone();
        let on_pick: menu::OnPick =
            std::rc::Rc::new(move |s: &mut State, item, tx| pick_item(s, &pick_id, item, tx));
        // Over the composer, as wide as it.
        let anchor = format!("composer:{id}");
        let width = cx
            .layout_size(&anchor)
            .map_or(super::content_width(s), |[w, _]| w);
        menu::popup(
            s,
            k,
            layer,
            menu::Placing {
                key: "command-menu",
                anchor,
                side: rune::Side::Top,
                width,
                menu_of: super::composer::menu_of(&id),
            },
            &items,
            selected,
            on_pick,
        );
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
    row()
        .w_full()
        .justify_center()
        .px(16.)
        .child(el().w_full().max_w(super::content_width(s)).child(body))
}

fn pick_item(s: &mut State, id: &str, item: &zlo_client::commands::MenuItem, tx: &mut Tx) {
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
    if let Some(summary) = super::orchestration::tail_summary(s, &chat.thread_id) {
        pill = pill.child(
            text(summary)
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.text_muted),
        );
    }
    row()
        .pin(Some(0.), None, Some(0.), Some(12.))
        .justify_center()
        .child(pill.on_press_tx(move |s: &mut State, tx| {
            if let Some(c) = chat_for(s, &id) {
                c.far_from_bottom = false;
            }
            tx.scroll_to_bottom(transcript_key(&id));
        }))
}
