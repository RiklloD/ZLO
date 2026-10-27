//! Documents and web pages inside the window, drawn by macOS itself:
//! QuickLook (`QLPreviewView`) for PDF, Word, slides, sheets and media, and
//! WebKit (`WKWebView`) for HTML and live pages such as `localhost:3000`.
//! Zeron's browser (MIT) showed the shape; this is the smallest version.
//!
//! Kept light on purpose:
//! - Neither framework is linked. Both are `dlopen`ed the first time a
//!   preview opens, so a session that never opens one never maps them.
//! - At most two native views for the whole app: one for the Browser tab,
//!   one for file previews ([`Slot`]), so a preview never replaces the page.
//! - A view lives only while an element of [`surface`] paints it. A frame
//!   that doesn't hides it, and it is destroyed after [`Slot::idle`]
//!   hidden, which also ends WebKit's helper processes. The Browser's page
//!   address survives that, so it comes back where it was.
//! - Web pages use a non-persistent data store: no cookies or cache on disk.
//!
//! The native view sits above GPUI's own view, so GPUI popovers can't draw
//! over it. GPUI keeps keyboard focus except while the page is clicked.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::time::Duration;

use objc2::encode::{Encode, Encoding};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::ui::*;

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

/// Fills its parent with the `slot` view showing `source`.
pub fn surface(slot: Slot, source: Source) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| show(window, slot, &source, bounds),
    )
    .size_full()
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
    if let Some(old) = SURFACES.with(|c| c.borrow_mut()[slot as usize].take()) {
        old.destroy();
    }
}

fn with<R>(slot: Slot, f: impl FnOnce(Option<&mut Surface>) -> R) -> R {
    SURFACES.with(|c| f(c.borrow_mut()[slot as usize].as_mut()))
}

/// Call once per frame from the window's root view, before children render.
/// A view that no element paints this frame is hidden right after it.
/// `covered`: GPUI draws an overlay this frame, so the view stays hidden.
pub fn frame_begin(window: &Window, covered: bool) {
    COVERED.with(|c| c.set(covered));
    let mut shown = false;
    SURFACES.with(|cell| {
        for s in cell.borrow_mut().iter_mut().flatten().filter(|s| !s.hidden) {
            s.claimed = false;
            shown = true;
        }
    });
    if shown {
        window.on_next_frame(|_, cx| settle(cx));
    }
}

/// A click on GPUI takes the keyboard back from a page clicked earlier.
pub fn reclaim_focus(window: &Window) {
    if SURFACES.with(|cell| cell.borrow().iter().all(Option::is_none)) {
        return;
    }
    let Some(view) = gpui_view(window) else {
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
    static SURFACES: RefCell<[Option<Surface>; 2]> = const { RefCell::new([None, None]) };
    /// Bumped on every hide and show, so a stale destroy timer does nothing.
    static EPOCH: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
    /// The Browser's asked-for address and the page it had reached when its
    /// view was destroyed; a new view for the same address opens that page.
    static RESUME: RefCell<Option<(Source, String)>> = const { RefCell::new(None) };
    static COVERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct Surface {
    slot: Slot,
    /// GPUI's view; the native view is its sibling, ordered above it.
    gpui: Retained<AnyObject>,
    view: Retained<AnyObject>,
    engine: Engine,
    source: Source,
    frame: Option<CGRect>,
    hidden: bool,
    /// Painted since the last [`frame_begin`].
    claimed: bool,
}

impl Surface {
    fn create(slot: Slot, gpui: Retained<AnyObject>, source: &Source) -> Option<Self> {
        let engine = source.engine();
        let view = unsafe {
            let host: *mut AnyObject = msg_send![&*gpui, superview];
            if host.is_null() {
                return None;
            }
            let view = match engine {
                Engine::Look => new_look_view()?,
                Engine::Web => new_web_view()?,
            };
            // NSWindowAbove = 1.
            let _: () = msg_send![host, addSubview: &*view, positioned: 1isize, relativeTo: &*gpui];
            view
        };
        let mut surface = Self {
            slot,
            gpui,
            view,
            engine,
            source: source.clone(),
            frame: None,
            hidden: false,
            claimed: true,
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

    /// Match the GPUI bounds; AppKit's superview counts y from the bottom.
    fn place(&mut self, bounds: Bounds<Pixels>) {
        unsafe {
            let host: *mut AnyObject = msg_send![&*self.gpui, superview];
            if host.is_null() {
                return;
            }
            let host_bounds: CGRect = msg_send![host, bounds];
            let top = f64::from(f32::from(bounds.top()));
            let h = f64::from(f32::from(bounds.size.height)).max(0.);
            let frame = CGRect {
                origin: CGPoint {
                    x: f64::from(f32::from(bounds.left())),
                    y: host_bounds.size.height - top - h,
                },
                size: CGSize {
                    width: f64::from(f32::from(bounds.size.width)).max(0.),
                    height: h,
                },
            };
            if self.frame != Some(frame) {
                let _: () = msg_send![&*self.view, setFrame: frame];
                self.frame = Some(frame);
            }
        }
    }

    fn set_hidden(&mut self, hidden: bool) {
        if self.hidden != hidden {
            self.hidden = hidden;
            let ix = self.slot as usize;
            EPOCH.with(|e| {
                let mut v = e.get();
                v[ix] += 1;
                e.set(v);
            });
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

fn show(window: &Window, slot: Slot, source: &Source, bounds: Bounds<Pixels>) {
    if COVERED.with(|c| c.get()) {
        return;
    }
    let Some(gpui) = gpui_view(window) else {
        return;
    };
    SURFACES.with(|cell| {
        let mut all = cell.borrow_mut();
        let entry = &mut all[slot as usize];
        let reuse = entry.as_ref().is_some_and(|s| {
            Retained::as_ptr(&s.gpui) == Retained::as_ptr(&gpui) && s.engine == source.engine()
        });
        if !reuse {
            if let Some(old) = entry.take() {
                old.destroy();
            }
            *entry = Surface::create(slot, gpui, source);
        }
        let Some(s) = entry.as_mut() else {
            return;
        };
        if s.source != *source {
            s.load(source);
        }
        s.place(bounds);
        s.claimed = true;
        s.set_hidden(false);
    });
}

/// After a frame: hide what wasn't painted and schedule its end.
fn settle(cx: &mut App) {
    for slot in Slot::ALL {
        let hid = with(slot, |s| match s {
            Some(s) if !s.claimed && !s.hidden => {
                s.set_hidden(true);
                true
            }
            _ => false,
        });
        if !hid {
            continue;
        }
        let epoch = EPOCH.with(|e| e.get()[slot as usize]);
        cx.spawn(async move |cx| {
            cx.background_executor().timer(slot.idle()).await;
            cx.update(|_| {
                if EPOCH.with(|e| e.get()[slot as usize]) != epoch {
                    return;
                }
                let old = SURFACES.with(|c| c.borrow_mut()[slot as usize].take_if(|s| s.hidden));
                if let Some(old) = old {
                    old.destroy();
                }
            });
        })
        .detach();
    }
}

fn gpui_view(window: &Window) -> Option<Retained<AnyObject>> {
    let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<AnyObject>()) }
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
