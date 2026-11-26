//! The AppKit side of the native views (`super`): QuickLook and WebKit
//! views placed as siblings of Rune's content `NSView`.

use std::time::Instant;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use raw_window_handle::RawWindowHandle;

use super::appkit::{
    CGPoint, CGRect, CGSize, file_url, new_look_view, new_web_view, ns_string, string,
};
use super::{Engine, RESUME, REVEAL_FADE, Slot, Source, WebCommand, WebState, input, observe};

/// Native views exist here.
pub(super) const AVAILABLE: bool = true;

/// The window's content view.
pub(super) type Host = Retained<AnyObject>;

/// The content view of a Rune window, if it has an AppKit one.
pub(super) fn host_of(window: &rune::WindowHandle) -> Result<Option<Host>, String> {
    window
        .with_raw_handle(|handle| match handle.as_raw() {
            // SAFETY: winit's live content view, retained for as long as we keep it.
            RawWindowHandle::AppKit(h) => unsafe {
                Retained::retain(h.ns_view.as_ptr().cast::<AnyObject>())
            },
            _ => None,
        })
        .map_err(|e| e.to_string())
}

pub(super) fn same_host(a: &Host, b: &Host) -> bool {
    Retained::as_ptr(a) == Retained::as_ptr(b)
}

pub(super) fn install_input_monitor() {
    input::install_monitor();
}

pub(super) fn keep_waker(handle: &rune::AppHandle<crate::state::State>) {
    observe::keep_waker(handle);
}

pub(super) fn take_page_change(now: Instant) -> super::PageChange {
    observe::take(now)
}

#[cfg(test)]
pub(super) fn note_page_change_for_tests() -> bool {
    observe::note_for_tests()
}

/// macOS's Reduce motion setting.
pub(super) fn system_reduced_motion() -> bool {
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

pub(super) struct Surface {
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
    pub(super) fn create(slot: Slot, rune: Host, source: &Source) -> Option<Self> {
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

    pub(super) fn load(&mut self, source: &Source) {
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
    pub(super) fn place(&mut self, rect: [f32; 4]) {
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
    pub(super) fn set_hidden(&mut self, hidden: bool, fade: impl FnOnce() -> bool) {
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

    pub(super) fn destroy(self) {
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

impl Surface {
    pub(super) fn host(&self) -> &Host {
        &self.rune
    }

    pub(super) fn engine(&self) -> Engine {
        self.engine
    }

    pub(super) fn source(&self) -> &Source {
        &self.source
    }

    /// Hidden since then, awaiting its idle end.
    pub(super) fn hidden_since(&self) -> Option<Instant> {
        self.hidden
    }

    /// Reload what it shows (its file was rewritten).
    pub(super) fn refresh(&self) {
        // SAFETY: AppKit calls on a live view, on the main thread.
        unsafe {
            match self.engine {
                Engine::Look => {
                    let _: () = msg_send![&*self.view, refreshPreviewItem];
                }
                Engine::Web => {
                    let _: *mut AnyObject = msg_send![&*self.view, reload];
                }
            }
        }
    }

    /// The page a web view shows.
    pub(super) fn web_state(&self) -> WebState {
        let v = &*self.view;
        // SAFETY: WKWebView property reads on a live view, on the main thread.
        unsafe {
            let title: *mut AnyObject = msg_send![v, title];
            WebState {
                title: string(title),
                url: current_url(v),
                loading: msg_send![v, isLoading],
                progress: msg_send![v, estimatedProgress],
                back: msg_send![v, canGoBack],
                forward: msg_send![v, canGoForward],
            }
        }
    }

    /// Drive a web view's page.
    pub(super) fn web(&self, command: WebCommand) {
        let v = &*self.view;
        // SAFETY: WKWebView calls on a live view, on the main thread.
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
    }
}
