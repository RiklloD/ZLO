//! Documents and web pages inside the window, drawn by macOS itself:
//! QuickLook (`QLPreviewView`) for PDF, Word, slides,
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
//!   its siblings, ordered above it.
//! - [`surface`] is the element a tab puts where the view goes: an empty box
//!   whose drawn rect (`El::on_drawn_layout`) places the view.
//! - What the pane wants is reconciled with what is on screen whenever it
//!   can have changed, and never on a standing timer:
//!   - every view run ([`cover`], from the window's root view): Rune polls
//!     before handlers run, so the view run that follows a handler is what
//!     lands a tab switch, a closed page or an overlay in the same frame;
//!   - every drawn-rect change of a [`surface`] (resize, a dragged dock);
//!   - every [`poll`] (before each frame, also while the window is hidden);
//!   - one wake at the next moment time alone changes the answer: a dock's
//!     slide has settled, or a hidden view's idle time is up
//!     ([`next_change`]).
//!
//! The native view sits above Rune's own view, so Rune popovers can't draw
//! over it. The window's root view passes [`cover`] whether its overlay
//! layer holds anything (the ⌘K menu, a menu or popover, a dialog), and the
//! views hide meanwhile. Rune keeps keyboard focus except while the page is
//! clicked.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use raw_window_handle::RawWindowHandle;
use rune::{El, Spring, Tx, el};

use crate::state::State;

mod appkit;
mod input;
mod observe;
use appkit::{CGPoint, CGRect, CGSize, file_url, new_look_view, new_web_view, ns_string, string};

/// Added to a timed wake so it lands after its deadline, never a hair
/// before (the poll it runs would find nothing due and ask for no other).
const WAKE_PAD: Duration = Duration::from_millis(20);

/// The exits of the overlays that can close over a native view, slowest
/// first. Rune does not tell the app when a removed element's exit has
/// finished, so the hold is derived from these springs:
/// - rune-kit's anchored popover (model picker, profile pickers, menus
///   built on it) fades out on a 0.25 s removal spring
///   (`rune-kit/src/overlays/popover.rs`);
/// - a plain `Transition::opacity()` (the ⌘K scrim, kit scrims and
///   dialogs) leaves on Rune's default 0.15 s;
/// - the app's own popovers and cards (`design::motion::pop`) leave on the
///   pop spring at 60 %.
const OVERLAY_EXITS: [Spring; 3] = [
    Spring::new(0.25, 0.0),
    Spring::new(0.15, 0.0),
    Spring::new(0.22 * 0.6, 0.03),
];

/// An exiting overlay at or under this opacity no longer reads as drawn.
const EXIT_GONE: f32 = 0.02;

/// How long the views stay hidden after the overlay layer empties. A
/// closing overlay is gone from the layer at once but still plays its exit
/// where the views would draw over it, since they sit above Rune's view:
/// the slowest of [`OVERLAY_EXITS`] to fade to [`EXIT_GONE`], plus a frame.
fn cover_hold() -> Duration {
    let slowest = OVERLAY_EXITS
        .iter()
        .map(|s| s.settling_duration(EXIT_GONE))
        .fold(0f32, f32::max);
    Duration::from_secs_f32(slowest) + Duration::from_millis(17)
}

/// A view coming into sight fades in over this, on Core Animation's clock:
/// Rune draws no frames for it. Hiding stays instant, so nothing Rune
/// draws over the view's place is ever under it.
const REVEAL_FADE: f64 = 0.15;

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
    /// When the overlay layer last emptied: the views stay hidden for
    /// [`cover_hold`] after, while the overlay plays its exit.
    uncovered: Cell<Option<Instant>>,
}

impl NativeViews {
    /// Whether an overlay covers the window, as the last view run drew it.
    pub fn covered(&self) -> bool {
        self.covered.get()
    }

    /// When the hold after an overlay closed ends, while it runs.
    fn hold_ends(&self) -> Option<Instant> {
        self.uncovered
            .get()
            .map(|at| at + cover_hold())
            .filter(|end| *end > Instant::now())
    }
}

/// Every view run: mark the window covered by an overlay or not, and
/// reconcile. The root view (`workspace::shell::view`) calls it with whether
/// its overlay layer holds anything. Whatever a handler changed in this
/// frame (the tab, the page, an overlay) shows or hides the views here, at
/// once.
pub fn cover(s: &State, covered: bool) {
    let was = s.native.covered.replace(covered);
    if covered {
        s.native.uncovered.set(None);
    } else if was {
        s.native.uncovered.set(Some(Instant::now()));
    }
    reconcile(s);
    // A view run has no `Tx` to set a timed wake with: the poll does.
    if unasked_wake(s).is_some() {
        s.rt.handle.wake();
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
            input::install_monitor();
        }
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, "no native window handle for previews"),
    }
}

/// Show, place and hide the native views as the side pane wants them, and
/// ask for the one wake time alone calls for; call before every frame from
/// the app's `poll`.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    observe::keep_waker(&s.rt.handle);
    reconcile(s);
    if let Some(at) = unasked_wake(s) {
        tx.wake_after(at.saturating_duration_since(Instant::now()) + WAKE_PAD);
        WAKE.with(|w| w.set(Some(at)));
    }
    false
}

/// Each slot's source and place, or `None` to hide it.
fn plan(s: &State) -> [Option<(Source, [f32; 4])>; 2] {
    let held = s.native.covered() || s.native.hold_ends().is_some();
    let wanted = if held || s.fixture {
        [None, None]
    } else {
        crate::chat::side::native_wanted(s)
    };
    let mut ix = 0;
    wanted.map(|source| {
        let rect = s.native.rects[ix];
        ix += 1;
        source
            .zip(rect)
            .filter(|(_, rect)| rect[2] > 1. && rect[3] > 1.)
    })
}

fn reconcile(s: &State) {
    let plan = plan(s);
    #[cfg(test)]
    PLANNED.with(|p| *p.borrow_mut() = plan.clone().map(|slot| slot.map(|(_, rect)| rect)));
    for (slot, planned) in Slot::ALL.into_iter().zip(plan) {
        match planned {
            Some((source, rect)) => show(slot, &source, rect, || fades(s)),
            None => hide(slot),
        }
    }
}

/// Whether a view coming into sight fades in: not under reduced motion
/// (Settings, the battery saver, or the system's setting).
fn fades(s: &State) -> bool {
    !crate::design::battery::reduced(&s.prefs, &s.look.battery, system_reduced_motion())
}

/// macOS's Reduce motion setting.
fn system_reduced_motion() -> bool {
    let Some(cls) = AnyClass::get(c"NSWorkspace") else {
        return false;
    };
    // SAFETY: a class method and a BOOL property read on the main thread.
    unsafe {
        let ws: *mut AnyObject = msg_send![cls, sharedWorkspace];
        if ws.is_null() {
            return false;
        }
        msg_send![ws, accessibilityDisplayShouldReduceMotion]
    }
}

/// The next moment the plan changes with no state change behind it, so no
/// view run or handler to reconcile on: a dock's slide has settled or a
/// closed overlay's exit is over (its native view may show), or a hidden
/// view has been idle long enough to end.
fn next_change(s: &State) -> Option<Instant> {
    if s.fixture {
        return None;
    }
    let hidden = SURFACES.with(|cell| {
        let all = cell.borrow();
        Slot::ALL.map(|slot| all[slot as usize].as_ref().and_then(|s| s.hidden))
    });
    [
        idle_end(hidden),
        crate::chat::side::native_settles_at(s),
        s.native.hold_ends(),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// When the first of the hidden views (hidden since `hidden`, by slot) ends.
fn idle_end(hidden: [Option<Instant>; 2]) -> Option<Instant> {
    Slot::ALL
        .into_iter()
        .zip(hidden)
        .filter_map(|(slot, since)| Some(since? + slot.idle()))
        .min()
}

/// [`next_change`], unless a wake for that moment is already asked for.
fn unasked_wake(s: &State) -> Option<Instant> {
    next_change(s).filter(|at| WAKE.with(Cell::get) != Some(*at))
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

/// What the app's poll should do about the page now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageChange {
    /// Nothing changed.
    None,
    /// Read the page now.
    Read,
    /// Something changed, but the last read was too recent: wake after this.
    After(Duration),
}

/// Whether the Browser's page changed since the app last read it (its
/// observer noticed), and so whether to read it now or after a moment:
/// reads are capped at four a second (`observe.rs`). Call from the poll.
pub fn take_page_change(now: Instant) -> PageChange {
    observe::take(now)
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
    /// WebKit's `estimatedProgress` of the load, 0 to 1.
    pub progress: f64,
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
                progress: msg_send![v, estimatedProgress],
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
    /// The moment the last timed wake was asked for ([`poll`]).
    static WAKE: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// What the last reconcile planned: each slot's rect, `None` hidden.
    /// A test run has no window, so no native view to look at.
    static PLANNED: RefCell<[Option<[f32; 4]>; 2]> = const { RefCell::new([None, None]) };
}

/// What a test can see of this module: the last plan (each slot's rect, or
/// `None` hidden), the next timed change and the wake asked for.
#[cfg(test)]
pub(crate) struct Probe {
    pub planned: [Option<[f32; 4]>; 2],
    pub next_change: Option<Instant>,
    pub wake_asked: Option<Instant>,
}

/// Let the hold after a closed overlay run out, as if [`cover_hold`] had
/// passed on the wall clock.
#[cfg(test)]
pub(crate) fn end_cover_hold(s: &State) {
    if let Some(at) = s.native.uncovered.get() {
        s.native.uncovered.set(Some(at - cover_hold()));
    }
}

/// When the hold after a closed overlay ends, while it runs.
#[cfg(test)]
pub(crate) fn cover_hold_ends(s: &State) -> Option<Instant> {
    s.native.hold_ends()
}

/// What the page's observer does when WebKit notifies it, minus the wake
/// (a test has no app to wake): whether it would wake.
#[cfg(test)]
pub(crate) fn note_page_change() -> bool {
    observe::note_for_tests()
}

#[cfg(test)]
pub(crate) fn probe(s: &State) -> Probe {
    Probe {
        planned: PLANNED.with(|p| *p.borrow()),
        next_change: next_change(s),
        wake_asked: WAKE.with(Cell::get),
    }
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
    /// The Browser page's key-value observer (`observe.rs`), removed in
    /// [`Surface::destroy`] before the view goes.
    observer: Option<Retained<AnyObject>>,
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
            // Hidden until `show` places it and fades it in; layer-backed
            // so the fade is Core Animation's.
            let _: () = msg_send![&*view, setWantsLayer: true];
            let _: () = msg_send![&*view, setHidden: true];
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
            hidden: Some(Instant::now()),
            observer: None,
        };
        if slot == Slot::Browser && engine == Engine::Web {
            surface.observer = observe::attach(&surface.view);
        }
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

    /// Hide at once, or show: faded in over [`REVEAL_FADE`] when `fade`
    /// says so (asked only when the view comes into sight).
    fn set_hidden(&mut self, hidden: bool, fade: impl FnOnce() -> bool) {
        if self.hidden.is_some() == hidden {
            return;
        }
        self.hidden = hidden.then(Instant::now);
        let v = &*self.view;
        // SAFETY: AppKit calls on a live view, on the main thread.
        unsafe {
            if hidden {
                let _: () = msg_send![v, setHidden: true];
                return;
            }
            let ctx = AnyClass::get(c"NSAnimationContext").filter(|_| fade());
            let Some(ctx) = ctx else {
                let _: () = msg_send![v, setAlphaValue: 1.0f64];
                let _: () = msg_send![v, setHidden: false];
                return;
            };
            let _: () = msg_send![v, setAlphaValue: 0.0f64];
            let _: () = msg_send![v, setHidden: false];
            let _: () = msg_send![ctx, beginGrouping];
            let current: *mut AnyObject = msg_send![ctx, currentContext];
            if !current.is_null() {
                let _: () = msg_send![current, setDuration: REVEAL_FADE];
            }
            let animator: *mut AnyObject = msg_send![v, animator];
            if animator.is_null() {
                let _: () = msg_send![v, setAlphaValue: 1.0f64];
            } else {
                let _: () = msg_send![animator, setAlphaValue: 1.0f64];
            }
            let _: () = msg_send![ctx, endGrouping];
        }
    }

    fn destroy(self) {
        if let Some(observer) = &self.observer {
            observe::detach(&self.view, observer);
        }
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

fn show(slot: Slot, source: &Source, rect: [f32; 4], fade: impl FnOnce() -> bool) {
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
        s.set_hidden(false, fade);
    });
}

/// Hide `slot`, and end it once it has been hidden past its idle time.
fn hide(slot: Slot) {
    let expired = with(slot, |s| match s {
        Some(s) => {
            s.set_hidden(true, || false);
            s.hidden.is_some_and(|t| t.elapsed() >= slot.idle())
        }
        None => false,
    });
    if expired {
        close_view(slot);
    }
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

    /// The anchored popover (the model picker) fades on a 0.25 s spring:
    /// still plainly drawn at the old 180 ms hold, gone by the hold's end.
    #[test]
    fn the_cover_hold_outlasts_the_popover_fade() {
        let popover = Spring::new(0.25, 0.0);
        let opacity = |t: Duration| popover.sample(1., 0., 0., t.as_secs_f32()).0;
        assert!(opacity(Duration::from_millis(180)) > 0.05);
        assert!(opacity(cover_hold()) <= EXIT_GONE);
        assert!(cover_hold() < Duration::from_millis(400), "still brief");
    }

    #[test]
    fn the_idle_wake_is_for_the_first_hidden_view_to_end() {
        let now = Instant::now();
        assert_eq!(idle_end([None, None]), None, "nothing hidden: no wake");
        assert_eq!(
            idle_end([Some(now), None]),
            Some(now + Slot::Browser.idle())
        );
        // Hidden together, the file preview ends first.
        assert_eq!(
            idle_end([Some(now), Some(now)]),
            Some(now + Slot::Preview.idle())
        );
        // A page hidden long ago ends before a preview hidden just now.
        let long_ago = now - Duration::from_secs(100);
        assert_eq!(
            idle_end([Some(long_ago), Some(now)]),
            Some(long_ago + Slot::Browser.idle())
        );
    }
}
