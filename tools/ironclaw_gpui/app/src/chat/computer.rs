//! The computer-use screen share (the Hermes plugin's Desktop viewer).
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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::computer_use::{
    ComputerApp, ComputerControl, ComputerSession, ComputerState, FrameAnswer,
};

use crate::design::Palette;
use crate::jobs::run;
use crate::ui::*;

const POLL_ACTIVE: Duration = Duration::from_millis(700);
const POLL_IDLE: Duration = Duration::from_secs(4);
const POLL_BACKGROUND: Duration = Duration::from_secs(15);
const POLL_OFF: Duration = Duration::from_secs(60);
/// Refresh period of the cards behind the front one.
const STILL_EVERY: Duration = Duration::from_secs(3);
const MAX_CARDS: usize = 4;
const CARD_W: f32 = 300.;
const STAGE_H: f32 = 200.;
const DECK_W: f32 = 560.;
const GLIDE: Duration = Duration::from_millis(550);
const RIPPLE: Duration = Duration::from_millis(600);
const TRAIL: Duration = Duration::from_millis(1800);
/// Pointer events older than this are placed, not animated.
const FRESH_MS: i64 = 3000;
const TYPING_MS: i64 = 4000;
const INDIGO: u32 = 0x6366f1;
const INDIGO_SOFT: u32 = 0x818cf8;
const AMBER: u32 = 0xf59e0b;

/// The share asked the chat to stop its turn as well.
pub struct StopRun;

impl EventEmitter<StopRun> for ComputerShare {}

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
    from: (f32, f32),
    to: (f32, f32),
    started: Instant,
}

impl Glide {
    fn at(&self) -> ((f32, f32), bool) {
        let t = (self.started.elapsed().as_secs_f32() / GLIDE.as_secs_f32()).min(1.);
        let e = 1. - (1. - t).powi(3);
        let lerp = |a: f32, b: f32| a + (b - a) * e;
        (
            (lerp(self.from.0, self.to.0), lerp(self.from.1, self.to.1)),
            t < 1.,
        )
    }
}

struct Ripple {
    at: (f32, f32),
    starts: Instant,
    right: bool,
}

struct Card {
    image: Option<Arc<RenderImage>>,
    natural: (u32, u32),
    seq: Option<u64>,
    mode: Mode,
    width: u32,
    fps: f64,
    fetching: bool,
    refusal: Option<String>,
    glide: Option<Glide>,
    pointer_seq: u64,
    ripples: Vec<Ripple>,
    trail: Option<(Frac, Frac, Instant)>,
}

impl Card {
    fn new() -> Self {
        Self {
            image: None,
            natural: (0, 0),
            seq: None,
            mode: Mode::Off,
            width: 0,
            fps: 0.,
            fetching: false,
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

pub struct ComputerShare {
    backend: Backend,
    thread_id: String,
    viewer: String,
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
    note: Option<SharedString>,
    /// macOS permissions Numoh still lacks while computer use is on.
    missing: Vec<crate::permissions::Permission>,
    permissions_hidden: bool,
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
    format!("numoh{:x}{:x}", std::process::id(), nanos & 0xffff_ffff)
}

fn decode(jpeg: &[u8]) -> Option<Arc<RenderImage>> {
    let rgba = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()? // silent-ok: a broken frame is skipped; the next one replaces it
        .to_rgba8();
    let mut bgra = rgba;
    for px in bgra.pixels_mut() {
        px.0.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(bgra)])))
}

impl ComputerShare {
    pub fn new(backend: Backend, thread_id: String, cx: &mut Context<Self>) -> Self {
        let this = Self {
            backend,
            thread_id,
            viewer: viewer_id(),
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
        };
        Self::start_polling(cx);
        this
    }

    /// The agent is (or just was) driving apps in this conversation.
    pub fn has_session(&self) -> bool {
        self.session.is_some() || self.mine
    }

    pub fn hidden(&self) -> bool {
        self.hidden
    }

    pub fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.hidden = !self.hidden;
        self.sync_cards(cx);
        cx.notify();
    }

    fn poll_delay(&self, cx: &App) -> Duration {
        if !self.enabled {
            POLL_OFF
        } else if cx.active_window().is_none() {
            POLL_BACKGROUND
        } else if self.session.as_ref().is_some_and(|s| !s.ended) || self.mine {
            POLL_ACTIVE
        } else {
            POLL_IDLE
        }
    }

    fn start_polling(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok((backend, viewer)) =
                    this.update(cx, |this, _| (this.backend.clone(), this.viewer.clone()))
                else {
                    break;
                };
                let job = backend.spawn(move |c| async move { c.computer_state(&viewer).await });
                let result = crate::jobs::flatten(job.await);
                let Ok(delay) = this.update(cx, |this, cx| {
                    match result {
                        Ok(state) => this.apply(state, cx),
                        // silent-ok: an older gateway has no live view route,
                        // and a failed poll is retried on the next tick.
                        Err(e) => {
                            tracing::debug!(error = %e, "computer-use state poll failed");
                            if e.is_not_found() {
                                this.enabled = false;
                            }
                        }
                    }
                    this.poll_delay(cx)
                }) else {
                    break;
                };
                cx.background_executor().timer(delay).await;
            }
        })
        .detach();
    }

    fn apply(&mut self, state: ComputerState, cx: &mut Context<Self>) {
        self.enabled = state.enabled;
        self.missing = if state.enabled {
            crate::permissions::missing()
        } else {
            Vec::new()
        };
        if state.now > 0 {
            self.skew_ms = now_ms() - state.now;
        }
        let session = state
            .sessions
            .into_iter()
            .find(|s| s.thread_id == self.thread_id && s.watchable && !s.apps.is_empty());
        // A new session in this conversation reopens a stack hidden for an
        // earlier one.
        if session.is_some() && self.session.is_none() {
            self.hidden = false;
        }
        if self.mine && session.as_ref().is_some_and(|s| !s.human_has_control) {
            self.mine = false; // released elsewhere (Stop, or another viewer)
        }
        self.session = session;
        self.sync_cards(cx);
        cx.notify();
    }

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

    /// Cards follow the windows; each holds a live, still or no fetch.
    fn sync_cards(&mut self, cx: &mut Context<Self>) {
        let visible = !self.hidden && self.session.is_some();
        let apps = if visible { self.apps() } else { Vec::new() };
        let keep: Vec<u32> = apps.iter().map(|a| a.window_id).collect();
        let gone: Vec<u32> = self
            .cards
            .keys()
            .filter(|w| !keep.contains(w))
            .copied()
            .collect();
        for window in gone {
            if let Some(card) = self.cards.remove(&window)
                && let Some(image) = card.image
            {
                cx.drop_image(image, None);
            }
        }
        if self.big.is_some_and(|w| !keep.contains(&w)) {
            self.big = None;
        }
        let paused = self.paused();
        let session = self.session.clone();
        let skew = self.skew_ms;
        for (ix, app) in apps.iter().enumerate() {
            let big = self.big == Some(app.window_id);
            let (mode, width, fps) = match (paused, big, ix) {
                (true, _, _) => (Mode::Off, 0, 0.),
                (_, true, _) => (Mode::Live, 1280, 10.),
                (_, _, 0) => (Mode::Live, 720, 8.),
                _ => (Mode::Still, 400, 0.3),
            };
            let card = self.cards.entry(app.window_id).or_insert_with(Card::new);
            card.mode = mode;
            card.width = width;
            card.fps = fps;
            if let Some(session) = &session {
                follow_pointer(card, session, app.window_id, skew);
            }
            if mode != Mode::Off && !card.fetching {
                card.fetching = true;
                self.fetch(app.window_id, cx);
            }
        }
    }

    fn fetch(&self, window: u32, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let thread_id = self.thread_id.clone();
        cx.spawn(async move |this, cx| {
            let mut backoff = Duration::from_secs(1);
            loop {
                let plan = this.update(cx, |this, cx| {
                    let card = this.cards.get_mut(&window)?;
                    // One loop per card: it follows the card's current mode
                    // and size, and ends when the card stops fetching.
                    if card.mode == Mode::Off {
                        card.fetching = false;
                        return None;
                    }
                    let active = cx.active_window().is_some();
                    Some((card.mode, card.width, card.fps, card.seq, active))
                });
                let Ok(Some((mode, width, fps, seq, active))) = plan else {
                    break;
                };
                if !active {
                    // Nobody is watching: no capture until the window is back.
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    continue;
                }
                let thread = thread_id.clone();
                let fps = if mode == Mode::Still { 0.3 } else { fps };
                let after = if mode == Mode::Live { seq } else { None };
                let job = backend.spawn(move |c| async move {
                    c.computer_frame(&thread, window, width, fps, after).await
                });
                let answer = crate::jobs::flatten(job.await);
                let pause = match answer {
                    Ok(FrameAnswer::Frame(frame)) => {
                        backoff = Duration::from_secs(1);
                        let jpeg = frame.jpeg;
                        let image = cx
                            .background_executor()
                            .spawn(async move { decode(&jpeg) })
                            .await;
                        let _ = this.update(cx, |this, cx| {
                            if let Some(card) = this.cards.get_mut(&window)
                                && let Some(image) = image
                            {
                                card.natural = (frame.width, frame.height);
                                card.seq = Some(frame.seq);
                                card.refusal = None;
                                if let Some(old) = card.image.replace(image) {
                                    cx.drop_image(old, None);
                                }
                                cx.notify();
                            }
                        });
                        (mode == Mode::Still).then_some(STILL_EVERY)
                    }
                    Ok(FrameAnswer::Unchanged) => (mode == Mode::Still).then_some(STILL_EVERY),
                    Ok(FrameAnswer::Refused { code, detail }) => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(card) = this.cards.get_mut(&window) {
                                card.refusal = Some(match (code.as_str(), detail) {
                                    ("human_has_control", _) => {
                                        "Paused while you have control".into()
                                    }
                                    (_, Some(detail)) => format!("No picture: {detail}"),
                                    ("ended", None) => "The session ended".into(),
                                    _ => "No picture yet".into(),
                                });
                                cx.notify();
                            }
                        });
                        let wait = backoff;
                        backoff = (backoff * 2).min(Duration::from_secs(10));
                        Some(wait)
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "computer-use frame fetch failed");
                        let wait = backoff;
                        backoff = (backoff * 2).min(Duration::from_secs(10));
                        Some(wait)
                    }
                };
                if let Some(pause) = pause {
                    cx.background_executor().timer(pause).await;
                }
            }
        })
        .detach();
    }

    fn control(&mut self, action: ComputerControl, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.note = None;
        let thread_id = self.thread_id.clone();
        let viewer = self.viewer.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.computer_control(&thread_id, action, &viewer).await },
            move |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        match action {
                            ComputerControl::TakeOver => this.mine = true,
                            ComputerControl::HandBack | ComputerControl::Stop => this.mine = false,
                        }
                        this.sync_cards(cx);
                    }
                    Err(e) => {
                        this.note = Some(
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
                cx.notify();
            },
        );
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.control(ComputerControl::Stop, cx);
        // The Hermes viewer also interrupts the whole turn.
        cx.emit(StopRun);
    }

    fn take_over_or_back(&mut self, cx: &mut Context<Self>) {
        let action = if self.mine {
            ComputerControl::HandBack
        } else {
            ComputerControl::TakeOver
        };
        self.control(action, cx);
    }

    fn click_card(&mut self, window: u32, front: bool, cx: &mut Context<Self>) {
        if front {
            self.big = Some(window);
        } else if let Some(first) = self.apps().first() {
            self.pinned = Some((window, first.window_id));
        }
        self.sync_cards(cx);
        cx.notify();
    }

    fn close_big(&mut self, cx: &mut Context<Self>) {
        self.big = None;
        self.sync_cards(cx);
        cx.notify();
    }
}

impl Drop for ComputerShare {
    /// A take over must not outlive its viewer: hand the desktop back.
    fn drop(&mut self) {
        if self.mine {
            let thread_id = self.thread_id.clone();
            let viewer = self.viewer.clone();
            let _detached = self.backend.spawn(move |c| async move {
                c.computer_control(&thread_id, ComputerControl::HandBack, &viewer)
                    .await
            });
        }
    }
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
    card.glide = Some(Glide {
        from: if first || !fresh { to } else { from },
        to,
        started: Instant::now(),
    });
    if first || !fresh {
        return;
    }
    if p.kind.contains("click") {
        let right = p.kind == "right_click";
        card.ripples.push(Ripple {
            at: to,
            starts: Instant::now() + GLIDE,
            right,
        });
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
        card.glide = Some(Glide {
            from: to,
            to: end,
            started: Instant::now() + GLIDE,
        });
    }
}

struct Status {
    dot: u32,
    pulse: bool,
    text: String,
}

fn status_of(share: &ComputerShare, s: &ComputerSession) -> Status {
    let st = |dot, pulse, text: String| Status { dot, pulse, text };
    if share.paused() {
        return st(AMBER, false, "You have control. The agent is paused".into());
    }
    if s.stopping {
        return st(AMBER, false, "Stopping…".into());
    }
    if s.ended {
        return match &s.last_failure {
            Some(f) => st(
                0xef4444,
                false,
                format!("Ended · last problem: {}", f.label),
            ),
            None => st(0x777777, false, "Done".into()),
        };
    }
    if s.inflight > 0 {
        let text = if s.label.is_empty() {
            "Working…".into()
        } else {
            s.label.clone()
        };
        return st(0x22c55e, true, text);
    }
    if s.ok == Some(false) {
        let why = match s.status.as_deref() {
            Some("refused") => "Refused",
            Some("unknown") => "Outcome unknown",
            _ => "Failed",
        };
        return st(0xef4444, false, format!("{why}: {}", s.label));
    }
    let last = if s.label.is_empty() {
        "—"
    } else {
        s.label.as_str()
    };
    st(
        INDIGO_SOFT,
        true,
        format!("Waiting for the agent · last: {last}"),
    )
}

fn age_text(app: &ComputerApp) -> Option<String> {
    let age = app.capture.frame_age?.round() as i64;
    let text = match age {
        a if a <= 2 => "live".to_string(),
        a if a < 60 => format!("{a}s old"),
        a => format!("{}m old", a / 60),
    };
    Some(if app.capture.method.as_deref() == Some("snapshot") {
        format!("{text} · agent view")
    } else {
        text
    })
}

/// The picture's rectangle inside a stage of `w`×`h` (contain, anchored
/// top-centre), as (x, y, w, h).
fn content_rect(natural: (u32, u32), w: f32, h: f32) -> (f32, f32, f32, f32) {
    let (nw, nh) = (natural.0 as f32, natural.1 as f32);
    if nw <= 0. || nh <= 0. {
        return (0., 0., w, h);
    }
    let k = (w / nw).min(h / nh);
    let (cw, ch) = (nw * k, nh * k);
    ((w - cw) / 2., 0., cw, ch)
}

/// The cursor, ripples and drag trail over one card's picture.
fn overlay(card: &Card, rect: (f32, f32, f32, f32), scale: f32) -> impl IntoElement {
    let cursor = card.glide.map(|g| g.at().0);
    let now = Instant::now();
    let ripples: Vec<((f32, f32), f32, bool)> = card
        .ripples
        .iter()
        .filter(|r| now >= r.starts)
        .map(|r| {
            (
                r.at,
                (now - r.starts).as_secs_f32() / RIPPLE.as_secs_f32(),
                r.right,
            )
        })
        .collect();
    let trail = card.trail.map(|(a, b, _)| (a, b));
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, _| {
            let ox = f32::from(bounds.left()) + rect.0;
            let oy = f32::from(bounds.top()) + rect.1;
            let at = |f: (f32, f32)| point(px(ox + f.0 * rect.2), px(oy + f.1 * rect.3));
            if let Some((a, b)) = trail {
                let mut path = PathBuilder::stroke(px(2.)).dash_array(&[px(4.), px(3.)]);
                path.move_to(at(a));
                path.line_to(at(b));
                if let Ok(path) = path.build() {
                    window.paint_path(path, rgb(INDIGO_SOFT));
                }
            }
            for (center, t, right) in &ripples {
                let r = 15. * (0.3 + 1.4 * t.min(1.)) * scale;
                let alpha = (1. - t).clamp(0., 1.);
                let c = at(*center);
                let mut path = PathBuilder::stroke(px(2.));
                let steps = 24;
                for i in 0..=steps {
                    let a = i as f32 / steps as f32 * std::f32::consts::TAU;
                    let p = point(c.x + px(r * a.cos()), c.y + px(r * a.sin()));
                    if i == 0 {
                        path.move_to(p)
                    } else {
                        path.line_to(p)
                    }
                }
                if let Ok(path) = path.build() {
                    let color = if *right {
                        rgba((AMBER << 8) | 0xff)
                    } else {
                        rgba((INDIGO_SOFT << 8) | 0xff)
                    };
                    let mut color: gpui_kit::Hsla = color.into();
                    color.a = alpha;
                    window.paint_path(path, color);
                }
            }
            if let Some(tip) = cursor {
                let t = at(tip);
                let k = 0.9 * scale.max(0.6);
                let pt = |x: f32, y: f32| point(t.x + px((x - 4.) * k), t.y + px((y - 2.5) * k));
                let shape = [(4., 2.5), (19.5, 11.5), (12.7, 13.), (9.1, 19.5)];
                let mut outline = PathBuilder::stroke(px(1.7));
                let mut fill = PathBuilder::fill();
                for (i, (x, y)) in shape.iter().enumerate() {
                    if i == 0 {
                        outline.move_to(pt(*x, *y));
                        fill.move_to(pt(*x, *y));
                    } else {
                        outline.line_to(pt(*x, *y));
                        fill.line_to(pt(*x, *y));
                    }
                }
                outline.close();
                fill.close();
                if let Ok(path) = fill.build() {
                    window.paint_path(path, rgb(INDIGO));
                }
                if let Ok(path) = outline.build() {
                    window.paint_path(path, rgb(0xffffff));
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[derive(Clone, Copy)]
struct CardGeometry {
    w: f32,
    h: f32,
    scale: f32,
}

impl ComputerShare {
    /// One card. `big` is the enlarged view: clicking it closes it.
    fn render_card(
        &self,
        app: &ComputerApp,
        session: &ComputerSession,
        geometry: CardGeometry,
        front: bool,
        big: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let CardGeometry { w, h, scale } = geometry;
        let id = if big {
            SharedString::from("cu-big")
        } else {
            SharedString::from(format!("cu-card-{}", app.window_id))
        };
        let card = self.cards.get(&app.window_id);
        let natural = card.map_or((0, 0), |c| c.natural);
        let rect = content_rect(natural, w, h);
        let image = card.and_then(|c| c.image.clone());
        let note = if self.paused() {
            Some("Paused while you have control".to_string())
        } else if image.is_none() {
            card.and_then(|c| c.refusal.clone())
                .or_else(|| {
                    app.capture
                        .error
                        .clone()
                        .map(|e| format!("No picture: {e}"))
                })
                .or_else(|| Some("Connecting…".into()))
        } else {
            None
        };
        let typing = session.typing.as_ref().filter(|t| {
            t.window_id == Some(app.window_id) && now_ms() - self.skew_ms - t.at < TYPING_MS
        });
        let cursor = card.and_then(|c| c.glide).map(|g| g.at().0);
        let window = app.window_id;
        let tag = if app.app.is_empty() {
            app.title.clone()
        } else {
            app.app.clone()
        };
        let old = app.capture.frame_age.is_some_and(|a| a > 4.) && session.inflight > 0;
        let running = front && session.inflight > 0;
        div()
            .id(id)
            .relative()
            .w(px(w))
            .h(px(h))
            .overflow_hidden()
            .rounded(px(10. * scale.max(0.7)))
            .bg(rgb(0x0d0d0d))
            .border_1()
            .border_color(if running {
                rgba(0x6366f1bf)
            } else {
                rgba(0xffffff24)
            })
            .shadow_lg()
            .cursor_pointer()
            .when(!front, |d| d.opacity(0.7).hover(|d| d.opacity(0.95)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if big {
                    this.close_big(cx);
                } else {
                    this.click_card(window, front, cx);
                }
            }))
            .when_some(image, |d, image| {
                d.child(
                    img(image)
                        .absolute()
                        .left(px(rect.0))
                        .top(px(rect.1))
                        .w(px(rect.2))
                        .h(px(rect.3)),
                )
            })
            .when_some(card, |d, card| d.child(overlay(card, rect, scale)))
            .when_some(note, |d, note| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_3()
                        .bg(rgba(0x0000008c))
                        .text_color(rgb(0xaaaaaa))
                        .text_xs()
                        .text_center()
                        .child(note),
                )
            })
            .child(
                div()
                    .absolute()
                    .left(px(6.))
                    .top(px(6.))
                    .px(px(7.))
                    .rounded_full()
                    .bg(if front {
                        rgba(0x6366f1eb)
                    } else {
                        rgba(0x3c3c3ce6)
                    })
                    .text_color(rgb(0xffffff))
                    .text_size(px(10.5))
                    .font_semibold()
                    .max_w(px(w * 0.7))
                    .truncate()
                    .child(tag),
            )
            .when_some(age_text(app).filter(|_| front), |d, age| {
                d.child(
                    div()
                        .absolute()
                        .right(px(6.))
                        .top(px(6.))
                        .px(px(6.))
                        .rounded_full()
                        .bg(rgba(0x00000099))
                        .text_color(if old { rgb(AMBER) } else { rgb(0xcccccc) })
                        .text_size(px(10.))
                        .child(age),
                )
            })
            .when_some(typing, |d, t| {
                // On the pointer when there is one, else along the bottom.
                let (x, y) = match cursor {
                    Some(c) => (rect.0 + c.0 * rect.2 + 14., rect.1 + c.1 * rect.3 + 20.),
                    None => (rect.0 + 8., rect.1 + rect.3 - 28.),
                };
                d.child(
                    div()
                        .absolute()
                        .left(px(x.min(w - 60.)))
                        .top(px(y.min(h - 20.)))
                        .px(px(7.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(rgba(0x000000cc))
                        .text_color(rgb(0xffffff))
                        .text_size(px(10.5))
                        .max_w(px(240.))
                        .truncate()
                        .child(format!("⌨ {}", t.text)),
                )
            })
    }
}

impl ComputerShare {
    /// Computer use cannot act until Numoh has both permissions; say so
    /// before the agent tries, with one button that asks macOS.
    fn render_permissions(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let names: Vec<&str> = self.missing.iter().map(|m| m.label()).collect();
        let screen = self
            .missing
            .contains(&crate::permissions::Permission::ScreenRecording);
        v_flex()
            .absolute()
            .top_3()
            .right_3()
            .w(px(CARD_W + 40.))
            .gap_2()
            .p_3()
            .rounded(px(8.))
            .bg(p.surface_overlay)
            .border_1()
            .border_color(p.border)
            .text_xs()
            .text_color(p.text)
            .child(format!(
                "Numoh needs {} to use apps for you.",
                names.join(" and ")
            ))
            .child(div().text_color(p.text_faint).child(if screen {
                "Click Allow, switch Numoh on in System Settings, then reopen Numoh."
            } else {
                "Click Allow, then switch Numoh on in System Settings."
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("cu-allow")
                            .label("Allow")
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, _| {
                                crate::permissions::request(&this.missing)
                            })),
                    )
                    .child(
                        Button::new("cu-later")
                            .label("Later")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.permissions_hidden = true;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ComputerShare {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().absolute().top_0().left_0().size_full();
        if !self.missing.is_empty() && !self.permissions_hidden {
            return root.child(self.render_permissions(cx)).into_any_element();
        }
        let Some(session) = self.session.clone().filter(|_| !self.hidden) else {
            return root.into_any_element();
        };
        // Expire finished animations; keep drawing while any runs.
        let now = Instant::now();
        let mut animating = false;
        for card in self.cards.values_mut() {
            card.ripples.retain(|r| now < r.starts + RIPPLE);
            if card.trail.is_some_and(|(_, _, at)| at.elapsed() > TRAIL) {
                card.trail = None;
            }
            animating |= card.animating();
        }
        if animating {
            window.request_animation_frame();
        }
        let p = *Palette::of(cx);
        let status = status_of(self, &session);
        let apps = self.apps();
        let mut deck = div().relative().w(px(DECK_W)).h(px(STAGE_H + 30.));
        for (ix, app) in apps.iter().enumerate().rev() {
            let scale = 0.82f32.powi(ix as i32);
            let (w, h) = (CARD_W * scale, STAGE_H * scale);
            let right = ix as f32 * 110.;
            let top = STAGE_H / 2. + ix as f32 * 10. - h / 2.;
            let geometry = CardGeometry { w, h, scale };
            deck = deck.child(
                div()
                    .absolute()
                    .right(px(right))
                    .top(px(top))
                    .child(self.render_card(app, &session, geometry, ix == 0, false, cx)),
            );
        }
        let dot = div()
            .flex_none()
            .size(px(7.))
            .rounded_full()
            .bg(rgb(status.dot))
            .when(status.pulse, |d| {
                let t = (now_ms() % 1100) as f32 / 1100.;
                d.opacity(0.3 + 0.7 * (1. - (t * std::f32::consts::TAU).cos()) / 2.)
            });
        if status.pulse {
            crate::design::pulse::request(crate::design::pulse::Rate::Smooth, window, cx);
        }
        let held_elsewhere = session.human_has_control && !self.mine;
        let bar = h_flex()
            .ml_auto()
            .w(px(CARD_W))
            .mt_2()
            .gap_1p5()
            .items_center()
            .pl(px(10.))
            .pr(px(6.))
            .py(px(5.))
            .rounded(px(8.))
            .bg(p.surface_overlay)
            .border_1()
            .border_color(p.border)
            .text_xs()
            .text_color(p.text)
            .child(dot)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.note.clone().unwrap_or_else(|| status.text.into())),
            )
            .when(!session.ended, |b| {
                b.child(
                    Button::new("cu-stop")
                        .label("Stop")
                        .icon(IconName::Square)
                        .ghost()
                        .xsmall()
                        .tooltip("Stop the agent now (also stops this conversation's turn)")
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            })
            .when(!held_elsewhere, |b| {
                b.child(
                    Button::new("cu-take")
                        .label(if self.mine { "Hand back" } else { "Take over" })
                        .ghost()
                        .xsmall()
                        .when(self.mine, |b| b.selected(true))
                        .tooltip(if self.mine {
                            "Give the computer back to the agent"
                        } else {
                            "Pause the agent and use the computer yourself until you hand it back"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.take_over_or_back(cx))),
                )
            })
            .child(
                Button::new("cu-hide")
                    .icon(IconName::Close)
                    .ghost()
                    .xsmall()
                    .tooltip("Hide the screen share (stops capturing)")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
            );
        let stack = v_flex()
            .absolute()
            .top(px(52.))
            .right(px(16.))
            .w(px(DECK_W))
            .child(deck)
            .child(bar);
        let big = self
            .big
            .and_then(|w| apps.iter().find(|a| a.window_id == w).cloned());
        let root = root.child(stack);
        match big {
            Some(app) => {
                let viewport = window.viewport_size();
                let w = (f32::from(viewport.width) * 0.8).min(1100.);
                let h = (f32::from(viewport.height) * 0.75).min(760.);
                let geometry = CardGeometry {
                    w,
                    h,
                    scale: w / CARD_W,
                };
                let card = self.render_card(&app, &session, geometry, true, true, cx);
                root.child(
                    div()
                        .id("cu-big-veil")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgba(0x00000099))
                        .occlude()
                        .on_click(cx.listener(|this, _, _, cx| this.close_big(cx)))
                        .child(card),
                )
                .into_any_element()
            }
            None => root.into_any_element(),
        }
    }
}
