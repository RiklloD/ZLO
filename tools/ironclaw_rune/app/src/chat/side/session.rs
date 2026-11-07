//! The session card (Zeron's first titlebar toggle) and side chats (GPUI
//! `chat/side/session.rs`).
//!
//! The card shows where the conversation runs: the profile, its model and
//! folder, the project the coding tabs read, and the side chats.
//!
//! A side chat is a by-the-way question about this conversation. It is a
//! fork (the conversation so far, the running turn's prompt included) that
//! opens as a full chat in the right pane, so the main turn keeps going in
//! view while the side one answers. Side chats are listed under their
//! parent (in `Prefs::side_chats`) and stay out of the inbox. One open at a
//! time; closing it drops its stream (area A's embedded chat), reopening
//! reads it back.

use ironclaw_gpui_client::orchestration::ForkOptions;
use rune::{Align, Color, Cursor, Cx, El, Side, col, el, icon, row};
use rune_kit::Kit;

use super::ui::{self, SM, XS};
use super::{SideChat, Tab, bridge, pick_project_folder, short_path, side, side_mut, thread_id};
use crate::icons;
use crate::jobs;
use crate::state::State;

const TOGGLE_KEY: &str = "side-session-toggle";

/// ⌥⌘N works here: not busy forking, and the conversation has a message.
pub fn new_side_chat_allowed(s: &State) -> bool {
    side(s).is_some_and(|x| !x.chat_busy) && !bridge::rows(s).is_empty()
}

/// Fork this conversation into a side chat and show it (⌥⌘N). Area A's
/// embedded side chat must not call this: only top-level chats have side
/// chats (Zeron's rule).
pub fn new_side_chat(s: &mut State) {
    let Some(parent) = thread_id(s) else { return };
    if side(s).is_none_or(|x| x.chat_busy) {
        return;
    }
    if bridge::rows(s).is_empty() {
        bridge::set_error(s, "Send a message first: a side chat starts from this conversation.".into());
        return;
    }
    if let Some(x) = side_mut(s) {
        x.chat_busy = true;
        x.session_open = false;
    }
    let asked = parent.clone();
    jobs::run(
        s,
        "side-chat",
        move |c| async move {
            let options = ForkOptions {
                title: Some("Side chat".into()),
                ..ForkOptions::default()
            };
            c.fork_thread(&asked, &options).await
        },
        move |s, result, _| {
            // The link to the parent is saved even if the user switched
            // threads meanwhile, so the child never shows up in the inbox
            // as a stray thread.
            if let Ok(forked) = &result {
                let child = forked.fork_thread_id.clone();
                let key = parent.clone();
                s.update_prefs(|p| p.side_chats.entry(key).or_default().push(child));
            }
            let Some(x) = super::side_of(s, &parent) else { return };
            x.chat_busy = false;
            match result {
                Ok(forked) => open(s, forked.fork_thread_id),
                Err(e) => {
                    tracing::warn!(error = %e, "side chat fork failed");
                    bridge::set_error(s, e.to_string());
                }
            }
        },
    );
}

/// Show side chat `thread_id` in the right pane.
pub(super) fn open(s: &mut State, thread_id: String) {
    let Some(x) = side(s) else { return };
    let fresh = x.chat.as_ref().map(|c| c.thread_id.as_str()) != Some(thread_id.as_str());
    let chat = fresh.then(|| SideChat::new(s, thread_id.clone()));
    let Some(x) = side_mut(s) else { return };
    x.session_open = false;
    if let Some(chat) = chat {
        x.chat = Some(chat);
        crate::chat::start_side_chat(s, &thread_id);
    }
    super::show(s, Tab::SideChat);
}

/// Close the side chat; the pane goes back to Activity.
fn close(s: &mut State) {
    if let Some(x) = side_mut(s) {
        x.chat = None;
    }
    s.update_prefs(|p| p.side_tab = Tab::Activity.id().into());
}

/// The side chats of this thread, newest first.
fn chats(s: &State) -> Vec<String> {
    s.chat
        .as_ref()
        .and_then(|c| s.prefs.side_chats.get(&c.thread_id))
        .map(|ids| ids.iter().rev().cloned().collect())
        .unwrap_or_default()
}

/// The pane was left on a side chat: the newest one comes back.
pub(super) fn restore(s: &mut State) {
    if s.prefs.side_tab != Tab::SideChat.id() {
        return;
    }
    let Some(id) = chats(s).into_iter().next() else { return };
    let chat = SideChat::new(s, id.clone());
    if let Some(x) = side_mut(s) {
        x.chat = Some(chat);
        crate::chat::start_side_chat(s, &id);
    }
}

/// The side chat tab's body.
pub(super) fn chat_view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    match side(s).and_then(|x| x.chat.as_ref()) {
        Some(chat) => ui::fill_col().child(crate::chat::embedded_view(s, &chat.state, cx, k, layer)),
        None => ui::empty(s, "No side chat open."),
    }
}

/// The side chat's title and its buttons, in the pane's title row.
pub(super) fn chat_actions(s: &State) -> El {
    let p = &s.palette;
    let k = crate::design::kit(p);
    let Some(chat) = side(s).and_then(|x| x.chat.as_ref()) else {
        return el();
    };
    let id = chat.thread_id.clone();
    let title = bridge::title(s, &id).filter(|t| t != "Side chat").unwrap_or_default();
    row()
        .grow(1.)
        .min_w(0.)
        .gap(4.)
        .items_center()
        .child(ui::line(title, XS, p.text_faint).grow(1.).min_w(0.))
        .child(
            ui::ibtn(&icons::PLUS, "New side chat")
                .on_press(new_side_chat)
                .build(&k)
                .key("side-chat-new"),
        )
        .child(
            ui::ibtn(&icons::MAXIMIZE, "Open as the main chat")
                .label("Open side chat as the main chat")
                .on_press(move |s: &mut State| crate::workspace::open_thread(s, id.clone()))
                .build(&k)
                .key("side-chat-full"),
        )
        .child(
            ui::ibtn(&icons::CLOSE, "Close side chat")
                .on_press(close)
                .build(&k)
                .key("side-chat-close"),
        )
}

/// Zeron's first titlebar toggle: the session card, with a dot while a side
/// chat works. The open card goes into `layer`, under the toggle.
pub(super) fn toggle(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else { return el() };
    let open = x.session_open;
    let ink = if open { p.text } else { p.text_muted };
    let mut toggle = super::dock::toggle(s, TOGGLE_KEY, open, "Session and side chats", card_glyph(ink))
        .on_press(|s: &mut State| {
            let Some(x) = side_mut(s) else { return };
            x.session_open = !x.session_open;
            if x.session_open {
                let ids = chats(s);
                bridge::ensure_titles(s, ids);
            }
        });
    if bridge::side_chat_running(s) || x.chat_busy {
        toggle = toggle.child(ui::dot(p.accent, 6.).absolute(19., 3.));
    }
    if open {
        layer.push(card(s, k));
    }
    toggle
}

/// The session glyph (GPUI `panels::card_glyph`): a card with two lines.
fn card_glyph(ink: Color) -> El {
    col()
        .w(15.)
        .h(13.)
        .radius(3.)
        .border(1.5, ink)
        .px(3.)
        .py(3.)
        .gap(2.)
        .child(el().w(7.).h(1.5).fill(ink))
        .child(el().w(5.).h(1.5).fill(ink.alpha(0.6)))
}

/// Zeron's session card: two glass cards, where the chat runs (profile,
/// model, folder, project) and its side chats, as plain rows.
fn card(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else { return el() };
    let glass = |content: El| {
        content
            .w_full()
            .padding(4.)
            .radius(crate::design::PANEL_RADIUS)
            .fill(p.surface_overlay)
            .border(1., p.border)
            .shadow(8., 24., Color::BLACK.alpha(if p.light { 0.12 } else { 0.35 }))
    };
    let line = |key: String, glyph: &'static rune::Icon, content: String, live: bool| {
        row()
            .key(key)
            .w_full()
            .h(32.)
            .px(8.)
            .gap(10.)
            .items_center()
            .radius(8.)
            .child(icon(glyph, 15.).color(if live { p.text_muted } else { p.text_faint }))
            .child(ui::line(content, SM, if live { p.text } else { p.text_faint }).grow(1.).min_w(0.))
    };
    let action = |e: El, enabled: bool| {
        if enabled { e.role("button").cursor(Cursor::Pointer).hover(p.hover) } else { e }
    };
    let hint = |content: &str| ui::line(content.to_string(), XS, p.text_faint);

    // Where it runs.
    let model = bridge::model_label(s);
    let profile = bridge::profile(s);
    let who = match &profile {
        Some((name, _, _)) => format!("{name} · {model}"),
        None => model,
    };
    let mut place = col().child(line("session-profile".into(), &icons::LAPTOP, who, true));
    if let Some((_, folder, branch)) = &profile {
        let mut folder_row = line(
            "session-folder".into(),
            &icons::FOLDER,
            short_path(std::path::Path::new(folder)),
            true,
        );
        if let Some(branch) = branch {
            folder_row = folder_row.child(
                row()
                    .gap(4.)
                    .items_center()
                    .child(icon(&icons::GIT_BRANCH, 12.).color(p.text_faint))
                    .child(ui::line(branch.clone(), XS, p.text_faint)),
            );
        }
        place = place.child(folder_row);
    }
    let has_project = x.folder.is_some();
    let project = action(
        line(
            "session-project".into(),
            &icons::HOUSE,
            x.folder.as_deref().map_or("No project".into(), short_path),
            has_project,
        ),
        true,
    )
    .label("Choose the project folder")
    .child(hint(if has_project { "Change" } else { "Choose…" }))
    .on_press_tx(|s: &mut State, tx| {
        if let Some(x) = side_mut(s) {
            x.session_open = false;
        }
        pick_project_folder(tx);
    });
    let place = place.child(project);

    // Side chats.
    let open_id = x.chat.as_ref().map(|c| c.thread_id.clone());
    let running = bridge::side_chat_running(s);
    let mut list = col().child(
        el().px(8.)
            .pt(8.)
            .pb(4.)
            .child(ui::line("SIDE CHATS", rune::tok::TextStyle::new(12., 600.), p.text_faint)),
    );
    for (ix, id) in chats(s).into_iter().enumerate() {
        let shown = open_id.as_deref() == Some(id.as_str());
        let name = bridge::title(s, &id).unwrap_or_else(|| "Side chat".into());
        let mut item = action(
            line(format!("session-side-chat-{ix}"), &icons::MESSAGES_SQUARE, name.clone(), true),
            true,
        )
        .label(format!("Open side chat {name}"));
        if shown {
            item = item.fill(p.active);
        }
        if shown && running {
            item = item.child(rune_kit::Spinner::new().size(12.).build(k));
        }
        list = list.child(item.on_press(move |s: &mut State| open(s, id.clone())));
    }
    let can_fork = !bridge::rows(s).is_empty();
    let busy = x.chat_busy;
    let mut new_chat = action(
        line("session-new-side-chat".into(), &icons::PLUS, "New side chat".into(), can_fork && !busy),
        can_fork && !busy,
    )
    .label("New side chat");
    new_chat = if busy {
        new_chat.child(rune_kit::Spinner::new().size(12.).build(k))
    } else {
        new_chat.child(hint("⌥⌘N"))
    };
    if can_fork && !busy {
        new_chat = new_chat.on_press(new_side_chat);
    }
    let mut fork = action(
        line("session-fork".into(), &icons::GIT_FORK, "Fork chat".into(), can_fork),
        can_fork,
    )
    .label("Fork chat");
    if can_fork {
        fork = fork.on_press(|s: &mut State| {
            if let Some(x) = side_mut(s) {
                x.session_open = false;
            }
            bridge::fork(s);
        });
    }
    list = list.child(new_chat).child(fork);
    if !can_fork {
        list = list.child(el().px(8.).pb(4.).child(hint("Send a message first.")));
    }

    col()
        .key("side-session-card")
        .w(280.)
        .gap(8.)
        .anchored(TOGGLE_KEY, Side::Bottom, Align::End, 6.)
        .transition(rune::Transition::opacity())
        .modal(true, |s: &mut State| {
            if let Some(x) = side_mut(s) {
                x.session_open = false;
            }
        })
        .child(glass(place))
        .child(glass(list))
}
