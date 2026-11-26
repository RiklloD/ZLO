//! Shortcuts and the native menu.
//!
//! The View menu's items carry their key equivalents, so on macOS AppKit
//! runs them before any field sees the chord.
//! `on_key` handles the same chords too, for headless scripts and platforms
//! without a native menu (a menu item that took the key never reaches it).

use rune::{Chord, Key, KeyEvent, MenuItem, Shortcut, Tx};

use super::{command, draft, in_settings, inbox};
use crate::state::{Conn, State};

/// One View menu entry: title, chord, action.
type Binding = (&'static str, &'static str, fn(&mut State, &mut Tx));

// A menu item runs as a queued update, which no input in the window caused,
// so Rune commits it at once unless the handler asks for motion
// (`Tx::animated`). Every View action here is something the person asked to
// see move: a column sliding, the composer flying back to a new draft, ⌘K
// or the model picker rising, the status bar folding, the palette morphing.
const BINDINGS: &[Binding] = &[
    ("New Thread", "mod+shift+o", |s, tx| {
        tx.animated();
        new_thread(s)
    }),
    // The column toggles slide like their titlebar buttons, on the panels'
    // one curve.
    ("Toggle Sidebar", "mod+b", |s, tx| {
        tx.animated();
        super::toggle_sidebar(s, tx)
    }),
    ("Toggle Right Pane", "alt+cmd+b", |s, tx| {
        crate::chat::side::dock_slide(s, tx);
        toggle_right_pane(s)
    }),
    // ⌘J opens and closes the middle column (the work panel) with the
    // same slide as its titlebar button; ⌘E stays an alias (`ALIASES`).
    ("Toggle Workspace Panel", "mod+j", |s, tx| {
        crate::chat::side::dock_slide(s, tx);
        toggle_work_panel(s)
    }),
    ("New Side Chat", "alt+cmd+n", |s, tx| {
        tx.animated();
        new_side_chat(s)
    }),
    ("Command Menu…", "mod+k", |s, tx| {
        tx.animated();
        open_command_menu(s)
    }),
    ("Model Picker…", "mod+shift+m", |s, tx| {
        tx.animated();
        crate::chat::model_picker::open(s)
    }),
    ("Toggle Status Bar", "mod+shift+s", |s, tx| {
        tx.animated();
        super::toggle_status_bar(s)
    }),
    ("Toggle Light and Dark", "mod+shift+y", |s, tx| {
        super::toggle_scheme(s, tx)
    }),
];

/// Extra chords for a View menu action, matched by `on_key` only (a menu
/// item carries one key equivalent): the action's title, then the chord.
const ALIASES: &[(&str, &str)] = &[("Toggle Workspace Panel", "mod+e")];

/// The View menu action titled `title`, for tests that run it as the menu does.
#[cfg(test)]
pub(crate) fn action(title: &str) -> fn(&mut State, &mut Tx) {
    BINDINGS
        .iter()
        .find(|(t, _, _)| *t == title)
        .map(|(_, _, a)| *a)
        .expect("bound")
}

/// The app menu's "Quit ZLO" and its chord.
const QUIT: (&str, &str) = ("Quit ZLO", "mod+q");

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
    // Rune may end the process without macOS's will-terminate: no
    // installer started from the app outlives it.
    crate::cli_install::stop_all();
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
        draft::start_in_profile(s, zlo_client::profiles::DEFAULT_PROFILE_ID.into());
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

/// ⌘J (and ⌘E): the workspace panel (changes, files, outputs), likewise.
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
    // ⌘ alone held: the cards show their ⌘N badges.
    let held = crate::system::primary_alone(ev.mods);
    let modifier_only = matches!(ev.key, Key::Modifier { .. });
    let was = s.workspace.inbox.cmd_held;
    if modifier_only || !held {
        s.workspace.inbox.cmd_held = held && modifier_only;
    }
    if modifier_only {
        // Taken only when ⌘-held flipped, so the frame rebuilds: the ⌘N
        // badges and the composer's live "Steer/Queue message" follow it.
        return s.workspace.inbox.cmd_held != was;
    }
    if ev.key == Key::Escape {
        return super::dismiss(s);
    }
    let aliased = ALIASES.iter().filter_map(|(title, chord)| {
        let (_, _, action) = BINDINGS.iter().find(|(t, _, _)| t == title)?;
        Some((*chord, *action))
    });
    let chords = BINDINGS.iter().map(|(_, c, a)| (*c, *a)).chain(aliased);
    for (chord, action) in chords {
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

    /// A menu item is a queued update: without `Tx::animated` a real
    /// window commits it at once (a headless `Runtime` animates every
    /// commit, so only the flag shows it). Every View action asks.
    #[test]
    fn every_view_action_asks_for_motion() {
        for (title, _, action) in BINDINGS {
            let mut s = crate::chat::fixtures::test_state();
            let mut tx = Tx::default();
            action(&mut s, &mut tx);
            assert!(crate::workspace::asked_motion(&tx), "{title} lands at once");
        }
        let mut s = crate::chat::fixtures::test_state();
        let mut tx = Tx::default();
        action("Toggle Light and Dark")(&mut s, &mut tx);
        assert!(
            crate::workspace::asked_color_motion(&tx),
            "the palette morphs"
        );
    }

    #[test]
    fn every_binding_parses_as_a_chord_and_a_shortcut() {
        for (title, chord) in BINDINGS
            .iter()
            .map(|(t, c, _)| (*t, *c))
            .chain([QUIT])
            .chain(ALIASES.iter().copied())
        {
            assert!(Chord::parse(chord).is_ok(), "{title}: {chord}");
            assert!(chord.parse::<Shortcut>().is_ok(), "{title}: {chord}");
        }
    }

    /// GPT review P2: ⌘ pressed or released alone is taken when it flips
    /// the held state, so the window rebuilds and the composer's send label
    /// ("Steer message" ↔ "Queue message") follows ⌘ live.
    #[test]
    fn a_command_press_alone_rebuilds_when_it_flips() {
        let mut s = crate::chat::fixtures::test_state();
        let mut tx = Tx::default();
        let press = |pressed: bool| KeyEvent {
            key: Key::Modifier {
                modifier: rune::Modifier::SuperLeft,
                pressed,
            },
            mods: rune::Mods {
                cmd: pressed,
                logo: pressed,
                ..rune::Mods::default()
            },
        };
        assert!(on_key(&mut s, &press(true), &mut tx), "held: rebuild");
        assert!(s.workspace.inbox.cmd_held);
        assert!(
            !on_key(&mut s, &press(true), &mut tx),
            "no change: not taken"
        );
        assert!(on_key(&mut s, &press(false), &mut tx), "released: rebuild");
        assert!(!s.workspace.inbox.cmd_held);
    }

    fn cmd(c: char) -> KeyEvent {
        KeyEvent {
            key: Key::Command(c),
            mods: rune::Mods {
                // The primary modifier alone: ⌘ on macOS, Ctrl on Linux.
                cmd: true,
                ..rune::Mods::default()
            },
        }
    }

    /// ⌘J toggles the middle column (the workspace panel) both ways, and
    /// ⌘E still does.
    #[test]
    fn command_j_and_e_toggle_the_workspace_panel() {
        let mut s = crate::chat::fixtures::test_state();
        crate::chat::fixtures::chat(&mut s);
        let mut tx = Tx::default();
        let open = |s: &State| crate::chat::side::dock_open(s, crate::chat::side::Dock::Work);
        let before = open(&s);
        assert!(on_key(&mut s, &cmd('j'), &mut tx), "⌘J is taken");
        assert_ne!(open(&s), before, "⌘J flips the panel");
        assert!(on_key(&mut s, &cmd('j'), &mut tx));
        assert_eq!(open(&s), before, "⌘J again flips it back");
        assert!(on_key(&mut s, &cmd('e'), &mut tx), "⌘E is an alias");
        assert_ne!(open(&s), before);
    }

    /// ⌘J slides the workspace panel open and shut through several frames
    /// (the menu's queued update asks for motion), like its titlebar button.
    #[test]
    fn command_j_slides_the_workspace_panel() {
        let mut s = crate::chat::fixtures::test_state();
        crate::chat::fixtures::chat(&mut s);
        s.update_prefs(|p| p.work_open = false);
        let mut rt = rune::Runtime::new(s, crate::view, (1400., 860.), rune::Env::new(true, 1.));
        rt.poll = Some(crate::poll);
        rt.frame(0.);
        for _ in 0..30 {
            rt.frame(1. / 60.);
        }
        let width = |rt: &rune::Runtime<State>| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some("side-work-dock"))
                .map_or(0., |e| e.rect[2])
        };
        let shut = width(&rt);
        let slide = |rt: &mut rune::Runtime<State>| {
            rt.update(action("Toggle Workspace Panel"));
            let mut seen = Vec::new();
            for _ in 0..40 {
                rt.frame(1. / 60.);
                seen.push(width(rt));
            }
            seen
        };
        let opening = slide(&mut rt);
        let open = *opening.last().expect("frames");
        assert!(open > shut + 100., "the panel opened: {shut} → {open}");
        let between = |seen: &[f32]| {
            seen.iter()
                .filter(|w| **w > shut + 1. && **w < open - 1.)
                .count()
        };
        assert!(between(&opening) >= 3, "it slides open: {opening:?}");
        let closing = slide(&mut rt);
        assert!(between(&closing) >= 3, "it slides shut: {closing:?}");
    }
}
