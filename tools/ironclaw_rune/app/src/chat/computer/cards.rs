//! The cards: one per window the agent used, each with its own picture
//! fetch loop (live, still or off), and the clicks that re-fan the deck or
//! enlarge a card.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Live,
    Still,
    Off,
}

pub(super) struct Card {
    pub(super) pixels: Pixels,
    pub(super) has_image: bool,
    pub(super) natural: (u32, u32),
    pub(super) seq: Option<u64>,
    pub(super) mode: Mode,
    pub(super) width: u32,
    pub(super) fps: f64,
    pub(super) fetching: bool,
    pub(super) backoff: Duration,
    pub(super) refusal: Option<String>,
    pub(super) glide: Option<Glide>,
    pub(super) pointer_seq: u64,
    pub(super) ripples: Vec<Ripple>,
    pub(super) trail: Option<(Frac, Frac, Instant)>,
}

impl Card {
    pub(super) fn new() -> Self {
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
}

/// JPEG to straight RGBA.
pub(super) fn decode(jpeg: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let rgba = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()? // silent-ok: a broken frame is skipped; the next one replaces it
        .to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Cards follow the windows; each holds a live, still or no fetch.
pub(super) fn sync_cards(s: &mut State, id: &str) {
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
pub(super) fn fetch(s: &mut State, thread_id: String, generation: Generation, window: u32) {
    let active = crate::workspace::window_active(s);
    let Some(cu) = same(s, &thread_id, generation) else {
        return;
    };
    let Some(card) = cu.cards.get_mut(&window) else {
        return;
    };
    if card.mode == Mode::Off {
        card.fetching = false;
        return;
    }
    if !active {
        // Nobody is watching: no capture until the window is back.
        jobs::after(s, Duration::from_secs(1), move |s, _| {
            fetch(s, thread_id, generation, window)
        });
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
                    tokio::task::spawn_blocking(move || decode(&jpeg))
                        .await
                        .ok()
                        .flatten()
                }
                _ => None,
            };
            Ok((answer, decoded))
        },
        move |s, result, _| {
            let Some(cu) = same(s, &thread_id, generation) else {
                return;
            };
            let Some(card) = cu.cards.get_mut(&window) else {
                return;
            };
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
                Some(pause) => jobs::after(s, pause, move |s, _| {
                    fetch(s, thread_id, generation, window)
                }),
                None => fetch(s, thread_id, generation, window),
            }
        },
    );
}

pub(super) fn click_card(s: &mut State, id: &str, window: u32, front: bool) {
    let Some(cu) = slot(s, id) else { return };
    if front {
        cu.big = Some(window);
    } else if let Some(first) = cu.apps().first() {
        cu.pinned = Some((window, first.window_id));
    }
    sync_cards(s, id);
}

pub(super) fn close_big(s: &mut State, id: &str) {
    if let Some(cu) = slot(s, id) {
        cu.big = None;
    }
    sync_cards(s, id);
}
