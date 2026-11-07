//! Documents and web pages inside the window, drawn by macOS itself (GPUI
//! `native_view.rs`): QuickLook (`QLPreviewView`) for PDF, Word, slides,
//! sheets and media, and WebKit (`WKWebView`) for HTML and live pages such
//! as `localhost:3000`. Zeron's browser (MIT) showed the shape; this is the
//! smallest version.
//!
//! Kept light on purpose:
//! - Neither framework is linked. Both are `dlopen`ed the first time a
//!   preview opens, so a session that never opens one never maps them.
//! - At most two native views for the whole app: one for the Browser tab,
//!   one for file previews ([`Slot`]), so a preview never replaces the page.
//! - A view lives only while the side pane wants it (`side::native_wanted`).
//!   Otherwise it is hidden, and destroyed after [`Slot::idle`] hidden,
//!   which also ends WebKit's helper processes. The Browser's page address
//!   survives that, so it comes back where it was.
//! - Web pages use a non-persistent data store: no cookies or cache on disk.
//!
//! # How it sits in a Rune window
//!
//! - [`on_window`] (wired with `App::on_window` in `main.rs`) keeps the
//!   window's content `NSView` from its raw-window-handle; native views are
//!   its siblings, ordered above it, as in the GPUI app.
//! - [`surface`] is the element a tab puts where the view goes: an empty box
//!   whose drawn rect (`El::on_drawn_layout`) places the view.
//! - [`poll`] (called from the app's `poll`) reconciles what the pane wants
//!   with what is on screen, before each frame and every 100 ms while a
//!   native view exists (Rune polls before handlers run, so a tab switch
//!   lands on the next poll at the latest).
//!
//! The native view sits above Rune's own view, so Rune popovers can't draw
//! over it. The window's root view calls [`cover`] on every run with
//! whether its overlay layer holds anything (the ⌘K menu, a menu or
//! popover, a dialog), and the views hide meanwhile (GPUI
//! `frame_begin(window, command.is_some())`). Rune keeps keyboard focus
//! except while the page is clicked.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2::encode::{Encode, Encoding};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use raw_window_handle::RawWindowHandle;
use rune::{El, Tx, el};

use crate::state::State;

/// How often the reconcile runs while a native view exists.
const TICK: Duration = Duration::from_millis(100);

/// Which of the two native views an element shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Browser,
    Preview,
}

impl Slot {
    const ALL: [Slot; 2] = [Slot::Browser, Slot::Preview];

    /// How long a hidden view is kept, so switching tabs doesn't reload it.
    fn idle(self) -> Duration {
        match self {
            Slot::Browser => Duration::from_secs(120),
            Slot::Preview => Duration::from_secs(45),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Slot::Browser => "native-browser",
            Slot::Preview => "native-preview",
        }
    }
}

/// What a native view shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Url(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Look,
    Web,
}

impl Source {
    fn engine(&self) -> Engine {
        match self {
            Source::Url(_) => Engine::Web,
            Source::File(path) if is_web_file(path) => Engine::Web,
            Source::File(_) => Engine::Look,
        }
    }
}

/// The native views' share of the app state (`State::native`).
#[derive(Default)]
pub struct NativeViews {
    /// Where each slot's [`surface`] element was last drawn, window points.
    rects: [Option<[f32; 4]>; 2],
    /// An overlay (command menu, popover, dialog) covers the window: views
    /// hide. Written by the root view as it assembles its overlay layer
    /// ([`cover`]), so it is a `Cell`: the view only reads the state, and
    /// the views hide in that same view run.
    covered: Cell<bool>,
}

impl NativeViews {
    /// Whether an overlay covers the window, as the last view run drew it.
    pub fn covered(&self) -> bool {
        self.covered.get()
    }
}

/// Mark the window covered by an overlay or not. The root view
/// (`workspace::shell::view`) calls it on every run with whether its overlay
/// layer holds anything; a change hides or shows the views at once.
pub fn cover(s: &State, covered: bool) {
    if s.native.covered.replace(covered) != covered {
        reconcile(s);
    }
}

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn is_web_file(path: &Path) -> bool {
    matches!(ext(path).as_str(), "html" | "htm" | "xhtml")
}

/// Files the pane shows natively instead of as text or an image.
pub fn shows_natively(path: &Path) -> bool {
    is_web_file(path)
        || matches!(
            ext(path).as_str(),
            "pdf"
                | "doc"
                | "docx"
                | "rtf"
                | "rtfd"
                | "odt"
                | "pages"
                | "ppt"
                | "pptx"
                | "key"
                | "odp"
                | "xls"
                | "xlsx"
                | "numbers"
                | "ods"
                | "epub"
                | "mov"
                | "mp4"
                | "m4v"
                | "mp3"
                | "m4a"
                | "wav"
                | "aiff"
        )
}

/// The box a native view fills: put it where the view goes, sized by its
/// parent. Headless runs draw it as an empty box.
pub fn surface(slot: Slot) -> El {
    el().key(slot.key())
        .w_full()
        .grow(1.)
        .min_h(0.)
        .on_drawn_layout(move |s: &mut State, rect| {
            s.native.rects[slot as usize] = Some(rect);
            reconcile(s);
        })
}

/// Keep the window's content view; called on every native window creation
/// (`App::on_window`). A new window drops views made for the old one.
pub fn on_window(window: &rune::WindowHandle) {
    let view = window.with_raw_handle(|handle| match handle.as_raw() {
        RawWindowHandle::AppKit(h) => unsafe {
            Retained::retain(h.ns_view.as_ptr().cast::<AnyObject>())
        },
        _ => None,
    });
    match view {
        Ok(Some(view)) => {
            for slot in Slot::ALL {
                close_view(slot);
            }
            HOST.with(|h| *h.borrow_mut() = Some(view));
            install_click_monitor();
        }
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, "no native window handle for previews"),
    }
}

/// Show, place and hide the native views as the side pane wants them; call
/// before every frame from the app's `poll`.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    reconcile(s);
    if alive() {
        tx.wake_after(TICK);
    }
    false
}

fn reconcile(s: &State) {
    let wanted = if s.native.covered() || s.fixture {
        [None, None]
    } else {
        crate::chat::side::native_wanted(s)
    };
    for slot in Slot::ALL {
        let ix = slot as usize;
        match (&wanted[ix], s.native.rects[ix]) {
            (Some(source), Some(rect)) if rect[2] > 1. && rect[3] > 1. => show(slot, source, rect),
            _ => hide(slot),
        }
    }
}

/// Load `source` into `slot` now, even if it is the address already asked
/// for (the person pressed Enter on it again).
pub fn load(slot: Slot, source: &Source) {
    with(slot, |s| {
        if let Some(s) = s.filter(|s| s.engine == source.engine()) {
            s.load(source);
        }
    });
}

/// Destroy `slot` now and forget its page.
pub fn close(slot: Slot) {
    if slot == Slot::Browser {
        RESUME.with(|r| r.borrow_mut().take());
    }
    close_view(slot);
}

fn close_view(slot: Slot) {
    if let Some(old) = SURFACES.with(|c| c.borrow_mut()[slot as usize].take()) {
        old.destroy();
    }
}

fn with<R>(slot: Slot, f: impl FnOnce(Option<&mut Surface>) -> R) -> R {
    SURFACES.with(|c| f(c.borrow_mut()[slot as usize].as_mut()))
}

/// A native view exists, shown or hidden awaiting its idle end.
pub fn alive() -> bool {
    SURFACES.with(|cell| cell.borrow().iter().any(Option::is_some))
}

/// GPUI took the keyboard back on any mouse-down in the window
/// (`capture_any_mouse_down` → `reclaim_focus`). Rune has no window-wide
/// press hook, so an AppKit local event monitor does it: a left or right
/// click that lands on Rune's view, not on a preview, hands the keyboard
/// back. Installed once for the app's life; every event passes unchanged.
fn install_click_monitor() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let Some(cls) = AnyClass::get(c"NSEvent") else {
        return;
    };
    let block = block2::RcBlock::new(|event: *mut AnyObject| -> *mut AnyObject {
        if !event.is_null() && alive() && click_on_rune(event) {
            reclaim_focus();
        }
        event
    });
    // NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown.
    const MASK: u64 = (1 << 1) | (1 << 3);
    // SAFETY: the block matches `NSEvent *(^)(NSEvent *)`; AppKit copies it
    // and keeps the monitor until it is removed (never, here).
    unsafe {
        let _monitor: *mut AnyObject =
            msg_send![cls, addLocalMonitorForEventsMatchingMask: MASK, handler: &*block];
    }
}

/// Whether a mouse-down `event` hit Rune's own view in its window (not a
/// QuickLook or WebKit preview lying above it).
fn click_on_rune(event: *mut AnyObject) -> bool {
    let Some(view) = HOST.with(|h| h.borrow().clone()) else {
        return false;
    };
    // SAFETY: plain AppKit queries on live objects, on the main thread (local
    // monitors run there); every returned pointer is null-checked.
    unsafe {
        let win: *mut AnyObject = msg_send![&*view, window];
        let event_win: *mut AnyObject = msg_send![event, window];
        if win.is_null() || !std::ptr::eq(win, event_win) {
            return false;
        }
        let parent: *mut AnyObject = msg_send![&*view, superview];
        if parent.is_null() {
            return false;
        }
        let at: CGPoint = msg_send![event, locationInWindow];
        let at: CGPoint =
            msg_send![parent, convertPoint: at, fromView: std::ptr::null::<AnyObject>()];
        let hit: *mut AnyObject = msg_send![parent, hitTest: at];
        if hit.is_null() {
            return false;
        }
        let inside: bool = msg_send![hit, isDescendantOf: &*view];
        inside
    }
}

/// A click on Rune's view takes the keyboard back from a page clicked
/// earlier (from the click monitor above).
fn reclaim_focus() {
    if !alive() {
        return;
    }
    let Some(view) = HOST.with(|h| h.borrow().clone()) else {
        return;
    };
    unsafe {
        let win: *mut AnyObject = msg_send![&*view, window];
        if win.is_null() {
            return;
        }
        let first: *mut AnyObject = msg_send![win, firstResponder];
        if !std::ptr::eq(first, Retained::as_ptr(&view)) {
            let _: bool = msg_send![win, makeFirstResponder: &*view];
        }
    }
}

/// Reload `source` if it is showing (its file was rewritten).
pub fn refresh(source: &Source) {
    SURFACES.with(|cell| {
        for s in cell
            .borrow()
            .iter()
            .flatten()
            .filter(|s| s.source == *source)
        {
            unsafe {
                match s.engine {
                    Engine::Look => {
                        let _: () = msg_send![&*s.view, refreshPreviewItem];
                    }
                    Engine::Web => {
                        let _: *mut AnyObject = msg_send![&*s.view, reload];
                    }
                }
            }
        }
    });
}

/// The visible page, for the Browser tab's toolbar.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebState {
    pub title: String,
    pub url: String,
    pub loading: bool,
    pub back: bool,
    pub forward: bool,
}

/// The Browser's page.
pub fn web_state() -> Option<WebState> {
    with(Slot::Browser, |s| {
        let s = s.filter(|s| s.engine == Engine::Web)?;
        let v = &*s.view;
        unsafe {
            let title: *mut AnyObject = msg_send![v, title];
            Some(WebState {
                title: string(title),
                url: current_url(v),
                loading: msg_send![v, isLoading],
                back: msg_send![v, canGoBack],
                forward: msg_send![v, canGoForward],
            })
        }
    })
}

unsafe fn current_url(view: &AnyObject) -> String {
    unsafe {
        let url: *mut AnyObject = msg_send![view, URL];
        if url.is_null() {
            return String::new();
        }
        let abs: *mut AnyObject = msg_send![url, absoluteString];
        string(abs)
    }
}

#[derive(Clone, Copy)]
pub enum WebCommand {
    Back,
    Forward,
    Reload,
    Stop,
}

/// Drive the Browser's page.
pub fn web(command: WebCommand) {
    with(Slot::Browser, |s| {
        let Some(s) = s.filter(|s| s.engine == Engine::Web) else {
            return;
        };
        let v = &*s.view;
        unsafe {
            match command {
                WebCommand::Back => {
                    let _: *mut AnyObject = msg_send![v, goBack];
                }
                WebCommand::Forward => {
                    let _: *mut AnyObject = msg_send![v, goForward];
                }
                WebCommand::Reload => {
                    let _: *mut AnyObject = msg_send![v, reload];
                }
                WebCommand::Stop => {
                    let _: () = msg_send![v, stopLoading];
                }
            }
        }
    });
}

thread_local! {
    /// The window's content view (winit's), from [`on_window`].
    static HOST: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
    static SURFACES: RefCell<[Option<Surface>; 2]> = const { RefCell::new([None, None]) };
    /// The Browser's asked-for address and the page it had reached when its
    /// view was destroyed; a new view for the same address opens that page.
    static RESUME: RefCell<Option<(Source, String)>> = const { RefCell::new(None) };
}

struct Surface {
    slot: Slot,
    /// Rune's view; the native view is its sibling, ordered above it.
    rune: Retained<AnyObject>,
    view: Retained<AnyObject>,
    engine: Engine,
    source: Source,
    frame: Option<CGRect>,
    /// Hidden since then, awaiting its idle end.
    hidden: Option<Instant>,
}

impl Surface {
    fn create(slot: Slot, rune: Retained<AnyObject>, source: &Source) -> Option<Self> {
        let engine = source.engine();
        let view = unsafe {
            let host: *mut AnyObject = msg_send![&*rune, superview];
            if host.is_null() {
                return None;
            }
            let view = match engine {
                Engine::Look => new_look_view()?,
                Engine::Web => new_web_view()?,
            };
            // NSWindowAbove = 1.
            let _: () = msg_send![host, addSubview: &*view, positioned: 1isize, relativeTo: &*rune];
            view
        };
        let mut surface = Self {
            slot,
            rune,
            view,
            engine,
            source: source.clone(),
            frame: None,
            hidden: None,
        };
        let resume = RESUME.with(|r| {
            r.borrow()
                .as_ref()
                .filter(|(asked, page)| asked == source && !page.is_empty())
                .map(|(_, page)| page.clone())
        });
        match resume.filter(|_| slot == Slot::Browser) {
            Some(page) => {
                surface.load(&Source::Url(page));
                surface.source = source.clone();
            }
            None => surface.load(source),
        }
        Some(surface)
    }

    fn load(&mut self, source: &Source) {
        self.source = source.clone();
        let v = &*self.view;
        unsafe {
            match source {
                Source::File(path) => {
                    let Some(url) = file_url(path) else { return };
                    match self.engine {
                        Engine::Look => {
                            let _: () = msg_send![v, setPreviewItem: &*url];
                        }
                        Engine::Web => {
                            // Read access stops at the file's own folder.
                            let dir = path
                                .parent()
                                .and_then(|d| file_url(d))
                                .unwrap_or(url.clone());
                            let _: *mut AnyObject =
                                msg_send![v, loadFileURL: &*url, allowingReadAccessToURL: &*dir];
                        }
                    }
                }
                Source::Url(text) => {
                    let Some(cls) = AnyClass::get(c"NSURL") else {
                        return;
                    };
                    let Some(s) = ns_string(text) else { return };
                    let url: Option<Retained<AnyObject>> = msg_send![cls, URLWithString: &*s];
                    let Some(url) = url else { return };
                    let Some(req) = AnyClass::get(c"NSURLRequest") else {
                        return;
                    };
                    let req: Option<Retained<AnyObject>> = msg_send![req, requestWithURL: &*url];
                    if let Some(req) = req {
                        let _: *mut AnyObject = msg_send![v, loadRequest: &*req];
                    }
                }
            }
        }
    }

    /// Match Rune's drawn rect (window points, y down) in the host's
    /// coordinates, whichever way the content view counts y.
    fn place(&mut self, rect: [f32; 4]) {
        unsafe {
            let host: *mut AnyObject = msg_send![&*self.rune, superview];
            if host.is_null() {
                return;
            }
            let flipped: bool = msg_send![&*self.rune, isFlipped];
            let bounds: CGRect = msg_send![&*self.rune, bounds];
            let [x, top, w, h] = rect.map(f64::from);
            let local = CGRect {
                origin: CGPoint {
                    x,
                    y: if flipped {
                        top
                    } else {
                        bounds.size.height - top - h
                    },
                },
                size: CGSize {
                    width: w.max(0.),
                    height: h.max(0.),
                },
            };
            let frame: CGRect = msg_send![&*self.rune, convertRect: local, toView: host];
            if self.frame != Some(frame) {
                let _: () = msg_send![&*self.view, setFrame: frame];
                self.frame = Some(frame);
            }
        }
    }

    fn set_hidden(&mut self, hidden: bool) {
        if self.hidden.is_some() != hidden {
            self.hidden = hidden.then(Instant::now);
            unsafe {
                let _: () = msg_send![&*self.view, setHidden: hidden];
            }
        }
    }

    fn destroy(self) {
        unsafe {
            if self.slot == Slot::Browser && self.engine == Engine::Web {
                let page = current_url(&self.view);
                let asked = self.source.clone();
                RESUME.with(|r| *r.borrow_mut() = Some((asked, page)));
            }
            match self.engine {
                Engine::Look => {
                    let _: () = msg_send![&*self.view, close];
                }
                Engine::Web => {
                    let _: () = msg_send![&*self.view, stopLoading];
                }
            }
            let _: () = msg_send![&*self.view, removeFromSuperview];
        }
    }
}

fn show(slot: Slot, source: &Source, rect: [f32; 4]) {
    let Some(rune) = HOST.with(|h| h.borrow().clone()) else {
        return;
    };
    SURFACES.with(|cell| {
        let mut all = cell.borrow_mut();
        let entry = &mut all[slot as usize];
        let reuse = entry.as_ref().is_some_and(|s| {
            Retained::as_ptr(&s.rune) == Retained::as_ptr(&rune) && s.engine == source.engine()
        });
        if !reuse {
            if let Some(old) = entry.take() {
                old.destroy();
            }
            *entry = Surface::create(slot, rune, source);
        }
        let Some(s) = entry.as_mut() else {
            return;
        };
        if s.source != *source {
            s.load(source);
        }
        s.place(rect);
        s.set_hidden(false);
    });
}

/// Hide `slot`, and end it once it has been hidden past its idle time.
fn hide(slot: Slot) {
    let expired = with(slot, |s| match s {
        Some(s) => {
            s.set_hidden(true);
            s.hidden.is_some_and(|t| t.elapsed() >= slot.idle())
        }
        None => false,
    });
    if expired {
        close_view(slot);
    }
}

// ---- framework loading and object construction ----

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

/// Load a system framework once; its classes register with the runtime.
fn framework(path: &CStr) -> bool {
    const RTLD_LAZY: i32 = 0x1;
    unsafe { !dlopen(path.as_ptr(), RTLD_LAZY).is_null() }
}

const ZERO: CGRect = CGRect {
    origin: CGPoint { x: 0., y: 0. },
    size: CGSize {
        width: 0.,
        height: 0.,
    },
};

unsafe fn new_look_view() -> Option<Retained<AnyObject>> {
    if !framework(c"/System/Library/Frameworks/Quartz.framework/Quartz") {
        return None;
    }
    let cls = AnyClass::get(c"QLPreviewView")?;
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        // QLPreviewViewStyleNormal = 0.
        let view: Option<Retained<AnyObject>> =
            msg_send![alloc, initWithFrame: ZERO, style: 0usize];
        let view = view?;
        let _: () = msg_send![&*view, setShouldCloseWithWindow: false];
        Some(view)
    }
}

unsafe fn new_web_view() -> Option<Retained<AnyObject>> {
    if !framework(c"/System/Library/Frameworks/WebKit.framework/WebKit") {
        return None;
    }
    let config_cls = AnyClass::get(c"WKWebViewConfiguration")?;
    let store_cls = AnyClass::get(c"WKWebsiteDataStore")?;
    let cls = AnyClass::get(c"WKWebView")?;
    unsafe {
        let config: Retained<AnyObject> = msg_send![config_cls, new];
        let store: Option<Retained<AnyObject>> = msg_send![store_cls, nonPersistentDataStore];
        if let Some(store) = store {
            let _: () = msg_send![&*config, setWebsiteDataStore: &*store];
        }
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        let view: Option<Retained<AnyObject>> =
            msg_send![alloc, initWithFrame: ZERO, configuration: &*config];
        view
    }
}

unsafe fn ns_string(text: &str) -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"NSString")?;
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        // NSUTF8StringEncoding = 4.
        msg_send![
            alloc,
            initWithBytes: text.as_ptr().cast::<c_void>(),
            length: text.len(),
            encoding: 4usize
        ]
    }
}

unsafe fn file_url(path: &Path) -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"NSURL")?;
    unsafe {
        let s = ns_string(&path.to_string_lossy())?;
        msg_send![cls, fileURLWithPath: &*s]
    }
}

unsafe fn string(ns: *mut AnyObject) -> String {
    if ns.is_null() {
        return String::new();
    }
    unsafe {
        let utf8: *const c_char = msg_send![ns, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

// ---- CoreGraphics geometry, declared here so no binding crate is needed ----

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_and_pages_pick_their_engine() {
        let file = |p: &str| Source::File(PathBuf::from(p));
        assert!(file("deck.pptx").engine() == Engine::Look);
        assert!(file("report.PDF").engine() == Engine::Look);
        assert!(file("site/index.html").engine() == Engine::Web);
        assert!(Source::Url("http://localhost:3000".into()).engine() == Engine::Web);
        assert!(shows_natively(Path::new("memo.docx")));
        assert!(!shows_natively(Path::new("main.rs")));
    }
}
