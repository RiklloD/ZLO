//! Folders the ZLO desktop app was given one at a time (through macOS's
//! open panel), opened in this backend process too.
//!
//! macOS ties that access to the process that holds it, and the app runs
//! `serve` as its child: the app hands over a bookmark per folder and this
//! process resolves each one and starts accessing it, for its lifetime.
//! The app's side is `tools/ironclaw_rune/app/src/folder_access.rs`.
//!
//! Hand-over: when `ZLO_FOLDER_GRANTS=stdin` is set, stdin carries one
//! hex-encoded bookmark per line, then EOF. Bookmarks are capabilities, so they
//! never travel in argv or env, and are never logged. Only the app sets the
//! variable; without it stdin is not read. Bounded in size and count.

use std::io::Read;

const ENV: &str = "ZLO_FOLDER_GRANTS";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_GRANTS: usize = 256;

/// Read and open the app's folder grants, if it sent any. Never fails the
/// boot: a folder that does not open is left to macOS's own prompt.
pub(crate) fn open_from_app() {
    if std::env::var(ENV).as_deref() != Ok("stdin") {
        return;
    }
    let mut raw = String::new();
    if let Err(error) = std::io::stdin().take(MAX_BYTES).read_to_string(&mut raw) {
        tracing::debug!(%error, "folder grants unreadable");
        return;
    }
    let bookmarks = parse(&raw);
    let opened = bookmarks.iter().filter(|b| mac::open(b)).count();
    tracing::debug!(sent = bookmarks.len(), opened, "folder grants from the app");
}

/// The bookmarks in the hand-over text: non-empty hex lines, at most
/// [`MAX_GRANTS`].
fn parse(raw: &str) -> Vec<Vec<u8>> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(MAX_GRANTS)
        .filter_map(|l| hex::decode(l).ok())
        .filter(|b| !b.is_empty())
        .collect()
}

#[cfg(target_os = "macos")]
mod mac {
    //! CoreFoundation's C API: no Objective-C runtime needed.

    use std::ffi::c_void;

    type CFTypeRef = *const c_void;
    type CFIndex = isize;
    type CFOptionFlags = usize;
    type Boolean = u8;

    /// `kCFURLBookmarkResolutionWithoutUIMask` |
    /// `kCFURLBookmarkResolutionWithoutMountingMask`. No security-scope
    /// flag: the app sends implicitly scoped bookmarks, made from URLs it
    /// is accessing, which is how macOS passes access to another process.
    const RESOLVE: CFOptionFlags = (1 << 8) | (1 << 9);

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: CFIndex) -> CFTypeRef;
        fn CFURLCreateByResolvingBookmarkData(
            allocator: CFTypeRef,
            bookmark: CFTypeRef,
            options: CFOptionFlags,
            relative_to_url: CFTypeRef,
            resource_properties_to_include: CFTypeRef,
            is_stale: *mut Boolean,
            error: *mut CFTypeRef,
        ) -> CFTypeRef;
        fn CFURLStartAccessingSecurityScopedResource(url: CFTypeRef) -> Boolean;
        fn CFRelease(cf: CFTypeRef);
    }

    /// Resolve one bookmark and start accessing it for the life of the
    /// process (the URL is kept, never released).
    pub(super) fn open(bookmark: &[u8]) -> bool {
        let Ok(len) = CFIndex::try_from(bookmark.len()) else {
            return false;
        };
        // SAFETY: plain CoreFoundation calls; every object created here is
        // released, except the accessing URL, which is kept on purpose.
        unsafe {
            let data = CFDataCreate(std::ptr::null(), bookmark.as_ptr(), len);
            if data.is_null() {
                return false;
            }
            let mut stale: Boolean = 0;
            let mut error: CFTypeRef = std::ptr::null();
            let url = CFURLCreateByResolvingBookmarkData(
                std::ptr::null(),
                data,
                RESOLVE,
                std::ptr::null(),
                std::ptr::null(),
                &mut stale,
                &mut error,
            );
            CFRelease(data);
            if !error.is_null() {
                CFRelease(error);
            }
            if url.is_null() {
                return false;
            }
            if CFURLStartAccessingSecurityScopedResource(url) == 0 {
                CFRelease(url);
                return false;
            }
            true
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    pub(super) fn open(_: &[u8]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_over_keeps_only_bounded_hex_lines() {
        assert_eq!(
            parse("0102\n\n  0304  \nnot hex!\n"),
            vec![vec![1, 2], vec![3, 4]]
        );
        let many = "01\n".repeat(MAX_GRANTS + 10);
        assert_eq!(parse(&many).len(), MAX_GRANTS);
    }
}
