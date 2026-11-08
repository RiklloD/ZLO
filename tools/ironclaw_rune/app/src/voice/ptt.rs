//! The push-to-talk key: hold a lone modifier (Right Option by default).
//!
//! A modifier on its own is not something `RegisterEventHotKey` can watch,
//! and an event tap needs Input Monitoring. Like the SnapShot trigger
//! (`snapshot/hotkey.rs`), a small thread asks the window server for key
//! state instead, which needs no permission: 10 times a second while the
//! shortcut is on (the SnapShot watcher's own cadence), 30 while the key is
//! down so release lands fast. Nothing runs while the shortcut is off.
//!
//! With Accessibility granted (computer use asks for it), modifier changes
//! anywhere reach an `NSEvent` flags-changed monitor, and the thread sleeps
//! until one arrives instead of waking 10 times a second: the shortcut then
//! costs nothing while no modifier moves. It still samples key state the
//! same way once woken, so the detector is the same either way.
//!
//! Holding is what counts. A quick tap does nothing, the take starts after
//! [`HOLD`], and any other key or modifier pressed during the hold (Option+E
//! typing é) cancels it, so the key keeps its normal job.

use std::time::{Duration, Instant};

/// How long the key must be held alone before the mic opens.
pub(crate) const HOLD: Duration = Duration::from_millis(180);

/// The modifier keys Settings offers, as saved names and labels.
pub(crate) const KEYS: [(&str, &str); 4] = [
    ("right_alt", "R ⌥"),
    ("right_cmd", "R ⌘"),
    ("right_ctrl", "R ⌃"),
    ("fn", "fn"),
];

/// One look at the keyboard.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub key_down: bool,
    /// Any other modifier is down.
    pub other_modifiers: bool,
    /// The system's running count of key presses (any key).
    pub key_presses: u32,
    pub at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PttEvent {
    Start,
    Stop,
    /// Part of a chord after all: drop what was recorded.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Up,
    /// Down alone since `since`, `presses` key presses seen then.
    Pending {
        since: Instant,
        presses: u32,
    },
    Active {
        presses: u32,
    },
    /// A chord: wait for release and do nothing.
    Chord,
}

pub(crate) struct Detector {
    state: State,
}

impl Default for Detector {
    fn default() -> Self {
        Self { state: State::Up }
    }
}

impl Detector {
    /// Whether the key is down (poll faster).
    pub fn held(&self) -> bool {
        self.state != State::Up
    }

    pub fn update(&mut self, s: Sample) -> Option<PttEvent> {
        let (next, event) = match self.state {
            State::Up if s.key_down && !s.other_modifiers => (
                State::Pending {
                    since: s.at,
                    presses: s.key_presses,
                },
                None,
            ),
            State::Up if s.key_down => (State::Chord, None),
            State::Up => (State::Up, None),
            State::Pending { .. } if !s.key_down => (State::Up, None),
            State::Pending { presses, .. } if s.other_modifiers || s.key_presses != presses => {
                (State::Chord, None)
            }
            State::Pending { since, presses } if s.at.duration_since(since) >= HOLD => {
                (State::Active { presses }, Some(PttEvent::Start))
            }
            pending @ State::Pending { .. } => (pending, None),
            State::Active { .. } if !s.key_down => (State::Up, Some(PttEvent::Stop)),
            State::Active { presses } if s.other_modifiers || s.key_presses != presses => {
                (State::Chord, Some(PttEvent::Cancel))
            }
            active @ State::Active { .. } => (active, None),
            State::Chord if !s.key_down => (State::Up, None),
            State::Chord => (State::Chord, None),
        };
        self.state = next;
        event
    }
}

#[cfg(target_os = "macos")]
pub(crate) use mac::Watcher;

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    use futures::channel::mpsc::{UnboundedReceiver, unbounded};

    use super::{Detector, PttEvent, Sample};

    const POLL_IDLE: Duration = Duration::from_millis(100);
    const POLL_HELD: Duration = Duration::from_millis(33);
    /// Event-driven: a look anyway after this long without a modifier
    /// change, in case a monitor missed one.
    const EVENT_SAFETY: Duration = Duration::from_secs(5);
    /// NSEventMaskFlagsChanged.
    const FLAGS_CHANGED: u64 = 1 << 12;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    /// Set by the flags-changed monitors, taken by the watcher thread.
    #[derive(Default)]
    struct Signal {
        changed: Mutex<bool>,
        cv: Condvar,
    }

    impl Signal {
        fn notify(&self) {
            crate::energy::hit(crate::energy::C::PttWake);
            if let Ok(mut changed) = self.changed.lock() {
                *changed = true;
                self.cv.notify_one();
            }
        }

        /// Sleep until a modifier changes (or `timeout`).
        fn wait(&self, timeout: Duration) {
            let Ok(guard) = self.changed.lock() else {
                std::thread::sleep(timeout);
                return;
            };
            if let Ok((mut changed, _)) = self.cv.wait_timeout_while(guard, timeout, |c| !*c) {
                *changed = false;
            }
        }
    }

    /// The global and local `NSEvent` monitors; removed on drop. Installed
    /// and dropped on the main thread (the voice session lives there).
    struct Monitors(Vec<*mut AnyObject>);

    impl Monitors {
        fn install(signal: &Arc<Signal>) -> Option<Self> {
            // SAFETY: a constant-free C query.
            if !unsafe { AXIsProcessTrusted() } {
                return None;
            }
            let cls = AnyClass::get(c"NSEvent")?;
            let global = {
                let signal = signal.clone();
                block2::RcBlock::new(move |_event: *mut AnyObject| signal.notify())
            };
            let local = {
                let signal = signal.clone();
                block2::RcBlock::new(move |event: *mut AnyObject| -> *mut AnyObject {
                    signal.notify();
                    event
                })
            };
            // SAFETY: the blocks match `void (^)(NSEvent *)` and
            // `NSEvent *(^)(NSEvent *)`; AppKit copies them and keeps each
            // monitor until `removeMonitor:`. The returned monitors are
            // retained by AppKit while installed.
            let monitors: Vec<*mut AnyObject> = unsafe {
                vec![
                    msg_send![cls, addGlobalMonitorForEventsMatchingMask: FLAGS_CHANGED, handler: &*global],
                    msg_send![cls, addLocalMonitorForEventsMatchingMask: FLAGS_CHANGED, handler: &*local],
                ]
            };
            if monitors.iter().any(|m| m.is_null()) {
                drop(Self(monitors));
                return None;
            }
            Some(Self(monitors))
        }
    }

    impl Drop for Monitors {
        fn drop(&mut self) {
            let Some(cls) = AnyClass::get(c"NSEvent") else {
                return;
            };
            for monitor in self.0.drain(..).filter(|m| !m.is_null()) {
                // SAFETY: a monitor `add…Monitor…` returned, removed once.
                unsafe {
                    let _: () = msg_send![cls, removeMonitor: monitor];
                }
            }
        }
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state: i32) -> u64;
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceCounterForEventType(state: i32, event: u32) -> u32;
    }

    /// kCGEventSourceStateHIDSystemState.
    const HID_STATE: i32 = 1;
    /// kCGEventKeyDown.
    const KEY_DOWN: u32 = 10;
    const SHIFT: u64 = 0x0002_0000;
    const CONTROL: u64 = 0x0004_0000;
    const ALTERNATE: u64 = 0x0008_0000;
    const COMMAND: u64 = 0x0010_0000;
    const FN: u64 = 0x0080_0000;
    /// NX_DEVICE*KEYMASK: which side of a modifier pair is down.
    const LEFT_ALT: u64 = 0x20;
    const LEFT_CMD: u64 = 0x08;
    const LEFT_CTRL: u64 = 0x01;

    /// (virtual key code, its own flag, its left twin's device bit).
    fn key(name: &str) -> (u16, u64, u64) {
        match name {
            "right_cmd" => (54, COMMAND, LEFT_CMD),
            "right_ctrl" => (62, CONTROL, LEFT_CTRL),
            "fn" => (63, FN, 0),
            _ => (61, ALTERNATE, LEFT_ALT),
        }
    }

    /// Live while the shortcut is on; dropping it stops the thread.
    pub(crate) struct Watcher {
        stop: Arc<AtomicBool>,
        signal: Arc<Signal>,
        _monitors: Option<Monitors>,
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Wake an event-driven thread so it sees the stop.
            self.signal.notify();
        }
    }

    impl Watcher {
        pub(crate) fn new(name: &str) -> Result<(Self, UnboundedReceiver<PttEvent>), String> {
            let (tx, rx) = unbounded();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let (code, own, twin) = key(name);
            let signal = Arc::new(Signal::default());
            let monitors = Monitors::install(&signal);
            let event_driven = monitors.is_some();
            let wake = signal.clone();
            tracing::debug!(event_driven, "push-to-talk watcher");
            std::thread::Builder::new()
                .name("voice-ptt".into())
                .spawn(move || {
                    let mut detector = Detector::default();
                    while !flag.load(Ordering::Relaxed) {
                        // SAFETY: plain state queries with constant arguments.
                        let sample = unsafe {
                            let flags = CGEventSourceFlagsState(HID_STATE);
                            let others = (SHIFT | CONTROL | ALTERNATE | COMMAND | FN) & !own;
                            Sample {
                                key_down: CGEventSourceKeyState(HID_STATE, code),
                                // Both Options down is a chord too.
                                other_modifiers: flags & others != 0
                                    || (twin != 0 && flags & twin != 0),
                                key_presses: CGEventSourceCounterForEventType(HID_STATE, KEY_DOWN),
                                at: Instant::now(),
                            }
                        };
                        if let Some(event) = detector.update(sample)
                            && tx.unbounded_send(event).is_err()
                        {
                            break;
                        }
                        if detector.held() {
                            std::thread::sleep(POLL_HELD);
                        } else if event_driven {
                            wake.wait(EVENT_SAFETY);
                        } else {
                            std::thread::sleep(POLL_IDLE);
                        }
                    }
                })
                .map_err(|e| format!("The push-to-talk watcher did not start ({e})."))?;
            Ok((
                Self {
                    stop,
                    signal,
                    _monitors: monitors,
                },
                rx,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Keys {
        d: Detector,
        t0: Instant,
        presses: u32,
    }

    impl Keys {
        fn new() -> Self {
            Self {
                d: Detector::default(),
                t0: Instant::now(),
                presses: 7,
            }
        }
        fn at(&mut self, ms: u64, down: bool, others: bool) -> Option<PttEvent> {
            self.d.update(Sample {
                key_down: down,
                other_modifiers: others,
                key_presses: self.presses,
                at: self.t0 + Duration::from_millis(ms),
            })
        }
    }

    #[test]
    fn hold_starts_and_release_stops() {
        let mut k = Keys::new();
        assert_eq!(k.at(0, true, false), None);
        assert!(k.d.held());
        assert_eq!(k.at(100, true, false), None);
        assert_eq!(k.at(200, true, false), Some(PttEvent::Start));
        assert_eq!(k.at(2000, true, false), None);
        assert_eq!(k.at(2030, false, false), Some(PttEvent::Stop));
        assert!(!k.d.held());
    }

    #[test]
    fn a_tap_does_nothing() {
        let mut k = Keys::new();
        k.at(0, true, false);
        assert_eq!(k.at(100, false, false), None);
        assert_eq!(k.at(300, false, false), None);
    }

    #[test]
    fn typing_during_the_hold_is_a_chord() {
        let mut k = Keys::new();
        k.at(0, true, false);
        k.presses += 1;
        assert_eq!(k.at(100, true, false), None);
        assert_eq!(k.at(500, true, false), None, "a chord never starts");
        assert_eq!(k.at(600, false, false), None);
    }

    #[test]
    fn typing_after_the_start_cancels() {
        let mut k = Keys::new();
        k.at(0, true, false);
        assert_eq!(k.at(200, true, false), Some(PttEvent::Start));
        k.presses += 1;
        assert_eq!(k.at(300, true, false), Some(PttEvent::Cancel));
        assert_eq!(k.at(400, false, false), None);
    }

    #[test]
    fn other_modifiers_make_a_chord() {
        let mut k = Keys::new();
        // Pressed together with Shift.
        assert_eq!(k.at(0, true, true), None);
        assert_eq!(k.at(300, true, false), None);
        k.at(400, false, false);
        // Shift added during an active take.
        k.at(500, true, false);
        assert_eq!(k.at(700, true, false), Some(PttEvent::Start));
        assert_eq!(k.at(800, true, true), Some(PttEvent::Cancel));
    }
}
