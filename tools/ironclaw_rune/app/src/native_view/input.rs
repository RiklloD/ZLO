//! The one AppKit local event monitor the native views install: a click on
//! Rune's own view takes the keyboard back from a page clicked earlier.
//! (What the page itself does reaches the app through its observer,
//! `observe.rs`, not through input.)

use std::cell::Cell;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};

use super::appkit::CGPoint;
use super::{HOST, alive};

thread_local! {
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown.
const MASK: u64 = (1 << 1) | (1 << 3);

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
        if !event.is_null() && alive() && click_on_rune(event) {
            reclaim_focus();
        }
        event
    });
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
