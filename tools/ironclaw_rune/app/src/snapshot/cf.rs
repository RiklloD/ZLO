//! The few CoreFoundation calls the capture and the AX walk share, declared
//! by hand like `permissions.rs` does (the app links no CF crate).

use std::ffi::{c_char, c_void};

pub(super) type CfRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(super) fn CFRelease(cf: CfRef);
    pub(super) fn CFRetain(cf: CfRef) -> CfRef;
    pub(super) fn CFGetTypeID(cf: CfRef) -> usize;
    pub(super) fn CFStringGetTypeID() -> usize;
    pub(super) fn CFNumberGetTypeID() -> usize;
    pub(super) fn CFBooleanGetTypeID() -> usize;
    pub(super) fn CFArrayGetTypeID() -> usize;
    pub(super) fn CFDictionaryGetTypeID() -> usize;
    pub(super) fn CFBooleanGetValue(boolean: CfRef) -> bool;
    pub(super) fn CFArrayGetCount(array: CfRef) -> isize;
    pub(super) fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    pub(super) fn CFArrayCreate(
        allocator: CfRef,
        values: *const CfRef,
        count: isize,
        callbacks: *const c_void,
    ) -> CfRef;
    pub(super) static kCFTypeArrayCallBacks: c_void;
    pub(super) fn CFDictionaryGetValue(dict: CfRef, key: CfRef) -> CfRef;
    pub(super) fn CFNumberGetValue(number: CfRef, kind: isize, out: *mut c_void) -> bool;
    fn CFStringCreateWithBytes(
        allocator: CfRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: bool,
    ) -> CfRef;
    fn CFStringGetLength(string: CfRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(string: CfRef, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
}

const UTF8: u32 = 0x0800_0100;
const NUMBER_DOUBLE: isize = 13;

/// An owned CF object (create/copy rule), released on drop.
pub(super) struct Owned(pub(super) CfRef);

impl Owned {
    pub(super) fn new(raw: CfRef) -> Option<Self> {
        (!raw.is_null()).then(|| Self(raw))
    }

    pub(super) fn string(text: &str) -> Option<Self> {
        // SAFETY: valid UTF-8 for `len` bytes; null is handled by `new`.
        Self::new(unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                text.as_ptr(),
                text.len() as isize,
                UTF8,
                false,
            )
        })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: owned by this wrapper (create/copy rule).
        unsafe { CFRelease(self.0) };
    }
}

pub(super) fn type_is(value: CfRef, type_id: usize) -> bool {
    // SAFETY: `value` is a live CF object.
    !value.is_null() && unsafe { CFGetTypeID(value) } == type_id
}

/// A CFString's text, or `None` for any other type.
pub(super) fn string(value: CfRef) -> Option<String> {
    // SAFETY: type-checked first; the buffer is sized for the worst case of
    // the string's length in UTF-8, plus the terminator.
    unsafe {
        if !type_is(value, CFStringGetTypeID()) {
            return None;
        }
        let max = CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), UTF8) + 1;
        let mut buffer = vec![0u8; usize::try_from(max).ok()?.max(1)];
        if !CFStringGetCString(value, buffer.as_mut_ptr().cast(), max, UTF8) {
            return None;
        }
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        buffer.truncate(end);
        Some(String::from_utf8_lossy(&buffer).into_owned())
    }
}

pub(super) fn number(value: CfRef) -> Option<f64> {
    // SAFETY: type-checked first; `out` is a valid f64 slot.
    unsafe {
        if !type_is(value, CFNumberGetTypeID()) {
            return None;
        }
        let mut out = 0f64;
        CFNumberGetValue(value, NUMBER_DOUBLE, (&raw mut out).cast()).then_some(out)
    }
}

/// Borrowed (get rule): valid while `dict` is.
pub(super) fn dict_get(dict: CfRef, key: &str) -> CfRef {
    let Some(key) = Owned::string(key) else {
        return std::ptr::null();
    };
    // SAFETY: `dict` is a live CFDictionary and `key` a live CFString.
    unsafe { CFDictionaryGetValue(dict, key.0) }
}
