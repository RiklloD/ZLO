//! The two macOS permissions computer use needs, asked of macOS directly.
//!
//! The backend and its computer-use engine run as Numoh's children, so macOS
//! charges both permissions to Numoh: what this process sees is what the
//! engine gets. Asking adds Numoh to the System Settings list, so the user
//! only flips a switch.

use std::ffi::c_void;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Accessibility,
    ScreenRecording,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Self::Accessibility => "Accessibility",
            Self::ScreenRecording => "Screen Recording",
        }
    }

    fn settings_url(self) -> &'static str {
        match self {
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
        }
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// The permissions Numoh does not have yet.
pub fn missing() -> Vec<Permission> {
    // SAFETY: plain status queries with no arguments.
    let (ax, screen) = unsafe { (AXIsProcessTrusted(), CGPreflightScreenCaptureAccess()) };
    let mut out = Vec::new();
    if !ax {
        out.push(Permission::Accessibility);
    }
    if !screen {
        out.push(Permission::ScreenRecording);
    }
    out
}

/// Put Numoh in each missing permission's list (macOS shows its own prompt)
/// and open System Settings at the first one.
pub fn request(missing: &[Permission]) {
    for permission in missing {
        match permission {
            // SAFETY: a one-entry CFDictionary built from CoreFoundation's own
            // constants, released after the call.
            Permission::Accessibility => unsafe {
                let keys = [kAXTrustedCheckOptionPrompt];
                let values = [kCFBooleanTrue];
                let options = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &raw const kCFTypeDictionaryKeyCallBacks,
                    &raw const kCFTypeDictionaryValueCallBacks,
                );
                if !options.is_null() {
                    AXIsProcessTrustedWithOptions(options);
                    CFRelease(options);
                }
            },
            // SAFETY: no arguments; returns at once when already granted.
            Permission::ScreenRecording => unsafe {
                CGRequestScreenCaptureAccess();
            },
        }
    }
    if let Some(first) = missing.first() {
        let _ = Command::new("/usr/bin/open")
            .arg(first.settings_url())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}
