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

mod view;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::computer_use::{
    ComputerApp, ComputerControl, ComputerSession, ComputerState as WireState, FrameAnswer,
};
use rune::{Pixels, Tx};

use crate::permissions;
use crate::jobs::{self, Generation};
use crate::state::State;

pub use view::view;

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
/// Pointer events older than this are placed, not animated.
const FRESH_MS: i64 = 3000;
const TYPING_MS: i64 = 4000;
/// Animation frame interval while the cursor moves.
const FRAME: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Live,
    Still,
    Off,
}

/// A point as fractions of the picture (0..1, origin top-left).
type Frac = (f32, f32);

/// The cursor, as fractions of the picture.
#[derive(Debug, Clone, Copy)]
struct Glide {
    from: Frac,
    to: Frac,
    started: Instant,
}

impl Glide {
    fn at(&self) -> (Frac, bool) {
        let elapsed = Instant::now().saturating_duration_since(self.started);
        let t = (elapsed.as_secs_f32() / GLIDE.as_secs_f32()).min(1.);
        let e = 1. - (1. - t).powi(3);
        let lerp = |a: f32, b: f32| a + (b - a) * e;
        ((lerp(self.from.0, self.to.0), lerp(self.from.1, self.to.1)), t < 1.)
    }
}

struct Ripple {
    at: Frac,
    starts: Instant,
    right: bool,
}

struct Card {
    pixels: Pixels,
    has_image: bool,
    natural: (u32, u32),
    seq: Option<u64>,
    mode: Mode,
    width: u32,
    fps: f64,
    fetching: bool,
    backoff: Duration,
    refusal: Option<String>,
    glide: Option<Glide>,
    pointer_seq: u64,
    ripples: Vec<Ripple>,
    trail: Option<(Frac, Frac, Instant)>,
}

impl Card {
    fn new() -> Self {
        Self {
            pixels: Pixels::new(),
            has_image: false,
            natural: (0, 0),
            seq: None,
            mode: Mode::Off,
            width: 0,
            fps: 0.,
            fetching: false,
            backoff: Duration::from_secs(1),
            refusal: None,
            glide: None,
            pointer_seq: 0,
            ripples: Vec::new(),
            trail: None,
        }
    }

    fn animating(&self) -> bool {
        self.glide.is_some_and(|g| g.at().1) || !self.ripples.is_empty() || self.trail.is_some()
    }
}

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
                c.computer_control(&thread_id, ComputerControl::HandBack, &viewer).await
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

/// JPEG to straight RGBA.
fn decode(jpeg: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let rgba = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()? // silent-ok: a broken frame is skipped; the next one replaces it
        .to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// The share of the chat showing `thread_id` (main or side), while that
/// chat is open.
fn slot<'a>(s: &'a mut State, thread_id: &str) -> Option<&'a mut ComputerState> {
    crate::chat::chat_for(s, thread_id).map(|c| &mut c.computer)
}

/// The share, only while the chat (and the `start`) that asked is current.
fn same<'a>(s: &'a mut State, thread_id: &str, generation: Generation) -> Option<&'a mut ComputerState> {
    slot(s, thread_id).filter(|cu| cu.generation == generation)
}

/// Bind the share of the chat showing `id` to it and start polling the state.
pub fn start(s: &mut State, id: &str) {
    let backend = s.rt.backend.clone();
    let Some(chat) = crate::chat::chat_for(s, id) else { return };
    let cu = &mut chat.computer;
    cu.thread_id = chat.thread_id.clone();
    cu.backend = backend;
    let generation = cu.generation.bump();
    let thread_id = cu.thread_id.clone();
    poll_state(s, thread_id, generation);
}

fn poll_state(s: &mut State, thread_id: String, generation: Generation) {
    let Some(cu) = same(s, &thread_id, generation) else { return };
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
            let Some(cu) = same(s, &thread_id, generation) else { return };
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
    let Some(cu) = same(s, thread_id, generation) else { return };
    cu.enabled = state.enabled;
    cu.missing = if state.enabled { permissions::missing() } else { Vec::new() };
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
}

impl ComputerState {
    /// The windows shown, front first.
    fn apps(&self) -> Vec<ComputerApp> {
        let Some(session) = &self.session else { return Vec::new() };
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

/// Cards follow the windows; each holds a live, still or no fetch.
fn sync_cards(s: &mut State, id: &str) {
    let Some(cu) = slot(s, id) else { return };
    let visible = !cu.hidden && cu.session.is_some();
    let apps = if visible { cu.apps() } else { Vec::new() };
    let keep: Vec<u32> = apps.iter().map(|a| a.window_id).collect();
    cu.cards.retain(|w, _| keep.contains(w));
    if cu.big.is_some_and(|w| !keep.contains(&w)) {
        cu.big = None;
    }
    let paused = cu.paused();
    let session = cu.session.clone();
    let skew = cu.skew_ms;
    let mut start: Vec<u32> = Vec::new();
    for (ix, app) in apps.iter().enumerate() {
        let big = cu.big == Some(app.window_id);
        let (mode, width, fps) = match (paused, big, ix) {
            (true, _, _) => (Mode::Off, 0, 0.),
            (_, true, _) => (Mode::Live, 1280, 10.),
            (_, _, 0) => (Mode::Live, 720, 8.),
            _ => (Mode::Still, 400, 0.3),
        };
        let card = cu.cards.entry(app.window_id).or_insert_with(Card::new);
        card.mode = mode;
        card.width = width;
        card.fps = fps;
        if let Some(session) = &session {
            follow_pointer(card, session, app.window_id, skew);
        }
        if mode != Mode::Off && !card.fetching {
            card.fetching = true;
            start.push(app.window_id);
        }
    }
    let (thread_id, generation) = (cu.thread_id.clone(), cu.generation);
    for window in start {
        fetch(s, thread_id.clone(), generation, window);
    }
}

/// One loop per card: it follows the card's current mode and size, and ends
/// when the card stops fetching.
fn fetch(s: &mut State, thread_id: String, generation: Generation, window: u32) {
    let active = crate::workspace::window_active(s);
    let Some(cu) = same(s, &thread_id, generation) else { return };
    let Some(card) = cu.cards.get_mut(&window) else { return };
    if card.mode == Mode::Off {
        card.fetching = false;
        return;
    }
    if !active {
        // Nobody is watching: no capture until the window is back.
        jobs::after(s, Duration::from_secs(1), move |s, _| fetch(s, thread_id, generation, window));
        return;
    }
    let mode = card.mode;
    let width = card.width;
    let fps = if mode == Mode::Still { 0.3 } else { card.fps };
    let after = if mode == Mode::Live { card.seq } else { None };
    let thread = thread_id.clone();
    jobs::run(
        s,
        "computer use",
        move |c| async move {
            let answer = c.computer_frame(&thread, window, width, fps, after).await?;
            // Decode off the UI thread.
            let decoded = match &answer {
                FrameAnswer::Frame(frame) => {
                    let jpeg = frame.jpeg.clone();
                    tokio::task::spawn_blocking(move || decode(&jpeg)).await.ok().flatten()
                }
                _ => None,
            };
            Ok((answer, decoded))
        },
        move |s, result, _| {
            let Some(cu) = same(s, &thread_id, generation) else { return };
            let Some(card) = cu.cards.get_mut(&window) else { return };
            let pause = match result {
                Ok((FrameAnswer::Frame(frame), decoded)) => {
                    card.backoff = Duration::from_secs(1);
                    if let Some((w, h, rgba)) = decoded {
                        card.natural = (frame.width, frame.height);
                        card.seq = Some(frame.seq);
                        card.refusal = None;
                        card.pixels.set(w, h, rgba);
                        card.has_image = true;
                    }
                    (mode == Mode::Still).then_some(STILL_EVERY)
                }
                Ok((FrameAnswer::Unchanged, _)) => (mode == Mode::Still).then_some(STILL_EVERY),
                Ok((FrameAnswer::Refused { code, detail }, _)) => {
                    card.refusal = Some(match (code.as_str(), detail) {
                        ("human_has_control", _) => "Paused while you have control".into(),
                        (_, Some(detail)) => format!("No picture: {detail}"),
                        ("ended", None) => "The session ended".into(),
                        _ => "No picture yet".into(),
                    });
                    let wait = card.backoff;
                    card.backoff = (card.backoff * 2).min(Duration::from_secs(10));
                    Some(wait)
                }
                Err(e) => {
                    tracing::debug!(error = %e, "computer-use frame fetch failed");
                    let wait = card.backoff;
                    card.backoff = (card.backoff * 2).min(Duration::from_secs(10));
                    Some(wait)
                }
            };
            match pause {
                Some(pause) => jobs::after(s, pause, move |s, _| fetch(s, thread_id, generation, window)),
                None => fetch(s, thread_id, generation, window),
            }
        },
    );
}

fn control(s: &mut State, id: &str, action: ComputerControl) {
    let Some(cu) = slot(s, id) else { return };
    if cu.busy {
        return;
    }
    cu.busy = true;
    cu.note = None;
    let (thread_id, viewer, generation) = (cu.thread_id.clone(), cu.viewer.clone(), cu.generation);
    let asked = thread_id.clone();
    jobs::run(
        s,
        "computer use",
        move |c| async move { c.computer_control(&thread_id, action, &viewer).await },
        move |s, result, _| {
            let Some(cu) = same(s, &asked, generation) else { return };
            cu.busy = false;
            match result {
                Ok(_) => {
                    match action {
                        ComputerControl::TakeOver => cu.mine = true,
                        ComputerControl::HandBack | ComputerControl::Stop => cu.mine = false,
                    }
                    sync_cards(s, &asked);
                }
                Err(e) => {
                    cu.note = Some(
                        match action {
                            ComputerControl::Stop => "Could not stop the agent",
                            ComputerControl::TakeOver => "Could not take over",
                            ComputerControl::HandBack => "Could not hand back",
                        }
                        .into(),
                    );
                    tracing::warn!(error = %e, "computer-use control failed");
                }
            }
        },
    );
}

fn stop(s: &mut State, id: &str) {
    control(s, id, ComputerControl::Stop);
    // The Hermes viewer also interrupts the whole turn.
    crate::chat::stop_run(s, id);
}

fn take_over_or_back(s: &mut State, id: &str) {
    let Some(cu) = slot(s, id) else { return };
    let action = if cu.mine { ComputerControl::HandBack } else { ComputerControl::TakeOver };
    control(s, id, action);
}

fn click_card(s: &mut State, id: &str, window: u32, front: bool) {
    let Some(cu) = slot(s, id) else { return };
    if front {
        cu.big = Some(window);
    } else if let Some(first) = cu.apps().first() {
        cu.pinned = Some((window, first.window_id));
    }
    sync_cards(s, id);
}

fn close_big(s: &mut State, id: &str) {
    if let Some(cu) = slot(s, id) {
        cu.big = None;
    }
    sync_cards(s, id);
}

/// Glide the card's cursor to the session's pointer when it targets this
/// window; ripple on fresh clicks, trail on fresh drags.
fn follow_pointer(card: &mut Card, session: &ComputerSession, window: u32, skew_ms: i64) {
    let Some(p) = session.pointer.as_ref().filter(|p| p.window_id == window) else {
        card.glide = None;
        return;
    };
    let to = (p.x as f32, p.y as f32);
    if p.seq == card.pointer_seq {
        return;
    }
    let first = card.pointer_seq == 0;
    card.pointer_seq = p.seq;
    let fresh = now_ms() - skew_ms - p.at < FRESH_MS;
    let from = card.glide.map_or(to, |g| g.at().0);
    card.glide = Some(Glide { from: if first || !fresh { to } else { from }, to, started: Instant::now() });
    if first || !fresh {
        return;
    }
    if p.kind.contains("click") {
        let right = p.kind == "right_click";
        card.ripples.push(Ripple { at: to, starts: Instant::now() + GLIDE, right });
        if p.kind == "double_click" {
            card.ripples.push(Ripple {
                at: to,
                starts: Instant::now() + GLIDE + Duration::from_millis(180),
                right: false,
            });
        }
    }
    if p.kind == "drag"
        && let (Some(x2), Some(y2)) = (p.to_x, p.to_y)
    {
        let end = (x2 as f32, y2 as f32);
        card.trail = Some((to, end, Instant::now()));
        // The cursor follows the drag once it reached the start.
        card.glide = Some(Glide { from: to, to: end, started: Instant::now() + GLIDE });
    }
}

/// Expire finished animations; keep drawing while any runs.
pub fn poll(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    let Some(cu) = slot(s, id) else { return false };
    if cu.hidden || cu.session.is_none() {
        return false;
    }
    let now = Instant::now();
    let mut animating = false;
    for card in cu.cards.values_mut() {
        card.ripples.retain(|r| now < r.starts + RIPPLE);
        if card.trail.is_some_and(|(_, _, at)| at.elapsed() > TRAIL) {
            card.trail = None;
        }
        animating |= card.animating();
    }
    if animating {
        tx.wake_after(FRAME);
    }
    animating
}

/// A session with three windows, a pointer mid-glide and typing, for the
/// headless scene. The cards have no picture (no gateway), so they show why.
pub fn fixture(thread_id: &str) -> ComputerState {
    use ironclaw_gpui_client::computer_use::{ComputerCapture, ComputerPointer, ComputerTyping};
    let now = now_ms();
    let app = |window_id: u32, name: &str, title: &str, age: f64| ComputerApp {
        pid: 1,
        window_id,
        app: name.into(),
        title: title.into(),
        width: Some(1440),
        height: Some(900),
        capture: ComputerCapture { method: Some("window".into()), error: None, frame_age: Some(age) },
    };
    let session = ComputerSession {
        thread_id: thread_id.into(),
        apps: vec![app(11, "Safari", "IronClaw docs", 1.), app(12, "Notes", "Port plan", 12.), app(13, "Terminal", "zsh", 90.)],
        inflight: 1,
        label: "Clicking “Install”".into(),
        status: None,
        ok: None,
        pointer: Some(ComputerPointer {
            kind: "move".into(),
            x: 0.42,
            y: 0.36,
            to_x: None,
            to_y: None,
            window_id: 11,
            seq: 7,
            at: now - 10_000,
        }),
        typing: Some(ComputerTyping { text: "rune port".into(), at: now, window_id: Some(11) }),
        last_activity: now,
        ended: false,
        last_failure: None,
        human_has_control: false,
        stopping: false,
        watchable: true,
    };
    let mut cu = ComputerState::default();
    cu.thread_id = thread_id.into();
    cu.session = Some(session);
    for (ix, window) in [11, 12, 13].into_iter().enumerate() {
        let mut card = Card::new();
        card.mode = if ix == 0 { Mode::Live } else { Mode::Still };
        if ix == 0 {
            card.pointer_seq = 7;
            card.glide = Some(Glide { from: (0.42, 0.36), to: (0.42, 0.36), started: Instant::now() });
            // A grey test picture, so the scene shows image placement.
            let (w, h) = (16u32, 10u32);
            card.pixels.set(w, h, [40u8, 44, 52, 255].repeat((w * h) as usize));
            card.natural = (1440, 900);
            card.has_image = true;
        }
        cu.cards.insert(window, card);
    }
    cu
}

/// Scene `computer` (`big`: `computer-big`): the screen share's stack over
/// the chat, or its front card enlarged.
pub fn scene(s: &mut State, big: bool) {
    crate::scenes::chat(s);
    if let Some(c) = s.chat.as_mut() {
        c.computer = fixture("t1");
        if big {
            c.computer.big = Some(11);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WireState, apply, fixture};
    use crate::chat::fixtures::{close_side_chat, test_state, with_side_chat};
    use crate::chat::{chat_for, chat_ref};
    use crate::jobs::Generation;

    fn wire() -> WireState {
        WireState { enabled: false, now: 0, sessions: fixture("sc2").session.clone().into_iter().collect() }
    }

    #[test]
    fn a_poll_lands_in_its_own_chat_and_is_dropped_once_that_chat_closed() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        if let Some(c) = chat_for(&mut s, "sc2") {
            c.computer.thread_id = "sc2".into();
        }
        apply(&mut s, "sc2", Generation::default(), wire());
        assert!(chat_ref(&s, "sc2").is_some_and(|c| c.computer.session.is_some()));
        assert!(chat_ref(&s, "t1").is_some_and(|c| c.computer.session.is_none()), "the open chat's share is its own");
        close_side_chat(&mut s);
        apply(&mut s, "sc2", Generation::default(), wire());
        assert!(chat_ref(&s, "t1").is_some_and(|c| c.computer.session.is_none()));
    }
}
