//! On-screen window lookup through CoreGraphics (the Hermes plugin's
//! `windows.py`): which CGWindowID belongs to an app's window, its bounds in
//! top-left screen points, and the main display height (to flip Cocoa's
//! bottom-left coordinates). Empty off macOS.

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowInfo {
    pub window_id: u32,
    pub pid: i64,
    pub layer: i64,
    pub title: String,
    /// x, y, width, height in screen points, origin top-left.
    pub bounds: Option<(f64, f64, f64, f64)>,
}

/// The app's normal (layer 0) window, preferring an exact title match, then
/// the frontmost one.
pub(crate) fn find_window(pid: i64, title: Option<&str>) -> Option<WindowInfo> {
    let candidates: Vec<WindowInfo> = list_windows()
        .into_iter()
        .filter(|w| {
            w.pid == pid
                && w.layer == 0
                && w.bounds
                    .is_some_and(|(_, _, width, height)| width >= 60.0 && height >= 40.0)
        })
        .collect();
    if let Some(title) = title.filter(|title| !title.is_empty())
        && let Some(exact) = candidates.iter().find(|w| w.title == title)
    {
        return Some(exact.clone());
    }
    candidates.into_iter().next()
}

#[cfg(target_os = "macos")]
pub(crate) use mac::{list_windows, main_display_height};

#[cfg(not(target_os = "macos"))]
pub(crate) fn list_windows() -> Vec<WindowInfo> {
    Vec::new()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn main_display_height() -> Option<f64> {
    None
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, c_char, c_void};

    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef};
    use core_foundation_sys::dictionary::{
        CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef,
    };
    use core_foundation_sys::number::{
        CFNumberGetTypeID, CFNumberGetValue, CFNumberRef, kCFNumberDoubleType,
    };
    use core_foundation_sys::string::{
        CFStringCreateWithBytes, CFStringGetCString, CFStringGetTypeID, CFStringRef,
        kCFStringEncodingUTF8,
    };

    use super::WindowInfo;

    #[repr(C)]
    struct CgPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    struct CgSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    struct CgRect {
        origin: CgPoint,
        size: CgSize,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(display: u32) -> CgRect;
    }

    const ON_SCREEN_ONLY: u32 = 1;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 16;

    /// A CFString key, released on drop.
    struct Key(CFStringRef);

    impl Key {
        fn new(name: &str) -> Option<Self> {
            // SAFETY: the bytes are valid UTF-8 for `len` bytes; a null result
            // is handled.
            let raw = unsafe {
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    name.as_ptr(),
                    name.len() as _,
                    kCFStringEncodingUTF8,
                    0,
                )
            };
            (!raw.is_null()).then_some(Self(raw))
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: created by `CFStringCreateWithBytes` (create rule).
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }

    /// Borrowed (get rule): valid while the dictionary is.
    fn get(dict: CFDictionaryRef, name: &str) -> Option<CFTypeRef> {
        let key = Key::new(name)?;
        // SAFETY: `dict` is a live CFDictionary and `key` a live CFString.
        let value = unsafe { CFDictionaryGetValue(dict, key.0 as *const c_void) };
        (!value.is_null()).then_some(value)
    }

    fn number(value: Option<CFTypeRef>) -> Option<f64> {
        let value = value?;
        // SAFETY: type-checked before the cast; `out` is a valid f64 slot.
        unsafe {
            if CFGetTypeID(value) != CFNumberGetTypeID() {
                return None;
            }
            let mut out = 0f64;
            CFNumberGetValue(
                value as CFNumberRef,
                kCFNumberDoubleType,
                (&mut out as *mut f64).cast(),
            )
            .then_some(out)
        }
    }

    fn string(value: Option<CFTypeRef>) -> String {
        let Some(value) = value else {
            return String::new();
        };
        let mut buffer = [0 as c_char; 2048];
        // SAFETY: type-checked before the cast; the buffer length is passed.
        unsafe {
            if CFGetTypeID(value) != CFStringGetTypeID()
                || CFStringGetCString(
                    value as CFStringRef,
                    buffer.as_mut_ptr(),
                    buffer.len() as _,
                    kCFStringEncodingUTF8,
                ) == 0
            {
                return String::new();
            }
            CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        }
    }

    fn bounds(value: Option<CFTypeRef>) -> Option<(f64, f64, f64, f64)> {
        let value = value?;
        // SAFETY: type-checked before treating it as a dictionary.
        if unsafe { CFGetTypeID(value) != CFDictionaryGetTypeID() } {
            return None;
        }
        let dict = value as CFDictionaryRef;
        Some((
            number(get(dict, "X"))?,
            number(get(dict, "Y"))?,
            number(get(dict, "Width"))?,
            number(get(dict, "Height"))?,
        ))
    }

    /// On-screen windows, front to back.
    pub(crate) fn list_windows() -> Vec<WindowInfo> {
        // SAFETY: a copy (create rule), released below; null is handled.
        let array =
            unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, 0) };
        if array.is_null() {
            return Vec::new();
        }
        let mut out = Vec::new();
        // SAFETY: `array` stays alive until the release at the end.
        let count = unsafe { CFArrayGetCount(array) };
        for index in 0..count {
            // SAFETY: index < count; each element is a CFDictionary.
            let dict = unsafe { CFArrayGetValueAtIndex(array, index) } as CFDictionaryRef;
            if dict.is_null() {
                continue;
            }
            let (Some(window_id), Some(pid)) = (
                number(get(dict, "kCGWindowNumber")),
                number(get(dict, "kCGWindowOwnerPID")),
            ) else {
                continue;
            };
            out.push(WindowInfo {
                window_id: window_id as u32,
                pid: pid as i64,
                layer: number(get(dict, "kCGWindowLayer")).unwrap_or(0.0) as i64,
                title: string(get(dict, "kCGWindowName")),
                bounds: bounds(get(dict, "kCGWindowBounds")),
            });
        }
        // SAFETY: balances the copy above.
        unsafe { CFRelease(array as CFTypeRef) };
        out
    }

    pub(crate) fn main_display_height() -> Option<f64> {
        // SAFETY: plain value-returning CoreGraphics calls.
        let height = unsafe { CGDisplayBounds(CGMainDisplayID()).size.height };
        (height > 0.0).then_some(height)
    }
}
