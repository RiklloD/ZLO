//! Folders the ZLO desktop app was given one at a time (through macOS's
//! open panel), opened in this backend process too.
//!
//! macOS ties that access to the process that holds it, and the app runs
//! `serve` as its child: the app hands over a bookmark per folder and this
//! process resolves each one and starts accessing it, for its lifetime.
//! The app's side is `tools/ironclaw_rune/app/src/folder_access.rs`.
//!
//! Hand-over: with `--folder-grants-stdin` (a flag only the app passes,
//! to the bundled backend, so nothing this process spawns inherits it),
//! stdin carries
//!
//! ```text
//! zlo-folder-grants 1 <count>
//! <hex bookmark>          (count lines)
//! end
//! ```
//!
//! then EOF. It is read to the end, all or nothing (bounded in size and
//! count, any malformed line rejects the lot), and stdin is then pointed at
//! `/dev/null`, so no descendant can read leftover bytes. Bookmarks are
//! capabilities: never in argv or env, never logged.

use std::io::Read;

const HEADER: &str = "zlo-folder-grants 1";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_GRANTS: usize = 256;

/// Read and open the app's folder grants. Never fails the boot: a folder
/// that does not open is left to macOS's own prompt.
pub(crate) fn open_from_app() {
    let mut raw = Vec::new();
    // One byte past the bound tells an oversized hand-over from a full one.
    let read = std::io::stdin().take(MAX_BYTES + 1).read_to_end(&mut raw);
    detach_stdin();
    if let Err(error) = read {
        tracing::debug!(%error, "folder grants unreadable");
        return;
    }
    if raw.len() as u64 > MAX_BYTES {
        tracing::debug!("folder grants over the size bound; none opened");
        return;
    }
    let bookmarks = match std::str::from_utf8(&raw)
        .map_err(|_| Reject::NotText)
        .and_then(parse)
    {
        Ok(bookmarks) => bookmarks,
        Err(reason) => {
            tracing::debug!(?reason, "folder grants rejected; none opened");
            return;
        }
    };
    let opened = bookmarks.iter().filter(|b| mac::open(b)).count();
    tracing::debug!(sent = bookmarks.len(), opened, "folder grants from the app");
}

/// Point fd 0 at `/dev/null` once the hand-over is read.
fn detach_stdin() {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        match std::fs::File::open("/dev/null") {
            // SAFETY: dup2 onto fd 0 with a valid, open descriptor.
            Ok(null) => {
                if unsafe { libc::dup2(null.as_raw_fd(), 0) } < 0 {
                    tracing::debug!(
                        error = %std::io::Error::last_os_error(),
                        "stdin not detached after folder grants"
                    );
                }
            }
            Err(error) => tracing::debug!(%error, "no /dev/null for stdin"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reject {
    NotText,
    Header,
    Count,
    Line(usize),
    End,
}

/// The bookmarks of a hand-over, or why it was rejected.
fn parse(raw: &str) -> Result<Vec<Vec<u8>>, Reject> {
    let mut lines = raw.lines();
    let count = lines
        .next()
        .and_then(|h| h.strip_prefix(HEADER))
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or(Reject::Header)?
        .parse::<usize>()
        .map_err(|_| Reject::Count)?;
    if count > MAX_GRANTS {
        return Err(Reject::Count);
    }
    let mut out = Vec::with_capacity(count);
    for ix in 0..count {
        let bytes = lines
            .next()
            .and_then(|l| hex::decode(l).ok())
            .filter(|b| !b.is_empty())
            .ok_or(Reject::Line(ix))?;
        out.push(bytes);
    }
    if lines.next() != Some("end") || lines.next().is_some() {
        return Err(Reject::End);
    }
    Ok(out)
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
    /// `…WithoutMountingMask` | `…WithoutImplicitStartAccessing` (access is
    /// started explicitly below, once). No security-scope flag: the app
    /// sends implicitly scoped bookmarks, made from URLs it is accessing,
    /// which is how macOS passes access to another process.
    const RESOLVE: CFOptionFlags = (1 << 8) | (1 << 9) | (1 << 15);

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
    fn a_hand_over_is_taken_whole_or_not_at_all() {
        assert_eq!(
            parse("zlo-folder-grants 1 2\n0102\n0304\nend\n"),
            Ok(vec![vec![1, 2], vec![3, 4]])
        );
        assert_eq!(parse("zlo-folder-grants 1 0\nend\n"), Ok(vec![]));
        assert_eq!(parse("0102\n"), Err(Reject::Header));
        assert_eq!(
            parse("zlo-folder-grants 2 1\n01\nend\n"),
            Err(Reject::Header)
        );
        assert_eq!(parse("zlo-folder-grants 1 x\nend\n"), Err(Reject::Count));
        assert_eq!(
            parse(&format!("zlo-folder-grants 1 {}\n", MAX_GRANTS + 1)),
            Err(Reject::Count)
        );
        // A bad or missing line, a cut-off end, or trailing bytes.
        assert_eq!(
            parse("zlo-folder-grants 1 2\n01\nzz\nend\n"),
            Err(Reject::Line(1))
        );
        assert_eq!(parse("zlo-folder-grants 1 2\n01\n"), Err(Reject::Line(1)));
        assert_eq!(parse("zlo-folder-grants 1 1\n01\n"), Err(Reject::End));
        assert_eq!(
            parse("zlo-folder-grants 1 1\n01\nend\n02\n"),
            Err(Reject::End)
        );
    }
}
