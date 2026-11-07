//! The SnapShot trigger: both Shift keys together, or a fallback key combo.
//!
//! Both-Shift is a modifier-only chord, which `RegisterEventHotKey` cannot
//! express. An NSEvent global monitor or a CGEventTap would see it, but
//! both need a privacy grant (Accessibility or Input Monitoring) just to
//! watch keys. Asking the window server for the current modifier state
//! needs none, so a small thread polls it while SnapShots are on: 10 times
//! a second, 20 while a Shift key is down so the chord lands just as fast.
//! One cheap call per tick, nothing at all while SnapShots are off.
//!
//! Rune port of the GPUI `snapshot/hotkey.rs`. The fallback combo now goes
//! through Rune's global shortcuts (`AppHandle::register_shortcut`, Carbon
//! `RegisterEventHotKey` underneath, still without any permission), so it
//! shares one registry with the app's other hotkeys; this file keeps the
//! Shift watcher and the combo's spelling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const POLL_IDLE: Duration = Duration::from_millis(100);
const POLL_SHIFT: Duration = Duration::from_millis(50);

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

/// Live while SnapShots are on; dropping it stops the Shift watcher.
pub(super) struct ShiftWatcher {
    stop: Arc<AtomicBool>,
}

impl Drop for ShiftWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl ShiftWatcher {
    /// Start the watcher; `pressed` runs (on its thread) each time both
    /// Shift keys go down together.
    pub(super) fn new(pressed: impl Fn() + Send + 'static) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("snapshot-shift".into())
            .spawn(move || {
                let mut held = false;
                while !flag.load(Ordering::Relaxed) {
                    let (any, both) = shifts_down();
                    if both && !held {
                        pressed();
                    }
                    held = both;
                    std::thread::sleep(if any { POLL_SHIFT } else { POLL_IDLE });
                }
            })
            .map_err(|e| format!("The Shift watcher did not start ({e})."))?;
        Ok(Self { stop })
    }
}

/// (any Shift down, both Shifts down).
fn shifts_down() -> (bool, bool) {
    // SAFETY: plain state queries with constant arguments.
    unsafe {
        let flags = CGEventSourceFlagsState(HID_STATE);
        if flags & SHIFT_MASK == 0 {
            return (false, false);
        }
        let both = flags & BOTH_SHIFT_DEVICE_BITS == BOTH_SHIFT_DEVICE_BITS
            || (CGEventSourceKeyState(HID_STATE, LEFT_SHIFT)
                && CGEventSourceKeyState(HID_STATE, RIGHT_SHIFT));
        (true, both)
    }
}

/// A saved `ctrl-alt-s` style combo as a Rune shortcut (`ctrl+alt+s`).
pub(super) fn shortcut(combo: &str) -> Result<rune::Shortcut, String> {
    let mut has_modifier = false;
    let mut key = None;
    for part in combo.split('-') {
        match part {
            "cmd" | "shift" | "alt" | "ctrl" => has_modifier = true,
            k => key = Some(k),
        }
    }
    let key = key
        .ok_or("The SnapShot shortcut has no key.")?
        .to_ascii_lowercase();
    if !has_modifier {
        return Err("The SnapShot shortcut needs a modifier.".into());
    }
    let accel = combo.replace('-', "+");
    accel
        .parse::<rune::Shortcut>()
        .map_err(|_| format!("Unsupported SnapShot key {key}."))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_fallback_parses() {
        for (combo, sign) in super::super::FALLBACKS {
            if combo == "none" {
                continue;
            }
            assert!(shortcut(combo).is_ok(), "{combo}");
            assert_eq!(label(combo), sign);
        }
    }

    #[test]
    fn a_combo_needs_a_modifier_and_a_key() {
        assert!(shortcut("s").is_err());
        assert!(shortcut("ctrl-alt").is_err());
    }
}
