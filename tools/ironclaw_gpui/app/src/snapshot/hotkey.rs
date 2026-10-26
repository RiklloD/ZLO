//! The SnapShot trigger: both Shift keys together, or a fallback key combo.
//!
//! Both-Shift is a modifier-only chord, which `RegisterEventHotKey` cannot
//! express. An NSEvent global monitor or a CGEventTap would see it, but
//! both need a privacy grant (Accessibility or Input Monitoring) just to
//! watch keys. Asking the window server for the current modifier state
//! needs none, so a small thread polls it 20 times a second while SnapShots
//! are on: one cheap call per tick, nothing at all while they are off.
//!
//! The fallback combo goes through HIToolbox like the quick composer's
//! shortcut, also without any permission. Its handler passes on presses of
//! other hotkeys, so the two never steal each other's keys.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

const POLL: Duration = Duration::from_millis(50);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
}

/// kCGEventSourceStateHIDSystemState: the hardware's view, any app in front.
const HID_STATE: i32 = 1;
const SHIFT_MASK: u64 = 0x0002_0000;
/// NX_DEVICELSHIFTKEYMASK | NX_DEVICERSHIFTKEYMASK.
const BOTH_SHIFT_DEVICE_BITS: u64 = 0x2 | 0x4;
const LEFT_SHIFT: u16 = 56;
const RIGHT_SHIFT: u16 = 60;

/// Live while SnapShots are on; dropping it stops both triggers.
pub(super) struct Trigger {
    stop: Arc<AtomicBool>,
    _combo: Option<Combo>,
}

impl Drop for Trigger {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Trigger {
    /// Start the Shift watcher, plus the fallback combo when one is set.
    /// A combo that cannot be registered comes back as the warning.
    pub(super) fn new(fallback: &str) -> (Self, UnboundedReceiver<()>, Option<String>) {
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let shift_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("snapshot-shift".into())
            .spawn(move || {
                let mut held = false;
                while !flag.load(Ordering::Relaxed) {
                    let both = both_shifts_down();
                    if both && !held && shift_tx.unbounded_send(()).is_err() {
                        break;
                    }
                    held = both;
                    std::thread::sleep(POLL);
                }
            });
        let mut warning = spawned
            .err()
            .map(|e| format!("The Shift watcher did not start ({e})."));
        let combo = match fallback.trim() {
            "" | "none" => None,
            shortcut => match Combo::new(shortcut, tx) {
                Ok(combo) => Some(combo),
                Err(e) => {
                    warning = Some(e);
                    None
                }
            },
        };
        (
            Self {
                stop,
                _combo: combo,
            },
            rx,
            warning,
        )
    }
}

fn both_shifts_down() -> bool {
    // SAFETY: plain state queries with constant arguments.
    unsafe {
        let flags = CGEventSourceFlagsState(HID_STATE);
        if flags & SHIFT_MASK == 0 {
            return false;
        }
        flags & BOTH_SHIFT_DEVICE_BITS == BOTH_SHIFT_DEVICE_BITS
            || (CGEventSourceKeyState(HID_STATE, LEFT_SHIFT)
                && CGEventSourceKeyState(HID_STATE, RIGHT_SHIFT))
    }
}

#[repr(C)]
struct EventType {
    class: u32,
    kind: u32,
}

#[repr(C)]
#[derive(Default)]
struct HotKeyId {
    signature: u32,
    id: u32,
}

type Handler = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        count: usize,
        types: *const EventType,
        data: *mut c_void,
        reference: *mut *mut c_void,
    ) -> i32;
    fn RemoveEventHandler(reference: *mut c_void) -> i32;
    fn RegisterEventHotKey(
        code: u32,
        modifiers: u32,
        id: HotKeyId,
        target: *mut c_void,
        options: u32,
        reference: *mut *mut c_void,
    ) -> i32;
    fn UnregisterEventHotKey(reference: *mut c_void) -> i32;
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        desired: u32,
        actual: *mut u32,
        size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> i32;
}

const SIGNATURE: u32 = u32::from_be_bytes(*b"NmSn");
/// eventNotHandledErr: let the next handler (the quick composer's) see it.
const NOT_HANDLED: i32 = -9874;

/// A registered fallback combo, unregistered on drop (main thread).
struct Combo {
    hotkey: *mut c_void,
    handler: *mut c_void,
    // Boxed so its address stays put while HIToolbox holds it.
    _sender: Box<UnboundedSender<()>>,
}

unsafe extern "C" fn pressed(_next: *mut c_void, event: *mut c_void, data: *mut c_void) -> i32 {
    let mut id = HotKeyId::default();
    // SAFETY: reads the event's hotkey id into a slot of its exact size.
    let error = unsafe {
        GetEventParameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            std::ptr::null_mut(),
            std::mem::size_of::<HotKeyId>(),
            std::ptr::null_mut(),
            (&raw mut id).cast(),
        )
    };
    if error != 0 || id.signature != SIGNATURE || id.id != 1 {
        return NOT_HANDLED;
    }
    // SAFETY: `data` is the boxed sender, alive until the handler is
    // removed in `Drop`, on the same main thread.
    let sender = unsafe { &*data.cast::<UnboundedSender<()>>() };
    let _ = sender.unbounded_send(());
    0
}

impl Combo {
    fn new(shortcut: &str, tx: UnboundedSender<()>) -> Result<Self, String> {
        let (code, modifiers) = parse(shortcut)?;
        let mut sender = Box::new(tx);
        let mut handler = std::ptr::null_mut();
        let mut hotkey = std::ptr::null_mut();
        // SAFETY: plain HIToolbox calls on the main thread with valid
        // out-pointers; the sender box outlives the handler (see Drop).
        unsafe {
            let target = GetApplicationEventTarget();
            let event = EventType {
                class: u32::from_be_bytes(*b"keyb"),
                // kEventHotKeyPressed
                kind: 5,
            };
            let error = InstallEventHandler(
                target,
                pressed,
                1,
                &event,
                (&mut *sender as *mut UnboundedSender<()>).cast(),
                &mut handler,
            );
            if error != 0 {
                return Err(format!(
                    "The SnapShot shortcut could not be set up ({error})."
                ));
            }
            let id = HotKeyId {
                signature: SIGNATURE,
                id: 1,
            };
            let error = RegisterEventHotKey(code, modifiers, id, target, 0, &mut hotkey);
            if error != 0 {
                RemoveEventHandler(handler);
                return Err(format!("{} is taken by another app.", label(shortcut)));
            }
        }
        Ok(Self {
            hotkey,
            handler,
            _sender: sender,
        })
    }
}

impl Drop for Combo {
    fn drop(&mut self) {
        // SAFETY: both references came from successful registrations.
        unsafe {
            UnregisterEventHotKey(self.hotkey);
            RemoveEventHandler(self.handler);
        }
    }
}

/// `ctrl-alt-s` style: Carbon modifier bits and an ANSI virtual key code.
fn parse(shortcut: &str) -> Result<(u32, u32), String> {
    const KEYS: &str = "asdfhgzxcv_bqweryt123465_97_80_ou_ip_lj_k____nm";
    let mut modifiers = 0;
    let mut key = None;
    for part in shortcut.split('-') {
        match part {
            "cmd" => modifiers |= 256,
            "shift" => modifiers |= 512,
            "alt" => modifiers |= 2048,
            "ctrl" => modifiers |= 4096,
            k => key = Some(k),
        }
    }
    let key = key
        .ok_or("The SnapShot shortcut has no key.")?
        .to_ascii_lowercase();
    let code = match key.as_str() {
        "space" => 49,
        k if k.chars().count() == 1 && k != "_" => KEYS
            .find(k)
            .map(|ix| ix as u32)
            .ok_or_else(|| format!("Unsupported SnapShot key {k}."))?,
        other => return Err(format!("Unsupported SnapShot key {other}.")),
    };
    if modifiers == 0 {
        return Err("The SnapShot shortcut needs a modifier.".into());
    }
    Ok((code, modifiers))
}

/// `ctrl-alt-s` as ⌃⌥S, for messages and Settings.
pub(super) fn label(shortcut: &str) -> String {
    let mut out = String::new();
    let parts: Vec<&str> = shortcut.split('-').collect();
    for (name, sign) in [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")] {
        if parts.contains(&name) {
            out.push_str(sign);
        }
    }
    if let Some(key) = parts
        .iter()
        .find(|p| !["ctrl", "alt", "shift", "cmd"].contains(*p))
    {
        out.push_str(&key.to_uppercase());
    }
    out
}
