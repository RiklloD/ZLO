//! The headless scenes' share: three windows, a pointer and typing.

use super::*;

/// A session with three windows, a pointer mid-glide and typing, for the
/// headless scene. The cards have no picture (no gateway), so they show why.
pub fn fixture(thread_id: &str) -> ComputerState {
    use zlo_client::computer_use::{ComputerCapture, ComputerPointer, ComputerTyping};
    let now = now_ms();
    let app = |window_id: u32, name: &str, title: &str, age: f64| ComputerApp {
        pid: 1,
        window_id,
        app: name.into(),
        title: title.into(),
        width: Some(1440),
        height: Some(900),
        capture: ComputerCapture {
            method: Some("window".into()),
            error: None,
            frame_age: Some(age),
        },
    };
    let session = ComputerSession {
        thread_id: thread_id.into(),
        apps: vec![
            app(11, "Safari", "IronClaw docs", 1.),
            app(12, "Notes", "Port plan", 12.),
            app(13, "Terminal", "zsh", 90.),
        ],
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
        typing: Some(ComputerTyping {
            text: "rune port".into(),
            at: now,
            window_id: Some(11),
        }),
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
            card.glide = Some(Glide {
                from: (0.42, 0.36),
                to: (0.42, 0.36),
                // Already landed: the scene rests.
                started: Instant::now()
                    .checked_sub(GLIDE)
                    .unwrap_or_else(Instant::now),
            });
            // A grey test picture, so the scene shows image placement.
            let (w, h) = (16u32, 10u32);
            card.pixels
                .set(w, h, [40u8, 44, 52, 255].repeat((w * h) as usize));
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
