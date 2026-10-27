//! Remembers the main window's frame between launches, like Safari or Zed.
//!
//! The first launch fills the screen's usable area (everything but the menu
//! bar and Dock). After that the window reopens where it was left. Moves and
//! resizes only update memory; the file is written once, on quit.

use std::path::PathBuf;

use gpui_kit::{App, Bounds, Global, Window, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Frame {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Frame {
    fn bounds(self) -> Bounds<gpui_kit::Pixels> {
        Bounds::new(point(px(self.x), px(self.y)), size(px(self.w), px(self.h)))
    }
}

/// The latest frame seen this session.
struct Last(Frame);

impl Global for Last {}

fn file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Numoh").join("window.json"))
}

/// Where the main window should open.
pub fn initial(cx: &App) -> WindowBounds {
    let saved = file()
        .and_then(|f| std::fs::read(f).ok()) // silent-ok: no file yet means first launch
        .and_then(|b| serde_json::from_slice::<Frame>(&b).ok()) // silent-ok: a bad file falls back to the default
        .map(Frame::bounds)
        // Skip a frame left on a display that is no longer connected.
        .filter(|b| cx.displays().iter().any(|d| d.bounds().intersects(b)));
    let bounds = saved
        .or_else(|| cx.primary_display().map(|d| d.visible_bounds()))
        .unwrap_or_else(|| Bounds::centered(None, size(px(1120.), px(780.)), cx));
    WindowBounds::Windowed(bounds)
}

/// Note the window's current frame. Full screen is not a frame to restore.
pub fn record(window: &Window, cx: &mut App) {
    if window.is_fullscreen() {
        return;
    }
    let b = window.bounds();
    cx.set_global(Last(Frame {
        x: b.origin.x.as_f32(),
        y: b.origin.y.as_f32(),
        w: b.size.width.as_f32(),
        h: b.size.height.as_f32(),
    }));
}

/// Write the last frame to disk. Called on quit.
pub fn save(cx: &App) {
    let (Some(Last(frame)), Some(file)) = (cx.try_global::<Last>(), file()) else {
        return;
    };
    let saved = file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::write(
                &file,
                serde_json::to_vec(frame).map_err(std::io::Error::other)?,
            )
        });
    if let Err(e) = saved {
        tracing::warn!("could not save the window frame: {e}");
    }
}
