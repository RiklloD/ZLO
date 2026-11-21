//! The Browser's page tells the app when it changed, instead of the app
//! polling it: one key-value observer per web view watches `URL`,
//! `loading`, `estimatedProgress`, `canGoBack`, `canGoForward` and
//! `title`. Any of them changing (a load, a redirect, a meta refresh, a
//! single-page app's `pushState`, a title set from script) marks the page
//! changed and wakes the app once. The callback runs on the main thread,
//! inside whatever WebKit was doing, so it touches no Rune state: it sets
//! a mark and wakes; the app's poll reads the page ([`take`]).
//!
//! The wakes are coalesced: after the app reads the page, the next read
//! waits at least [`GAP`] (at most 4 a second), and while a read is
//! pending no further notification wakes anything. An idle page sends no
//! notifications, so it costs no wakes at all.
//!
//! The observer's lifetime is the web view's: attached when the view is
//! made ([`attach`]) and removed before the view is torn down
//! ([`detach`], from `Surface::destroy`, which every teardown goes
//! through: closing the page, a new window, an engine change, and the
//! view ending after two minutes hidden).

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{ClassType, define_class, msg_send};
use rune::AppHandle;

use super::PageChange;
use super::appkit::ns_string;
use crate::state::State;

/// The key paths the observer watches on a `WKWebView`.
const KEYS: [&str; 6] = [
    "URL",
    "loading",
    "estimatedProgress",
    "canGoBack",
    "canGoForward",
    "title",
];

/// The shortest time between two reads of the page (4 a second at most).
pub(super) const GAP: Duration = Duration::from_millis(250);

/// The coalescing gate between WebKit's notifications and the app's reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Gate {
    /// A change was noted and not read yet (a wake is on its way or set).
    pending: bool,
    /// When the app last read the page.
    read: Option<Instant>,
}

impl Gate {
    /// WebKit says the page changed: whether to wake the app (only the
    /// first change since the last read does).
    pub(super) fn note(&mut self) -> bool {
        !std::mem::replace(&mut self.pending, true)
    }

    /// The app's poll at `now`.
    pub(super) fn take(&mut self, now: Instant) -> PageChange {
        if !self.pending {
            return PageChange::None;
        }
        if let Some(read) = self.read {
            let since = now.saturating_duration_since(read);
            if since < GAP {
                // Still pending: later notifications stay quiet, the
                // caller's timed wake brings the read.
                return PageChange::After(GAP - since);
            }
        }
        self.pending = false;
        self.read = Some(now);
        PageChange::Read
    }
}

thread_local! {
    static GATE: Cell<Gate> = const { Cell::new(Gate { pending: false, read: None }) };
    /// Wakes the app so its poll reads the page; kept by [`keep_waker`].
    static WAKER: RefCell<Option<AppHandle<State>>> = const { RefCell::new(None) };
}

/// Keep the app's handle for the observer's wake (once; cheap after).
pub(super) fn keep_waker(handle: &AppHandle<State>) {
    WAKER.with(|w| {
        if let Ok(mut w) = w.try_borrow_mut()
            && w.is_none()
        {
            *w = Some(handle.clone());
        }
    });
}

/// WebKit's notification: mark the page changed, wake the app if this is
/// the first change since its last read.
fn changed() {
    let wake = GATE.with(|g| {
        let mut gate = g.get();
        let wake = gate.note();
        g.set(gate);
        wake
    });
    if wake {
        WAKER.with(|w| {
            if let Ok(w) = w.try_borrow()
                && let Some(handle) = w.as_ref()
            {
                handle.wake();
            }
        });
    }
}

/// [`changed`]'s mark without its wake, for tests.
#[cfg(test)]
pub(super) fn note_for_tests() -> bool {
    GATE.with(|g| {
        let mut gate = g.get();
        let wake = gate.note();
        g.set(gate);
        wake
    })
}

/// The app's poll: whether to read the page now, later, or not at all.
pub(super) fn take(now: Instant) -> PageChange {
    GATE.with(|g| {
        let mut gate = g.get();
        let out = gate.take(now);
        g.set(gate);
        out
    })
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ZLORunePageObserver"]
    struct Observer;

    impl Observer {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe(
            &self,
            _key: *mut AnyObject,
            _object: *mut AnyObject,
            _change: *mut AnyObject,
            _context: *mut std::ffi::c_void,
        ) {
            changed();
        }
    }
);

/// Watch web view `view`; keep the result until [`detach`].
pub(super) fn attach(view: &AnyObject) -> Option<Retained<AnyObject>> {
    attach_keys(view, &KEYS)
}

fn attach_keys(view: &AnyObject, keys: &[&str]) -> Option<Retained<AnyObject>> {
    // SAFETY: a plain NSObject subclass made on the main thread, registered
    // as a KVO observer of a live view; it is removed again in `detach`
    // before the view goes.
    unsafe {
        let observer: Option<Retained<AnyObject>> = msg_send![Observer::class(), new];
        let observer = observer?;
        for key in keys {
            let Some(path) = ns_string(key) else { continue };
            // NSKeyValueObservingOptionNew: the value itself is read later.
            let _: () = msg_send![
                view,
                addObserver: &*observer,
                forKeyPath: &*path,
                options: 1usize,
                context: std::ptr::null_mut::<std::ffi::c_void>()
            ];
        }
        Some(observer)
    }
}

/// Stop watching `view`; call before the view is released.
pub(super) fn detach(view: &AnyObject, observer: &AnyObject) {
    detach_keys(view, observer, &KEYS);
}

fn detach_keys(view: &AnyObject, observer: &AnyObject, keys: &[&str]) {
    // SAFETY: undoes exactly the registrations `attach` made, on the main
    // thread, while both objects are alive.
    unsafe {
        for key in keys {
            let Some(path) = ns_string(key) else { continue };
            let _: () = msg_send![view, removeObserver: observer, forKeyPath: &*path];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The observer class really receives key-value notifications and
    /// marks the gate (on an `NSProgress`, which a test can make: a web
    /// view needs a running app), and stops once detached.
    #[test]
    fn the_observer_marks_the_page_changed_until_detached() {
        let Some(cls) = objc2::runtime::AnyClass::get(c"NSProgress") else {
            return;
        };
        let progress: Option<Retained<AnyObject>> = unsafe { msg_send![cls, new] };
        let progress = progress.expect("NSProgress");
        let keys = ["completedUnitCount"];
        let observer = attach_keys(&progress, &keys).expect("observer");
        assert_eq!(take(Instant::now()), PageChange::None, "nothing yet");
        unsafe {
            let _: () = msg_send![&*progress, setCompletedUnitCount: 3i64];
        }
        assert_eq!(take(Instant::now()), PageChange::Read, "one change noted");
        detach_keys(&progress, &observer, &keys);
        unsafe {
            let _: () = msg_send![&*progress, setCompletedUnitCount: 4i64];
        }
        let later = Instant::now() + GAP;
        assert_eq!(take(later), PageChange::None, "detached: no more");
    }

    #[test]
    fn an_idle_page_reads_nothing() {
        let mut gate = Gate::default();
        assert_eq!(gate.take(Instant::now()), PageChange::None);
    }

    /// Many notifications wake once; the read clears the mark.
    #[test]
    fn notifications_coalesce_into_one_wake_and_one_read() {
        let mut gate = Gate::default();
        assert!(gate.note(), "the first change wakes");
        assert!(!gate.note(), "later ones before the read do not");
        assert!(!gate.note());
        let t0 = Instant::now();
        assert_eq!(gate.take(t0), PageChange::Read);
        assert_eq!(gate.take(t0), PageChange::None, "read once");
        assert!(gate.note(), "a change after the read wakes again");
    }

    /// A page changing all the time is read at most every GAP: a change
    /// right after a read waits, quietly, for one timed wake.
    #[test]
    fn reads_are_capped_at_four_a_second() {
        let mut gate = Gate::default();
        let t0 = Instant::now();
        gate.note();
        assert_eq!(gate.take(t0), PageChange::Read);
        assert!(gate.note());
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(
            gate.take(t1),
            PageChange::After(GAP - Duration::from_millis(100))
        );
        assert!(!gate.note(), "pending: no wake while the timed one is set");
        assert_eq!(gate.take(t0 + GAP), PageChange::Read);
    }
}
