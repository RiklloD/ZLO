//! The translucent window's backdrop, as GPUI's
//! `WindowBackgroundAppearance::Blurred` made it: the WindowServer blurs
//! the desktop behind the window (radius 80, the private CoreGraphics call
//! Terminal.app uses, as GPUI and winit do) and adds no tint of its own, so
//! the shell's and panel's fills alone set how dark the window reads.
//!
//! Rune's own option, a system material, is an `NSVisualEffectView`; its
//! popover material lays a light grey wash over the blur, which fogged the
//! whole window next to the GPUI app.

/// GPUI's blur radius.
#[cfg(target_os = "macos")]
const RADIUS: i64 = 80;

/// `App::on_window`: blur what is behind the window when it was created
/// transparent (`translucent`, fixed at launch: `design::styles`).
pub fn on_window(window: &rune::WindowHandle, translucent: bool) {
    #[cfg(target_os = "macos")]
    if translucent && let Err(e) = window.with_raw_handle(mac::blur) {
        tracing::debug!("cannot blur the window's backdrop: {e}");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (window, translucent);
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::RawWindowHandle;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        // Private, but stable for years and used by Terminal.app, GPUI and winit.
        fn CGSMainConnectionID() -> *mut AnyObject;
        fn CGSSetWindowBackgroundBlurRadius(
            connection: *mut AnyObject,
            window_number: isize,
            radius: i64,
        ) -> i32;
    }

    pub(super) fn blur(h: raw_window_handle::WindowHandle<'_>) {
        let RawWindowHandle::AppKit(appkit) = h.as_raw() else {
            return;
        };
        let view = appkit.ns_view.as_ptr().cast::<AnyObject>();
        // SAFETY: the handle's view is a live NSView on the main thread.
        let window: *mut AnyObject = unsafe { msg_send![view, window] };
        if window.is_null() {
            return;
        }
        // SAFETY: `window` is the view's live NSWindow.
        let number: isize = unsafe { msg_send![window, windowNumber] };
        // SAFETY: plain C calls on this process's WindowServer connection.
        let status = unsafe {
            CGSSetWindowBackgroundBlurRadius(CGSMainConnectionID(), number, super::RADIUS)
        };
        if status != 0 {
            tracing::debug!(status, "the WindowServer refused the backdrop blur");
        }
    }
}
