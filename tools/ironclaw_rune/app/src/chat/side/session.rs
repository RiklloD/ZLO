//! Side chats and the titlebar toggle of the thread details card
//! (`details.rs`), which lists them.
//!
//! A side chat is a by-the-way question about this conversation. It is a
//! fork (the conversation so far, the running turn's prompt included) that
//! opens as a full chat in the right pane, so the main turn keeps going in
//! view while the side one answers. Side chats are listed under their
//! parent (in `Prefs::side_chats`) and stay out of the inbox. One open at a
//! time; closing it drops its stream (area A's embedded chat), reopening
//! reads it back.

use rune::{Cx, El, el, row};
use rune_kit::Kit;
use zlo_client::orchestration::ForkOptions;

use super::ui::{self, xs};
use super::{SideChat, Tab, bridge, side, side_mut, thread_id};
use crate::design::panels;
use crate::icons;
use crate::jobs;
use crate::state::State;

const TOGGLE_KEY: &str = "side-session-toggle";

/// Fork this conversation into a side chat and show it (⌥⌘N). Area A's
/// embedded side chat must not call this: only top-level chats have side
/// chats (Zeron's rule).
pub fn new_side_chat(s: &mut State) {
    let Some(parent) = thread_id(s) else { return };
    if side(s).is_none_or(|x| x.chat_busy) {
        return;
    }
    if bridge::rows(s).is_empty() {
        bridge::set_error(
            s,
            "Send a message first: a side chat starts from this conversation.".into(),
        );
        return;
    }
    super::details::close(s);
    if let Some(x) = side_mut(s) {
        x.chat_busy = true;
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
        move |s, result, tx| {
            // The side chat the person asked for opens with motion (a
            // job's commit would land at once).
            tx.animated();
            // The link to the parent is saved even if the user switched
            // threads meanwhile, so the child never shows up in the inbox
            // as a stray thread.
            if let Ok(forked) = &result {
                let child = forked.fork_thread_id.clone();
                let key = parent.clone();
                s.update_prefs(|p| p.side_chats.entry(key).or_default().push(child));
            }
            let Some(x) = super::side_of(s, &parent) else {
                return;
            };
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
    super::details::close(s);
    let Some(x) = side_mut(s) else { return };
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
pub(super) fn chats(s: &State) -> Vec<String> {
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
    let Some(id) = chats(s).into_iter().next() else {
        return;
    };
    let chat = SideChat::new(s, id.clone());
    if let Some(x) = side_mut(s) {
        x.chat = Some(chat);
        crate::chat::start_side_chat(s, &id);
    }
}

/// The side chat tab's body.
pub(super) fn chat_view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    match side(s).and_then(|x| x.chat.as_ref()) {
        Some(chat) => {
            ui::fill_col().child(crate::chat::embedded_view(s, &chat.state, cx, k, layer))
        }
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
    let title = bridge::title(s, &id)
        .filter(|t| t != "Side chat")
        .unwrap_or_default();
    row()
        .grow(1.)
        .min_w(0.)
        .gap(4.)
        .items_center()
        .child(ui::line(title, xs(), p.text_faint).grow(1.).min_w(0.))
        .child(
            ui::ibtn(&icons::PLUS, "New side chat")
                .on_press(new_side_chat)
                .build(&k)
                .key("side-chat-new"),
        )
        .child(
            ui::ibtn(&icons::ARROW_UP_RIGHT, "Open as the main chat")
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

/// Zeron's first titlebar toggle: the thread details card (`details.rs`),
/// with a dot while a side chat works. The open card goes into `layer`,
/// under the toggle; `window_h` sizes it.
pub(super) fn toggle(s: &State, k: &Kit, layer: &mut Vec<El>, window_h: f32) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else { return el() };
    let open = x.session_open;
    let glyph = panels::card_glyph(
        &format!("{TOGGLE_KEY}-glyph"),
        open,
        panels::GLYPH_PX,
        if open { p.text } else { p.text_muted },
    )
    .group_color(p.text);
    let toggle = panels::toggle_with(s, TOGGLE_KEY, "Thread details · ⌥⌘N new side chat", glyph);
    let mut toggle = super::dock::active(s, toggle, open).on_press(|s: &mut State| {
        if side(s).is_some_and(|x| x.session_open) {
            super::details::close(s);
            return;
        }
        let Some(x) = side_mut(s) else { return };
        x.session_open = true;
        super::details::opened(s);
    });
    if bridge::side_chat_running(s) || x.chat_busy {
        toggle = toggle.child(ui::dot(p.accent, 6.).absolute(19., 3.));
    }
    if open {
        super::details::card(s, k, layer, TOGGLE_KEY, window_h);
    }
    toggle
}
