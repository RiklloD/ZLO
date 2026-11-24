//! What a send does while a turn runs, as t3code decides it
//! (`resolveComposerDispatchMode`, `composerSubmissionIntentForKey`,
//! `ComposerPrimaryActions`). Pure: the composer, its send button and the
//! queue panel ask here, and the tests pin it.
//!
//! - Idle, a send starts a turn.
//! - While a turn runs, Enter does Settings → Follow-ups (Steer by default)
//!   and ⌘↵ or ⌘-click does the other, for that one message.
//! - Steering waits only for compaction: a message sent while the turn is
//!   compacting queues.

use rune::{Icon, Key, KeyEvent};

use crate::design::prefs::FollowUps;
use crate::icons;

/// What one send does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// No turn runs: it starts one.
    Start,
    /// It joins the running turn.
    Steer,
    /// It waits in the queue for the turn to end.
    Queue,
}

/// t3code's `resolveComposerDispatchMode`. `alternate`: ⌘↵ or ⌘-click.
pub(crate) fn resolve(running: bool, alternate: bool, default: FollowUps, can_steer: bool) -> Mode {
    if !running {
        return Mode::Start;
    }
    if !can_steer {
        return Mode::Queue;
    }
    match (default, alternate) {
        (FollowUps::Steer, false) | (FollowUps::Queue, true) => Mode::Steer,
        (FollowUps::Queue, false) | (FollowUps::Steer, true) => Mode::Queue,
    }
}

/// Whether the running turn can take a steer: not while it compacts its
/// context (the backend's `compacting`, read from the queue view, which
/// knows the active run's prompt; t3code's `compactionBlocksSteering`).
pub(crate) fn can_steer(chat: &super::ChatState) -> bool {
    !chat.orchestration.queue.compacting
}

/// The send button's face.
#[derive(Clone, Debug)]
pub(crate) struct SendLook {
    /// Its accessible name.
    pub label: &'static str,
    pub icon: &'static Icon,
    pub help: String,
}

/// What the send button shows. `mode` is what a plain click does now (with
/// ⌘ held it is already the alternate); `editing`: a queued message is in
/// the composer; `resume`: the queue is held after Stop and the draft is
/// empty.
pub(crate) fn send_look(
    mode: Mode,
    default: FollowUps,
    can_steer: bool,
    editing: bool,
    resume: bool,
) -> SendLook {
    if editing {
        return SendLook {
            label: "Update queued message",
            icon: &icons::CHECK,
            help: match mode {
                Mode::Start => "Save the queued message (Enter)".into(),
                _ => "Save the queued message (Enter) · save and steer (⌘↩)".into(),
            },
        };
    }
    match mode {
        Mode::Start if resume => SendLook {
            label: "Resume thread",
            icon: &icons::PLAY,
            help: "Resume thread: run the queued messages".into(),
        },
        Mode::Start => SendLook {
            label: "Send",
            icon: &icons::ARROW_UP,
            help: "Send (Enter)".into(),
        },
        Mode::Steer | Mode::Queue => SendLook {
            label: if mode == Mode::Steer {
                "Steer message"
            } else {
                "Queue message"
            },
            icon: if mode == Mode::Steer {
                &icons::CORNER_UP_RIGHT
            } else {
                &icons::LIST_PLUS
            },
            help: running_help(default, can_steer),
        },
    }
}

/// The send button's tooltip while a turn runs (t3code's wording).
pub(crate) fn running_help(default: FollowUps, can_steer: bool) -> String {
    if !can_steer {
        return "Queue message: steering waits until compaction finishes".into();
    }
    match default {
        FollowUps::Steer => "Click to steer, ⌘-click or ⌘↩ to queue".into(),
        FollowUps::Queue => "Click to queue, ⌘-click or ⌘↩ to steer".into(),
    }
}

/// Settings → Follow-ups, under its title.
pub(crate) const FOLLOW_UPS_HELP: &str = "Steer the current turn with messages you send while the agent works, or queue them for after it. Press ⌘ + Enter to do the opposite for one message.";

/// What the composer has, for its keys.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KeyContext {
    pub running: bool,
    /// Nothing typed and no files.
    pub draft_empty: bool,
    pub caret_at_start: bool,
    /// Messages in the queue that can be sent or edited (not `sending`).
    pub queued: usize,
    /// A queued message is being edited in the composer.
    pub editing: bool,
    /// An open approval: `Some(allow_always)`.
    pub approval: Option<bool>,
    /// The OS auto-repeat of a held key.
    pub repeat: bool,
}

/// A composer key the chat handles itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAction {
    /// ⌘↵ while a turn runs: the other of steer and queue, once.
    SendAlternate,
    /// ⌘↵ idle: an ordinary send.
    Send,
    /// ⇧⌘↵: send the first queued message now.
    SendFirstQueued,
    /// ⌥↑ with the caret at the start: edit the last queued message.
    EditLastQueued,
    /// ⌥⌘↵ approves the open approval, ⌥⇧⌘↵ always allows it. Never a send
    /// or queue chord, so a send can't grant a tool.
    Approve { always: bool },
    /// A held key's repeat of one of the above: taken, and nothing done.
    Swallow,
}

/// The composer's own keys. `None` leaves the key to the field (and the
/// `/` menu). Approval has its own chord (⌥ added), apart from every send
/// and queue chord, and only with an open approval. A key repeat of a
/// chord does nothing: one press, one action.
pub(crate) fn key_action(ev: &KeyEvent, cx: KeyContext) -> Option<KeyAction> {
    let m = ev.mods;
    let action = match ev.key {
        Key::Enter if m.cmd && m.alt && !m.ctrl_only() => {
            cx.approval.map(|always| KeyAction::Approve {
                always: m.shift && always,
            })
        }
        Key::Enter if m.cmd && !m.alt && !m.ctrl_only() => {
            if m.shift {
                (cx.queued > 0).then_some(KeyAction::SendFirstQueued)
            } else if !cx.draft_empty || cx.editing {
                Some(if cx.running || cx.editing {
                    KeyAction::SendAlternate
                } else {
                    KeyAction::Send
                })
            } else {
                None
            }
        }
        Key::Up if m.alt && !m.cmd && !m.shift && cx.caret_at_start && cx.queued > 0 => {
            Some(KeyAction::EditLastQueued)
        }
        _ => None,
    };
    match action {
        Some(_) if cx.repeat => Some(KeyAction::Swallow),
        other => other,
    }
}

/// `Mods` helpers the key map reads.
trait ModsExt {
    /// Control held off macOS-style ⌘ (Ctrl+Enter on a Mac is not ⌘↵).
    fn ctrl_only(&self) -> bool;
}

impl ModsExt for rune::Mods {
    fn ctrl_only(&self) -> bool {
        cfg!(target_os = "macos") && self.ctrl && !self.logo
    }
}

/// t3code's `QUEUE_VISIBLE_ROWS`: up to this many queued messages show;
/// a longer queue shows its first two and "Show all n queued".
pub(crate) const QUEUE_VISIBLE_ROWS: usize = 3;

/// How many of `n` queued messages show, and the toggle's label if any.
pub(crate) fn queue_rows(n: usize, expanded: bool) -> (usize, Option<String>) {
    if n <= QUEUE_VISIBLE_ROWS {
        return (n, None);
    }
    if expanded {
        (n, Some("Show fewer".into()))
    } else {
        (QUEUE_VISIBLE_ROWS - 1, Some(format!("Show all {n} queued")))
    }
}

/// The steer marker's label and tooltip (t3code `UserMessageIntentMarker`).
pub(crate) fn steer_marker(waiting: bool) -> (&'static str, &'static str) {
    if waiting {
        (
            "Steer · waiting to be read",
            "Waiting for the agent to read this. It reads steers between steps, after a running command finishes.",
        )
    } else {
        ("Steer", "Steered the active turn")
    }
}

/// The fold line of the work after a steer: "Continued for 3m 5s".
pub(crate) fn continued_label(ms: Option<i64>) -> String {
    match ms {
        Some(ms) if ms >= 1000 => format!("Continued for {}", super::flow::duration(ms)),
        _ => "Continued".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn key(key: Key, mods: Mods) -> KeyEvent {
        KeyEvent { key, mods }
    }

    fn cmd() -> Mods {
        Mods {
            cmd: true,
            logo: cfg!(target_os = "macos"),
            ctrl: !cfg!(target_os = "macos"),
            ..Mods::default()
        }
    }

    #[test]
    fn idle_starts_running_steers_by_default_and_the_alternate_flips_it() {
        use FollowUps::*;
        assert_eq!(resolve(false, false, Steer, true), Mode::Start);
        assert_eq!(resolve(false, true, Queue, true), Mode::Start);
        assert_eq!(resolve(true, false, Steer, true), Mode::Steer);
        assert_eq!(resolve(true, true, Steer, true), Mode::Queue);
        assert_eq!(resolve(true, false, Queue, true), Mode::Queue);
        assert_eq!(resolve(true, true, Queue, true), Mode::Steer);
        // Only compaction blocks steering, and then everything queues.
        assert_eq!(resolve(true, false, Steer, false), Mode::Queue);
        assert_eq!(resolve(true, true, Queue, false), Mode::Queue);
        assert_eq!(FollowUps::default(), Steer, "t3code's default");
    }

    #[test]
    fn a_compacting_turn_cannot_be_steered() {
        // The backend's word, not a guess from the transcript: a `/compact`
        // prompt row alone does not block steering, the queue view does.
        let mut chat = crate::chat::ChatState::new("t1".into());
        assert!(can_steer(&chat));
        chat.orchestration.queue.compacting = true;
        assert!(!can_steer(&chat));
    }

    #[test]
    fn the_send_button_names_what_a_click_does() {
        let look = send_look(Mode::Steer, FollowUps::Steer, true, false, false);
        assert_eq!(look.label, "Steer message");
        assert_eq!(look.icon.name, "corner-up-right");
        assert_eq!(look.help, "Click to steer, ⌘-click or ⌘↩ to queue");
        // ⌘ held: the same button now queues, its tooltip stays the rule.
        let held = send_look(Mode::Queue, FollowUps::Steer, true, false, false);
        assert_eq!(held.label, "Queue message");
        assert_eq!(held.icon.name, "list-plus");
        let queue_first = send_look(Mode::Queue, FollowUps::Queue, true, false, false);
        assert_eq!(queue_first.help, "Click to queue, ⌘-click or ⌘↩ to steer");
        let resume = send_look(Mode::Start, FollowUps::Steer, true, false, true);
        assert_eq!((resume.label, resume.icon.name), ("Resume thread", "play"));
        let edit = send_look(Mode::Steer, FollowUps::Steer, true, true, false);
        assert_eq!(edit.label, "Update queued message");
    }

    #[test]
    fn composer_keys_follow_t3code() {
        let running = KeyContext {
            running: true,
            queued: 2,
            ..KeyContext::default()
        };
        let enter_cmd = key(Key::Enter, cmd());
        assert_eq!(
            key_action(&enter_cmd, running),
            Some(KeyAction::SendAlternate)
        );
        let idle = KeyContext {
            running: false,
            ..running
        };
        assert_eq!(key_action(&enter_cmd, idle), Some(KeyAction::Send));
        // Plain Enter is the field's submit; ⌥Enter no longer queues.
        assert_eq!(key_action(&key(Key::Enter, Mods::default()), running), None);
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        assert_eq!(key_action(&key(Key::Enter, alt), running), None);
        // ⌥↑ edits the last queued message only from the caret's start.
        let up = key(Key::Up, alt);
        assert_eq!(key_action(&up, running), None);
        let at_start = KeyContext {
            caret_at_start: true,
            ..running
        };
        assert_eq!(key_action(&up, at_start), Some(KeyAction::EditLastQueued));
        let none_queued = KeyContext {
            queued: 0,
            ..at_start
        };
        assert_eq!(key_action(&up, none_queued), None);
    }

    /// GPT review P1: a send or queue chord must never grant a tool. With a
    /// queued item, an empty composer and an open approval, ⇧⌘↵ sends the
    /// queued item (what the row advertises), ⌘↵ does nothing, and only
    /// the ⌥ chords approve.
    #[test]
    fn send_and_queue_chords_never_approve() {
        let shift_cmd = Mods {
            shift: true,
            ..cmd()
        };
        let alt_cmd = Mods { alt: true, ..cmd() };
        let alt_shift_cmd = Mods {
            alt: true,
            shift: true,
            ..cmd()
        };
        let gate = KeyContext {
            running: true,
            draft_empty: true,
            queued: 1,
            approval: Some(true),
            ..KeyContext::default()
        };
        assert_eq!(
            key_action(&key(Key::Enter, shift_cmd), gate),
            Some(KeyAction::SendFirstQueued)
        );
        assert_eq!(key_action(&key(Key::Enter, cmd()), gate), None);
        assert_eq!(
            key_action(&key(Key::Enter, alt_cmd), gate),
            Some(KeyAction::Approve { always: false })
        );
        assert_eq!(
            key_action(&key(Key::Enter, alt_shift_cmd), gate),
            Some(KeyAction::Approve { always: true })
        );
        let no_queue = KeyContext { queued: 0, ..gate };
        assert_eq!(key_action(&key(Key::Enter, shift_cmd), no_queue), None);
        let no_gate = KeyContext {
            approval: None,
            ..gate
        };
        assert_eq!(key_action(&key(Key::Enter, alt_cmd), no_gate), None);
    }

    /// GPT review P1: holding ⌘↵ or ⇧⌘↵ acts once; the repeats are taken
    /// and do nothing (no approval after the composer clears, no second
    /// queued item sent).
    #[test]
    fn a_held_chord_acts_once() {
        let held = KeyContext {
            running: true,
            queued: 3,
            repeat: true,
            approval: Some(false),
            ..KeyContext::default()
        };
        let shift_cmd = Mods {
            shift: true,
            ..cmd()
        };
        assert_eq!(
            key_action(&key(Key::Enter, shift_cmd), held),
            Some(KeyAction::Swallow)
        );
        let cleared = KeyContext {
            draft_empty: true,
            ..held
        };
        assert_eq!(
            key_action(&key(Key::Enter, cmd()), cleared),
            None,
            "no approval either"
        );
        let typed = KeyContext {
            draft_empty: false,
            ..held
        };
        assert_eq!(
            key_action(&key(Key::Enter, cmd()), typed),
            Some(KeyAction::Swallow)
        );
    }

    #[test]
    fn a_long_queue_shows_two_and_a_toggle() {
        assert_eq!(queue_rows(3, false), (3, None));
        assert_eq!(queue_rows(5, false), (2, Some("Show all 5 queued".into())));
        assert_eq!(queue_rows(5, true), (5, Some("Show fewer".into())));
    }

    #[test]
    fn steer_markers_read_like_t3code() {
        assert_eq!(steer_marker(true).0, "Steer · waiting to be read");
        assert_eq!(steer_marker(false), ("Steer", "Steered the active turn"));
        assert_eq!(continued_label(Some(185_000)), "Continued for 3m 5s");
        assert_eq!(continued_label(None), "Continued");
    }
}
