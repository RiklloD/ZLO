//! Shortcuts and the native menu (GPUI `main.rs` actions, key bindings and
//! menus, and the workspace's `on_key_down` / `on_action` handlers).
//!
//! The View menu's items carry their key equivalents, so on macOS AppKit
//! runs them before any field sees the chord, as GPUI's bindings did.
//! `on_key` handles the same chords too, for headless scripts and platforms
//! without a native menu (a menu item that took the key never reaches it).

use rune::{Chord, Key, KeyEvent, MenuItem, Shortcut, Tx};

use super::{command, draft, in_settings, inbox};
use crate::state::{Conn, State};

/// One View menu entry: title, chord, action.
type Binding = (&'static str, &'static str, fn(&mut State, &mut Tx));

const BINDINGS: &[Binding] = &[
    ("New Thread", "cmd+shift+o", |s, _| new_thread(s)),
    ("Toggle Sidebar", "cmd+b", |s, _| super::toggle_sidebar(s)),
    ("Toggle Right Pane", "alt+cmd+b", |s, _| {
        toggle_right_pane(s)
    }),
    ("Toggle Changes and Files", "cmd+e", |s, _| {
        toggle_work_panel(s)
    }),
    ("New Side Chat", "alt+cmd+n", |s, _| new_side_chat(s)),
    ("Command Menu…", "cmd+k", |s, _| open_command_menu(s)),
    ("Model Picker…", "cmd+shift+m", |s, _| {
        crate::chat::model_picker::open(s)
    }),
    ("Toggle Status Bar", "cmd+shift+s", |s, _| {
        super::toggle_status_bar(s)
    }),
    ("Toggle Light and Dark", "cmd+shift+y", |s, _| {
        super::toggle_scheme(s)
    }),
];

/// The app menu's "Quit ZLO" (GPUI `main.rs`) and its chord.
const QUIT: (&str, &str) = ("Quit ZLO", "cmd+q");

/// The native menus, for `App::menu_item`: "Quit ZLO" in the app menu, then
/// the View menu.
pub fn menu_items() -> Vec<MenuItem<State>> {
    let with_chord = |item: MenuItem<State>, chord: &str| match chord.parse::<Shortcut>() {
        Ok(shortcut) => item.key_equivalent(shortcut),
        Err(e) => {
            tracing::warn!(%chord, error = %e, "menu shortcut did not parse");
            item
        }
    };
    let quit = with_chord(MenuItem::new(QUIT.0, quit).id("quit"), QUIT.1);
    std::iter::once(quit)
        .chain(BINDINGS.iter().map(|(title, chord, action)| {
            with_chord(MenuItem::new(*title, *action).menu("View"), chord)
        }))
        .collect()
}

/// ⌘Q and "Quit ZLO": write the window frame and pending saves (flushed),
/// then quit; `main` stops the backend once the app loop ends.
pub fn quit(s: &mut State, tx: &mut Tx) {
    tracing::debug!("quit requested");
    on_close(s, tx);
    tx.quit();
}

/// `App::on_close`: before the window goes (the close button, ⌘Q, Quit from
/// the menu or the Dock): note the frame and write pending drafts.
pub fn on_close(s: &mut State, _tx: &mut Tx) {
    crate::window_state::save();
    // Drafts, then every other queued save (preferences, voice, SnapShot):
    // the writer is one ordered queue, so this flush waits for all of them.
    crate::drafts::save(s, true);
}

/// ⇧⌘O: a fresh draft in the Default profile.
fn new_thread(s: &mut State) {
    if matches!(s.conn, Conn::Ready) {
        draft::start_in_profile(s, ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID.into());
    }
}

fn open_command_menu(s: &mut State) {
    if matches!(s.conn, Conn::Ready) {
        command::toggle(s);
    }
}

/// ⌥⌘B folds the open thread's right pane; elsewhere the preference.
fn toggle_right_pane(s: &mut State) {
    if s.page == crate::state::Page::Chat && s.chat.is_some() {
        crate::chat::side::toggle_right_pane(s);
    } else {
        s.update_prefs(|p| p.side_open = !p.side_open);
    }
}

/// ⌘E: the changes and files panel, likewise.
fn toggle_work_panel(s: &mut State) {
    if s.page == crate::state::Page::Chat && s.chat.is_some() {
        crate::chat::side::toggle_work_panel(s);
    } else {
        s.update_prefs(|p| p.work_open = !p.work_open);
    }
}

fn new_side_chat(s: &mut State) {
    if s.page == crate::state::Page::Chat && s.chat.is_some() {
        crate::chat::side::new_side_chat(s);
    }
}

pub(super) fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    crate::design::look::input(s);
    // ⌘ alone held: the cards show their ⌘N badges.
    let held = ev.mods.cmd && !(ev.mods.ctrl || ev.mods.alt || ev.mods.shift);
    let modifier_only = matches!(ev.key, Key::Modifier { .. });
    if modifier_only || !held {
        s.workspace.inbox.cmd_held = held && modifier_only;
    }
    if modifier_only {
        return false;
    }
    if ev.key == Key::Escape {
        return super::dismiss(s);
    }
    for (_, chord, action) in BINDINGS {
        if Chord::parse(chord).is_ok_and(|c| c.matches_tx(ev, tx)) {
            action(s, tx);
            return true;
        }
    }
    // ⌘1..⌘9 open the Nth sidebar thread.
    if let Key::Command(c) = &ev.key
        && !ev.mods.shift
        && !ev.mods.alt
        && let Some(n) = c.to_digit(10).filter(|n| (1..=9).contains(n))
    {
        if in_settings(s) {
            return false;
        }
        if let Some(id) = inbox::jump_target(s, n as usize) {
            super::open_thread(s, id);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_binding_parses_as_a_chord_and_a_shortcut() {
        for (title, chord) in BINDINGS.iter().map(|(t, c, _)| (*t, *c)).chain([QUIT]) {
            assert!(Chord::parse(chord).is_ok(), "{title}: {chord}");
            assert!(chord.parse::<Shortcut>().is_ok(), "{title}: {chord}");
        }
    }
}
