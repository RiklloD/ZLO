//! Remembers the main window's frame between launches, like Safari or Zed
//! (GPUI `window_state.rs`, same `window.json`, so both apps share it).
//!
//! The first launch fills the screen's usable area (everything but the menu
//! bar and Dock). After that the window reopens where it was left. The file
//! is written once, on quit.
//!
//! Rune has no window-frame API (it can resize and place, not report), so on
//! macOS this reads and sets the `NSWindow` frame itself through the raw
//! handle Rune hands `App::on_window`. Frames are stored as GPUI did:
//! top-left origin, points, y growing down from the primary screen's top.

use std::cell::RefCell;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Frame {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

thread_local! {
    /// The main window, once Rune created it (UI thread only).
    static WINDOW: RefCell<Option<rune::WindowHandle>> = const { RefCell::new(None) };
}

fn file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ZLO").join("window.json"))
}

fn saved() -> Option<Frame> {
    file()
        .and_then(|f| std::fs::read(f).ok()) // silent-ok: no file yet means first launch
        .and_then(|b| serde_json::from_slice::<Frame>(&b).ok()) // silent-ok: a bad file falls back to the default
}

/// `App::on_window`: the native window exists; put it where it was left
/// (or over the screen's usable area on first launch).
pub fn created(window: &rune::WindowHandle) {
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
    #[cfg(target_os = "macos")]
    if let Err(e) = window.with_raw_handle(|h| mac::restore(h, saved())) {
        tracing::debug!("cannot place the window: {e}");
    }
}

/// Write the window's current frame to disk. Called on quit. Full screen is
/// not a frame to restore.
pub fn save() {
    let frame = WINDOW.with(|w| {
        let window = w.borrow().clone()?;
        #[cfg(target_os = "macos")]
        {
            window.with_raw_handle(mac::current).ok().flatten()
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = window;
            None
        }
    });
    let (Some(frame), Some(file)) = (frame, file()) else {
        return;
    };
    let written = serde_json::to_vec(&frame)
        .map_err(std::io::Error::other)
        .and_then(|json| crate::persist::atomic_write(&file, &json));
    if let Err(e) = written {
        tracing::warn!("could not save the window frame: {e}");
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::encode::{Encode, Encoding};
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use raw_window_handle::RawWindowHandle;

    use super::Frame;

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Size {
        w: f64,
        h: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Rect {
        origin: Point,
        size: Size,
    }
    unsafe impl Encode for Point {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl Encode for Size {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl Encode for Rect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[Point::ENCODING, Size::ENCODING]);
    }

    impl Rect {
        fn intersects(&self, o: &Rect) -> bool {
            self.origin.x < o.origin.x + o.size.w
                && o.origin.x < self.origin.x + self.size.w
                && self.origin.y < o.origin.y + o.size.h
                && o.origin.y < self.origin.y + self.size.h
        }
    }

    fn ns_window(h: raw_window_handle::WindowHandle<'_>) -> Option<*mut AnyObject> {
        let RawWindowHandle::AppKit(appkit) = h.as_raw() else {
            return None;
        };
        let view = appkit.ns_view.as_ptr().cast::<AnyObject>();
        let window: *mut AnyObject = unsafe { msg_send![view, window] };
        (!window.is_null()).then_some(window)
    }

    /// Every screen's frame, the primary (menu bar) one first.
    fn screens() -> Vec<(Rect, Rect)> {
        let Some(class) = AnyClass::get(c"NSScreen") else {
            return Vec::new();
        };
        unsafe {
            let list: *mut AnyObject = msg_send![class, screens];
            if list.is_null() {
                return Vec::new();
            }
            let n: usize = msg_send![list, count];
            (0..n)
                .filter_map(|i| {
                    let screen: *mut AnyObject = msg_send![list, objectAtIndex: i];
                    if screen.is_null() {
                        return None;
                    }
                    let frame: Rect = msg_send![screen, frame];
                    let visible: Rect = msg_send![screen, visibleFrame];
                    Some((frame, visible))
                })
                .collect()
        }
    }

    /// Cocoa's bottom-left frame from GPUI's top-left one.
    fn to_cocoa(f: Frame, primary_h: f64) -> Rect {
        Rect {
            origin: Point {
                x: f.x as f64,
                y: primary_h - f.y as f64 - f.h as f64,
            },
            size: Size {
                w: f.w as f64,
                h: f.h as f64,
            },
        }
    }

    pub(super) fn restore(h: raw_window_handle::WindowHandle<'_>, saved: Option<Frame>) {
        let Some(window) = ns_window(h) else { return };
        let screens = screens();
        let Some((primary, primary_visible)) = screens.first().copied() else {
            return;
        };
        // Skip a frame left on a display that is no longer connected.
        let target = saved
            .map(|f| to_cocoa(f, primary.size.h))
            .filter(|r| screens.iter().any(|(s, _)| s.intersects(r)))
            .unwrap_or(primary_visible);
        unsafe {
            let _: () = msg_send![window, setFrame: target, display: Bool::YES];
        }
    }

    pub(super) fn current(h: raw_window_handle::WindowHandle<'_>) -> Option<Frame> {
        let window = ns_window(h)?;
        // NSWindowStyleMaskFullScreen.
        let style: usize = unsafe { msg_send![window, styleMask] };
        if style & (1 << 14) != 0 {
            return None;
        }
        let primary_h = screens().first()?.0.size.h;
        let r: Rect = unsafe { msg_send![window, frame] };
        Some(Frame {
            x: r.origin.x as f32,
            y: (primary_h - r.origin.y - r.size.h) as f32,
            w: r.size.w as f32,
            h: r.size.h as f32,
        })
    }
}
