//! The one AppKit local event monitor the native views install: a click on
//! Rune's own view takes the keyboard back from a clicked page, and a
//! click or keystroke inside the Browser's page marks it touched, so the
//! Browser tab follows a navigation the person started there instead of
//! polling a resting page on a timer.

use std::cell::{Cell, RefCell};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use rune::AppHandle;

use super::appkit::CGPoint;
use super::{Engine, HOST, SURFACES, Slot, alive};
use crate::state::State;

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
    /// The person acted inside the Browser's page since [`take`] last ran.
    static PAGE_INPUT: Cell<bool> = const { Cell::new(false) };
    /// Wakes the app so its poll sees [`PAGE_INPUT`]; kept by [`keep_waker`].
    static WAKER: RefCell<Option<AppHandle<State>>> = const { RefCell::new(None) };
}

// NSEventType values.
const LEFT_DOWN: u64 = 1;
const LEFT_UP: u64 = 2;
const RIGHT_DOWN: u64 = 3;
const KEY_DOWN: u64 = 10;
const OTHER_UP: u64 = 26;

/// Keep the app's handle for the monitor's wake (once; cheap after).
pub(super) fn keep_waker(handle: &AppHandle<State>) {
    WAKER.with(|w| {
        if let Ok(mut w) = w.try_borrow_mut()
            && w.is_none()
        {
            *w = Some(handle.clone());
        }
    });
}

pub(super) fn take() -> bool {
    PAGE_INPUT.with(|p| p.replace(false))
}

/// Install the monitor once for the app's life; every event passes on
/// unchanged. Rune has no window-wide press hook, hence AppKit's.
pub(super) fn install_monitor() {
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let Some(cls) = AnyClass::get(c"NSEvent") else {
        return;
    };
    let block = block2::RcBlock::new(|event: *mut AnyObject| -> *mut AnyObject {
        if !event.is_null() && alive() {
            on_event(event);
        }
        event
    });
    const MASK: u64 =
        (1 << LEFT_DOWN) | (1 << LEFT_UP) | (1 << RIGHT_DOWN) | (1 << KEY_DOWN) | (1 << OTHER_UP);
    // SAFETY: the block matches `NSEvent *(^)(NSEvent *)`; AppKit copies it
    // and keeps the monitor until it is removed (never, here).
    unsafe {
        let _monitor: *mut AnyObject =
            msg_send![cls, addLocalMonitorForEventsMatchingMask: MASK, handler: &*block];
    }
}

fn on_event(event: *mut AnyObject) {
    // SAFETY: a live NSEvent handed to the monitor, on the main thread.
    let kind: u64 = unsafe { msg_send![event, type] };
    match kind {
        LEFT_DOWN | RIGHT_DOWN => {
            if hit_in(event, Hit::Rune) {
                reclaim_focus();
            }
        }
        // A link follows on the release, Back on a mouse's side button.
        LEFT_UP | OTHER_UP => {
            if hit_in(event, Hit::Page) {
                touched();
            }
        }
        KEY_DOWN => {
            if keys_in_page(event) {
                touched();
            }
        }
        _ => {}
    }
}

fn touched() {
    PAGE_INPUT.with(|p| p.set(true));
    WAKER.with(|w| {
        if let Ok(w) = w.try_borrow()
            && let Some(handle) = w.as_ref()
        {
            handle.wake();
        }
    });
}

/// The Browser's web view, while it exists.
fn page_view() -> Option<Retained<AnyObject>> {
    SURFACES.with(|cell| {
        let all = cell.try_borrow().ok()?;
        all[Slot::Browser as usize]
            .as_ref()
            .filter(|s| s.engine == Engine::Web && s.hidden.is_none())
            .map(|s| s.view.clone())
    })
}

#[derive(Clone, Copy)]
enum Hit {
    /// Rune's own view (not a preview lying above it).
    Rune,
    /// The Browser's page.
    Page,
}

/// Whether mouse `event` landed, in Rune's window, on `target`.
fn hit_in(event: *mut AnyObject, target: Hit) -> bool {
    let Some(view) = HOST.with(|h| h.borrow().clone()) else {
        return false;
    };
    let target = match target {
        Hit::Rune => view.clone(),
        Hit::Page => match page_view() {
            Some(page) => page,
            None => return false,
        },
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
        let inside: bool = msg_send![hit, isDescendantOf: &*target];
        inside
    }
}

/// Whether key `event` goes to the Browser's page (it holds the keyboard).
fn keys_in_page(event: *mut AnyObject) -> bool {
    let Some(page) = page_view() else {
        return false;
    };
    // SAFETY: AppKit queries on live objects, main thread, null-checked.
    unsafe {
        let win: *mut AnyObject = msg_send![event, window];
        if win.is_null() {
            return false;
        }
        let first: *mut AnyObject = msg_send![win, firstResponder];
        if first.is_null() {
            return false;
        }
        let Some(cls) = AnyClass::get(c"NSView") else {
            return false;
        };
        let is_view: bool = msg_send![first, isKindOfClass: cls];
        if !is_view {
            return false;
        }
        let inside: bool = msg_send![first, isDescendantOf: &*page];
        inside
    }
}

/// A click on Rune's view takes the keyboard back from a page clicked
/// earlier.
fn reclaim_focus() {
    let Some(view) = HOST.with(|h| h.borrow().clone()) else {
        return;
    };
    // SAFETY: AppKit calls on live objects, on the main thread.
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
