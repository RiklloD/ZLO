//! The computer-use screen share (the Hermes plugin's Desktop viewer). Port
//! of GPUI `chat/computer.rs`; the view is in `computer/view.rs`.
//!
//! While the agent in this conversation drives apps, a fanned stack of cards
//! floats at the top right: one per window it used, newest in front. The
//! front card is a live window capture; the cards behind refresh every few
//! seconds. The agent's cursor glides to each target, ripples on clicks and
//! draws drags, and a badge shows typing. Click a card behind to bring it
//! forward, the front card to enlarge it. Stop and Take over / Hand back sit
//! under the stack, with the frame's age or why there is no picture.
//!
//! The gateway serves the state and one window's picture (long-polled); this
//! view polls the state fast only while the agent is driving, and fetches
//! pictures only for the cards on screen.
//!
//! # Entry points
//!
//! Every chat has its own share, the open one and each side chat (GPUI:
//! each `ChatView` owned a `ComputerShare`); functions take the chat's
//! thread id and find it with `chat::chat_for`, so a poll or a frame for a
//! chat that closed meanwhile goes nowhere (and dropping the share hands a
//! taken-over desktop back).
//!
//! - `start(s, id)`: after a chat starts.
//! - `poll(s, id, tx) -> bool`: from `chat::poll`, drives the cursor animation.
//! - `view(s, chat, cx, k, layer) -> Option<El>`: the stack, pinned to the
//!   top right of its parent (area A puts it in the chat pane's relative
//!   container); the enlarged card goes into `layer`.
//! - `has_session(s, id)`, `hidden(s, id)`, `toggle_hidden(s, id)`: the side
//!   pane's screen-share button (area S).

mod cards;
mod control;
mod fixtures;
#[cfg(test)]
mod motion_tests;
mod pointer;
mod remote;
#[cfg(test)]
mod tests;
mod view;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rune::{Pixels, Tx};
use zlo_client::Backend;
use zlo_client::computer_use::{
    ComputerApp, ComputerControl, ComputerSession, ComputerState as WireState, FrameAnswer,
};

use crate::jobs::{self, Generation};
use crate::permissions;
use crate::state::State;

pub use fixtures::scene;
pub use pointer::poll;
pub use view::view;

use self::cards::{Card, Mode, click_card, close_big, sync_cards};
#[cfg(test)]
use self::control::{HANDED_BACK, control_landed};
use self::control::{stop, take_over_or_back};
#[cfg(test)]
use self::fixtures::fixture;
use self::pointer::{Frac, Glide, Ripple, follow_pointer, trail_alpha};

const POLL_ACTIVE: Duration = Duration::from_millis(700);
const POLL_IDLE: Duration = Duration::from_secs(4);
const POLL_BACKGROUND: Duration = Duration::from_secs(15);
const POLL_OFF: Duration = Duration::from_secs(60);
/// Refresh period of the cards behind the front one.
const STILL_EVERY: Duration = Duration::from_secs(3);
const MAX_CARDS: usize = 4;
const GLIDE: Duration = Duration::from_millis(550);
const RIPPLE: Duration = Duration::from_millis(600);
const TRAIL: Duration = Duration::from_millis(1800);
/// The drag trail fades out over the last part of its life.
const TRAIL_FADE: Duration = Duration::from_millis(400);
/// Pointer events older than this are placed, not animated.
const FRESH_MS: i64 = 3000;
const TYPING_MS: i64 = 4000;
/// Animation frame interval while the cursor moves.
const FRAME: Duration = Duration::from_millis(16);

pub struct ComputerState {
    thread_id: String,
    viewer: String,
    /// Kept to hand the desktop back when the chat closes.
    backend: Option<Backend>,
    enabled: bool,
    session: Option<ComputerSession>,
    /// Local clock minus the gateway's, in ms.
    skew_ms: i64,
    hidden: bool,
    /// This viewer holds the desktop (Take over).
    mine: bool,
    cards: HashMap<u32, Card>,
    pinned: Option<(u32, u32)>,
    big: Option<u32>,
    busy: bool,
    note: Option<String>,
    /// macOS permissions ZLO still lacks while computer use is on.
    missing: Vec<permissions::Permission>,
    /// A remote backend's own readiness (its engine and permissions); this
    /// Mac's are never asked for it.
    remote: Option<remote::Remote>,
    permissions_hidden: bool,
    /// Invalidates the poll loop of an earlier `start`.
    generation: Generation,
}

impl Default for ComputerState {
    fn default() -> Self {
        Self {
            thread_id: String::new(),
            viewer: viewer_id(),
            backend: None,
            enabled: true,
            session: None,
            skew_ms: 0,
            hidden: false,
            mine: false,
            cards: HashMap::new(),
            pinned: None,
            big: None,
            busy: false,
            note: None,
            missing: Vec::new(),
            remote: None,
            permissions_hidden: false,
            generation: Generation::default(),
        }
    }
}

impl Drop for ComputerState {
    /// A take over must not outlive its viewer: hand the desktop back.
    fn drop(&mut self) {
        if self.mine
            && let Some(backend) = &self.backend
        {
            let thread_id = self.thread_id.clone();
            let viewer = self.viewer.clone();
            let _detached = backend.spawn(move |c| async move {
                c.computer_control(&thread_id, ComputerControl::HandBack, &viewer)
                    .await
            });
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn viewer_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("zlo{:x}{:x}", std::process::id(), nanos & 0xffff_ffff)
}

/// The share of the chat showing `thread_id` (main or side), while that
/// chat is open.
fn slot<'a>(s: &'a mut State, thread_id: &str) -> Option<&'a mut ComputerState> {
    crate::chat::chat_for(s, thread_id).map(|c| &mut c.computer)
}

/// The share, only while the chat (and the `start`) that asked is current.
fn same<'a>(
    s: &'a mut State,
    thread_id: &str,
    generation: Generation,
) -> Option<&'a mut ComputerState> {
    slot(s, thread_id).filter(|cu| cu.generation == generation)
}

/// Bind the share of the chat showing `id` to it and start polling the state.
pub fn start(s: &mut State, id: &str) {
    let backend = s.rt.backend.clone();
    let Some(chat) = crate::chat::chat_for(s, id) else {
        return;
    };
    let cu = &mut chat.computer;
    cu.thread_id = chat.thread_id.clone();
    cu.backend = backend;
    let generation = cu.generation.bump();
    let thread_id = cu.thread_id.clone();
    poll_state(s, thread_id, generation);
}

fn poll_state(s: &mut State, thread_id: String, generation: Generation) {
    let Some(cu) = same(s, &thread_id, generation) else {
        return;
    };
    let viewer = cu.viewer.clone();
    jobs::run(
        s,
        "computer use",
        move |c| async move { c.computer_state(&viewer).await },
        move |s, result, _| {
            let active = crate::workspace::window_active(s);
            match result {
                Ok(state) => apply(s, &thread_id, generation, state),
                // silent-ok: an older gateway has no live view route, and a
                // failed poll is retried on the next tick.
                Err(e) => {
                    tracing::debug!(error = %e, "computer-use state poll failed");
                    if e.is_not_found()
                        && let Some(cu) = same(s, &thread_id, generation)
                    {
                        cu.enabled = false;
                    }
                }
            }
            let Some(cu) = same(s, &thread_id, generation) else {
                return;
            };
            let delay = if !cu.enabled {
                POLL_OFF
            } else if !active {
                POLL_BACKGROUND
            } else if cu.session.as_ref().is_some_and(|s| !s.ended) || cu.mine {
                POLL_ACTIVE
            } else {
                POLL_IDLE
            };
            jobs::after(s, delay, move |s, _| poll_state(s, thread_id, generation));
        },
    );
}

fn apply(s: &mut State, thread_id: &str, generation: Generation, state: WireState) {
    let remote = crate::remote::backend_is_remote(s);
    let Some(cu) = same(s, thread_id, generation) else {
        return;
    };
    cu.enabled = state.enabled;
    cu.missing = if state.enabled {
        permissions::missing_for_backend(remote)
    } else {
        Vec::new()
    };
    if !remote {
        cu.remote = None;
    }
    let ask_backend = remote && state.enabled && remote::due(cu.remote.as_ref(), Instant::now());
    if state.now > 0 {
        cu.skew_ms = now_ms() - state.now;
    }
    let session = state
        .sessions
        .into_iter()
        .find(|x| x.thread_id == cu.thread_id && x.watchable && !x.apps.is_empty());
    // A new session in this conversation reopens a stack hidden for an
    // earlier one.
    if session.is_some() && cu.session.is_none() {
        cu.hidden = false;
    }
    if cu.mine && session.as_ref().is_some_and(|x| !x.human_has_control) {
        cu.mine = false; // released elsewhere (Stop, or another viewer)
    }
    cu.session = session;
    sync_cards(s, thread_id);
    if ask_backend {
        remote::fetch(s, thread_id.to_string(), generation);
    }
}

impl ComputerState {
    /// The windows shown, front first.
    fn apps(&self) -> Vec<ComputerApp> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let mut apps: Vec<ComputerApp> = session.apps.iter().take(MAX_CARDS).cloned().collect();
        if let Some((window, front)) = self.pinned
            && apps.first().is_some_and(|a| a.window_id == front)
            && let Some(ix) = apps.iter().position(|a| a.window_id == window)
        {
            let app = apps.remove(ix);
            apps.insert(0, app);
        }
        apps
    }

    fn paused(&self) -> bool {
        self.mine || self.session.as_ref().is_some_and(|s| s.human_has_control)
    }
}

/// The agent is (or just was) driving apps in `id`'s conversation.
pub fn has_session(s: &State, id: &str) -> bool {
    crate::chat::chat_ref(s, id).is_some_and(|c| c.computer.session.is_some() || c.computer.mine)
}

pub fn hidden(s: &State, id: &str) -> bool {
    crate::chat::chat_ref(s, id).is_some_and(|c| c.computer.hidden)
}

pub fn toggle_hidden(s: &mut State, id: &str) {
    if let Some(cu) = slot(s, id) {
        cu.hidden = !cu.hidden;
    }
    sync_cards(s, id);
}
