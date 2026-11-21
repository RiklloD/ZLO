//! AppKit plumbing for the native views: loading the two frameworks on
//! first use, constructing their views, Foundation strings and URLs, and
//! the CoreGraphics geometry `msg_send!` passes by value.

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;

use objc2::encode::{Encode, Encoding};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};

// ---- framework loading and object construction ----

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

/// Load a system framework once; its classes register with the runtime.
pub(super) fn framework(path: &CStr) -> bool {
    const RTLD_LAZY: i32 = 0x1;
    unsafe { !dlopen(path.as_ptr(), RTLD_LAZY).is_null() }
}

pub(super) const ZERO: CGRect = CGRect {
    origin: CGPoint { x: 0., y: 0. },
    size: CGSize {
        width: 0.,
        height: 0.,
    },
};

pub(super) unsafe fn new_look_view() -> Option<Retained<AnyObject>> {
    if !framework(c"/System/Library/Frameworks/Quartz.framework/Quartz") {
        return None;
    }
    let cls = AnyClass::get(c"QLPreviewView")?;
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        // QLPreviewViewStyleNormal = 0.
        let view: Option<Retained<AnyObject>> =
            msg_send![alloc, initWithFrame: ZERO, style: 0usize];
        let view = view?;
        let _: () = msg_send![&*view, setShouldCloseWithWindow: false];
        Some(view)
    }
}

pub(super) unsafe fn new_web_view() -> Option<Retained<AnyObject>> {
    if !framework(c"/System/Library/Frameworks/WebKit.framework/WebKit") {
        return None;
    }
    let config_cls = AnyClass::get(c"WKWebViewConfiguration")?;
    let store_cls = AnyClass::get(c"WKWebsiteDataStore")?;
    let cls = AnyClass::get(c"WKWebView")?;
    unsafe {
        let config: Retained<AnyObject> = msg_send![config_cls, new];
        let store: Option<Retained<AnyObject>> = msg_send![store_cls, nonPersistentDataStore];
        if let Some(store) = store {
            let _: () = msg_send![&*config, setWebsiteDataStore: &*store];
        }
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        let view: Option<Retained<AnyObject>> =
            msg_send![alloc, initWithFrame: ZERO, configuration: &*config];
        view
    }
}

pub(super) unsafe fn ns_string(text: &str) -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"NSString")?;
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        // NSUTF8StringEncoding = 4.
        msg_send![
            alloc,
            initWithBytes: text.as_ptr().cast::<c_void>(),
            length: text.len(),
            encoding: 4usize
        ]
    }
}

pub(super) unsafe fn file_url(path: &Path) -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"NSURL")?;
    unsafe {
        let s = ns_string(&path.to_string_lossy())?;
        msg_send![cls, fileURLWithPath: &*s]
    }
}

pub(super) unsafe fn string(ns: *mut AnyObject) -> String {
    if ns.is_null() {
        return String::new();
    }
    unsafe {
        let utf8: *const c_char = msg_send![ns, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

// ---- CoreGraphics geometry, declared here so no binding crate is needed ----

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CGPoint {
    pub(super) x: f64,
    pub(super) y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CGSize {
    pub(super) width: f64,
    pub(super) height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CGRect {
    pub(super) origin: CGPoint,
    pub(super) size: CGSize,
}

unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}
