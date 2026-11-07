//! Remembers the main window's frame between launches, like Safari or Zed
//! (GPUI `window_state.rs`, same `window.json`, so both apps share it).
//!
//! The first launch fills the screen's usable area (everything but the menu
//! bar and Dock). After that the window reopens where it was left. As in
//! GPUI (`observe_window_bounds` → `record`), every move and resize records
//! the frame (full screen is not a frame to restore); here a record is also
//! written, once the window has rested for [`QUIET`], through the app's one
//! ordered writer (`persist.rs`), and quit writes the last one again.
//!
//! Rune has no window-frame API (it can resize and place, not report), so on
//! macOS this reads and sets the `NSWindow` frame itself through the raw
//! handle Rune hands `App::on_window`, and hears moves and resizes from
//! `NSWindowDidMoveNotification`/`NSWindowDidResizeNotification`. Frames are
//! stored as GPUI did: top-left origin, points, y growing down from the
//! primary screen's top.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long the frame must rest before it is written (a drag or a live
/// resize sends many changes).
const QUIET: Duration = Duration::from_millis(500);

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
    /// The last windowed frame recorded (GPUI's `Last` global).
    static LAST: Cell<Option<Frame>> = const { Cell::new(None) };
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
/// (or over the screen's usable area on first launch), then follow its
/// moves and resizes.
pub fn created(window: &rune::WindowHandle) {
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
    #[cfg(target_os = "macos")]
    if let Err(e) = window.with_raw_handle(|h| {
        mac::restore(h, saved());
        mac::observe(h);
    }) {
        tracing::debug!("cannot place the window: {e}");
    }
}

/// Note `frame` as the window's frame and queue it for disk. `None` (full
/// screen) keeps the last windowed one.
fn record(frame: Option<Frame>) {
    let Some(frame) = frame else { return };
    if LAST.with(|l| l.replace(Some(frame))) == Some(frame) {
        return;
    }
    if let Some(tx) = recorder() {
        let _ = tx.send(frame);
    }
}

/// The debounced writer's input, started on first use.
fn recorder() -> Option<&'static Sender<Frame>> {
    static TX: OnceLock<Option<Sender<Frame>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("zlo-window-frame".into())
            .spawn(move || debounce(rx, QUIET, write));
        match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::warn!("cannot follow the window frame: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Hand `write` each frame from `rx` once no newer one came for `quiet`;
/// the last one is written when the channel closes.
fn debounce(rx: Receiver<Frame>, quiet: Duration, write: impl Fn(Frame)) {
    while let Ok(mut frame) = rx.recv() {
        loop {
            match rx.recv_timeout(quiet) {
                Ok(newer) => frame = newer,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    write(frame);
                    return;
                }
            }
        }
        write(frame);
    }
}

/// Queue `frame` on the app's ordered writer.
fn write(frame: Frame) {
    let Some(file) = file() else { return };
    match serde_json::to_vec(&frame) {
        Ok(json) => crate::persist::global().write(file, json),
        Err(e) => tracing::warn!("could not save the window frame: {e}"),
    }
}

/// Write the window's frame now (quit): the current one, or the last
/// windowed one when it is full screen. `workspace::on_close` flushes the
/// writer right after.
pub fn save() {
    let current = WINDOW.with(|w| {
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
    if let Some(frame) = current {
        LAST.with(|l| l.set(Some(frame)));
    }
    if let Some(frame) = LAST.with(Cell::get) {
        write(frame);
    }
}

/// The primary (menu bar) screen's height in points, when known.
pub fn primary_screen_height() -> Option<f32> {
    #[cfg(target_os = "macos")]
    {
        mac::primary_height()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use block2::RcBlock;
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
        frame_of(ns_window(h)?)
    }

    /// `window`'s frame, or `None` in full screen.
    fn frame_of(window: *mut AnyObject) -> Option<Frame> {
        if window.is_null() {
            return None;
        }
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

    pub(super) fn primary_height() -> Option<f32> {
        Some(screens().first()?.0.size.h as f32)
    }

    /// Record the frame on every move and resize of the window (GPUI
    /// `observe_window_bounds`). The observers live as long as the app.
    pub(super) fn observe(h: raw_window_handle::WindowHandle<'_>) {
        let Some(window) = ns_window(h) else { return };
        let Some(center_class) = AnyClass::get(c"NSNotificationCenter") else {
            return;
        };
        let Some(string) = AnyClass::get(c"NSString") else {
            return;
        };
        unsafe {
            let center: *mut AnyObject = msg_send![center_class, defaultCenter];
            if center.is_null() {
                return;
            }
            for name in [
                c"NSWindowDidMoveNotification",
                c"NSWindowDidResizeNotification",
            ] {
                let name: *mut AnyObject = msg_send![string, stringWithUTF8String: name.as_ptr()];
                if name.is_null() {
                    continue;
                }
                // Posted on the main thread (the window's), with the
                // window as the object.
                let block = RcBlock::new(|note: *mut AnyObject| {
                    if note.is_null() {
                        return;
                    }
                    let window: *mut AnyObject = msg_send![note, object];
                    super::record(frame_of(window));
                });
                let nil: *mut AnyObject = std::ptr::null_mut();
                let observer: *mut AnyObject = msg_send![
                    center,
                    addObserverForName: name,
                    object: window,
                    queue: nil,
                    usingBlock: &*block
                ];
                // The center keeps the observer for as long as the app runs.
                let _ = observer;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn frame(x: f32) -> Frame {
        Frame {
            x,
            y: 0.,
            w: 800.,
            h: 600.,
        }
    }

    #[test]
    fn a_burst_of_frames_writes_only_the_one_it_rested_on() {
        let (tx, rx) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = written.clone();
        let worker = std::thread::spawn(move || {
            debounce(rx, Duration::from_millis(80), move |f| {
                sink.lock().unwrap().push(f)
            })
        });
        // A drag: many frames in quick succession, then a rest.
        for x in 0..20 {
            tx.send(frame(x as f32)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(*written.lock().unwrap(), vec![frame(19.)]);
        // A later move, cut short by quit: written as the channel closes.
        tx.send(frame(42.)).unwrap();
        drop(tx);
        worker.join().unwrap();
        assert_eq!(*written.lock().unwrap(), vec![frame(19.), frame(42.)]);
    }

    #[test]
    fn full_screen_keeps_the_last_windowed_frame() {
        record(Some(frame(5.)));
        record(None);
        assert_eq!(LAST.with(Cell::get), Some(frame(5.)));
    }
}
