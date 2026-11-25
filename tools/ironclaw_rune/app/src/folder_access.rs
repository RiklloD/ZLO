//! Folders on this Mac that ZLO may use, one at a time instead of macOS's
//! whole-folder switches.
//!
//! When the backend (ZLO's child, so macOS charges it to ZLO) first touches
//! Desktop, Documents, Downloads, iCloud Drive or another volume, macOS asks
//! once for the *whole* folder. That prompt cannot be narrowed. What macOS
//! does offer is the open panel (the "Powerbox"): a folder the person picks
//! there is readable and writable without the whole-folder switch. ZLO
//! keeps each pick as a security-scoped bookmark and opens it again at
//! launch, before it starts the backend. macOS ties that access to the
//! process holding it, so each start of the bundled backend also gets the
//! folders handed over ([`handoff_for_backend_spawn`], written to its stdin
//! by `service.rs`; opened there by `ironclaw_cli`'s `folder_grants.rs`).
//!
//! The list is saved next to the appearance prefs as `folder-access.json`
//! (path plus bookmark, base64). A folder that does not open at launch (a
//! drive not plugged in) stays in the list and is tried again next time.
//!
//! Forgetting a folder only drops ZLO's bookmark: macOS may still let ZLO in
//! (a whole-folder switch turned on, or consent it keeps on its own), so
//! Settings says "Forget", never "revoke".
//!
//! A backend already running keeps the folders it started with: a change
//! reaches it only when it restarts. Nothing here restarts it (that would
//! stop running threads); [`pending`] tells Settings to offer Restart
//! IronClaw, and otherwise the change applies at the next start.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// One folder the person granted.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    path: PathBuf,
    /// `NSURL` security-scoped bookmark data, base64.
    bookmark: String,
}

/// A granted folder while the app runs.
struct Grant {
    path: PathBuf,
    bookmark: String,
    /// The resolved `NSURL` (retained, accessing), as an address; `None`
    /// while the folder does not open (kept, tried again at next launch).
    url: Option<usize>,
}

static GRANTS: Mutex<Vec<Grant>> = Mutex::new(Vec::new());

/// Bumped by every saved change to the list.
static REVISION: AtomicU64 = AtomicU64::new(0);
/// The revision the newest backend was started with
/// ([`handoff_for_backend_spawn`]).
static APPLIED: AtomicU64 = AtomicU64::new(0);

fn grants() -> std::sync::MutexGuard<'static, Vec<Grant>> {
    GRANTS.lock().unwrap_or_else(|e| e.into_inner())
}

fn file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ZLO").join("folder-access.json"))
}

fn load() -> Vec<Saved> {
    file()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(list: &[Saved]) -> Result<(), String> {
    let file = file().ok_or("ZLO has no settings folder to save the list in")?;
    let bytes = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
    crate::persist::atomic_write(&file, &bytes)
        .map_err(|e| format!("could not save the folder list: {e}"))
}

fn saved(list: &[Grant]) -> Vec<Saved> {
    list.iter()
        .map(|g| Saved {
            path: g.path.clone(),
            bookmark: g.bookmark.clone(),
        })
        .collect()
}

/// One spelling per folder (symlinks and `/private` resolved), so paths
/// compare as the folders they name.
fn normal(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Open every saved folder again. Call once at launch, before the backend
/// starts. A folder that does not open stays in the list for next time.
pub fn restore() {
    let mut opened = Vec::new();
    let mut refreshed = false;
    // Foundation runs outside the lock.
    for s in load() {
        match mac::resolve(&s.bookmark) {
            Some((url, path, fresh)) => {
                refreshed |= fresh.is_some();
                opened.push(Grant {
                    path: normal(&path),
                    bookmark: fresh.unwrap_or(s.bookmark),
                    url: Some(url),
                });
            }
            None => {
                tracing::debug!(path = %s.path.display(), "granted folder does not open now");
                opened.push(Grant {
                    path: s.path,
                    bookmark: s.bookmark,
                    url: None,
                });
            }
        }
    }
    if refreshed && let Err(e) = save(&saved(&opened)) {
        tracing::debug!(error = %e, "refreshed bookmarks not saved; the old ones still work");
    }
    *grants() = opened;
}

/// The service is about to start a backend: it gets the list as it is now.
/// Returns the stdin hand-over for it (`folder_grants.rs`'s framing), or
/// `None` when there is nothing to hand over. Implicitly scoped bookmarks,
/// made from the URLs this process is accessing, are how macOS passes that
/// access to another process. Never logged: they are capabilities.
pub fn handoff_for_backend_spawn() -> Option<String> {
    APPLIED.store(REVISION.load(Ordering::SeqCst), Ordering::SeqCst);
    let lines: Vec<String> = grants()
        .iter()
        .filter_map(|g| g.url.and_then(mac::implicit_bookmark))
        .map(|data| data.iter().map(|b| format!("{b:02x}")).collect())
        .collect();
    framed(&lines)
}

/// The hand-over text for `lines` (hex bookmarks); `None` when empty.
fn framed(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut out = format!("zlo-folder-grants 1 {}\n", lines.len());
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str("end\n");
    Some(out)
}

/// Whether the running backend started before the last change to the list,
/// so only a restart gives it the change.
pub fn pending() -> bool {
    misses_change(
        REVISION.load(Ordering::SeqCst),
        APPLIED.load(Ordering::SeqCst),
        crate::service::backend_pid().is_some(),
    )
}

fn misses_change(revision: u64, applied: u64, running: bool) -> bool {
    running && revision != applied
}

/// The granted folders, for Settings, with whether each opened.
pub fn list() -> Vec<(PathBuf, bool)> {
    grants()
        .iter()
        .map(|g| (g.path.clone(), g.url.is_some()))
        .collect()
}

/// Keep a folder the person just picked in the open panel. `Ok(true)` when
/// it is newly covered (a running backend gets it at its next start, see
/// [`pending`]), `Ok(false)` when a granted folder already holds it.
pub fn add(path: &Path) -> Result<bool, String> {
    let path = normal(path);
    // A folder that did not open at launch is picked again here: it is
    // replaced below rather than counted as covering.
    let opened = grants()
        .iter()
        .filter(|g| g.url.is_some())
        .map(|g| g.path.clone())
        .collect::<Vec<_>>();
    if covered(opened.iter().map(PathBuf::as_path), &path) {
        return Ok(false);
    }
    // The open panel gave this process access just now; a bookmark made
    // while it lasts carries that access to later launches.
    let bookmark = mac::bookmark(&path).ok_or("macOS would not remember this folder")?;
    let (url, resolved, _) = mac::resolve(&bookmark).ok_or("macOS would not open this folder")?;
    let resolved = normal(&resolved);
    let mut list = grants();
    // A new folder that holds older ones replaces them.
    let (inside, mut kept): (Vec<Grant>, Vec<Grant>) =
        list.drain(..).partition(|g| g.path.starts_with(&resolved));
    kept.push(Grant {
        path: resolved,
        bookmark,
        url: Some(url),
    });
    if let Err(e) = save(&saved(&kept)) {
        // Nothing changes: the old list stays, the new URL is let go.
        kept.pop();
        kept.extend(inside);
        *list = kept;
        drop(list);
        mac::release(url);
        return Err(e);
    }
    *list = kept;
    drop(list);
    for g in inside {
        if let Some(u) = g.url {
            mac::release(u);
        }
    }
    REVISION.fetch_add(1, Ordering::SeqCst);
    Ok(true)
}

/// Forget a granted folder: ZLO drops its bookmark (macOS may still let it
/// in, see the module note). `Ok(true)` when it was in the list.
pub fn forget(path: &Path) -> Result<bool, String> {
    let mut list = grants();
    let Some(ix) = list.iter().position(|g| g.path == path) else {
        return Ok(false);
    };
    let mut rest = saved(&list);
    rest.remove(ix);
    save(&rest)?;
    let gone = list.remove(ix);
    drop(list);
    if let Some(u) = gone.url {
        mac::release(u);
    }
    REVISION.fetch_add(1, Ordering::SeqCst);
    Ok(true)
}

/// A folder the open panel just handed over (a project folder, say): kept
/// when macOS guards where it lives and no granted folder holds it yet.
/// `true` when it was newly kept (see [`pending`]).
pub fn adopt_pick(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    if !guarded(&normal(path), &normal(&home)) {
        return false;
    }
    match add(path) {
        Ok(new) => new,
        Err(e) => {
            tracing::debug!(error = %e, path = %path.display(), "could not keep the picked folder");
            false
        }
    }
}

/// Whether macOS asks for a whole-folder switch before anything under
/// `path` can be read: Desktop, Documents, Downloads, iCloud Drive and
/// other volumes.
pub fn guarded(path: &Path, home: &Path) -> bool {
    const IN_HOME: [&str; 4] = [
        "Desktop",
        "Documents",
        "Downloads",
        "Library/Mobile Documents",
    ];
    IN_HOME.iter().any(|d| path.starts_with(home.join(d))) || path.starts_with("/Volumes")
}

/// Whether one of `granted` is `path` or holds it.
fn covered<'a>(mut granted: impl Iterator<Item = &'a Path>, path: &Path) -> bool {
    granted.any(|g| path.starts_with(g))
}

#[cfg(target_os = "macos")]
mod mac {
    //! `NSURL` security-scoped bookmarks. Every call runs inside its own
    //! autorelease pool; the one object kept (an accessing URL) is retained.

    use std::ffi::{CStr, CString, c_void};
    use std::path::{Path, PathBuf};

    use base64::Engine as _;
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject, Bool};

    /// `NSURLBookmarkCreationWithSecurityScope`.
    const CREATE_SCOPED: usize = 1 << 11;
    /// `NSURLBookmarkResolutionWithSecurityScope`, `…WithoutUI` and
    /// `…WithoutMounting`: no dialog or volume mount, at launch above all.
    const RESOLVE_SCOPED: usize = (1 << 10) | (1 << 8) | (1 << 9);

    fn class(name: &CStr) -> Option<&'static AnyClass> {
        AnyClass::get(name)
    }

    /// Bookmark data for `path`, base64; the process must have access now.
    pub fn bookmark(path: &Path) -> Option<String> {
        let c = CString::new(path.to_str()?).ok()?;
        autoreleasepool(|_| unsafe {
            let ns: *mut AnyObject =
                msg_send![class(c"NSString")?, stringWithUTF8String: c.as_ptr()];
            if ns.is_null() {
                return None;
            }
            let url: *mut AnyObject =
                msg_send![class(c"NSURL")?, fileURLWithPath: ns, isDirectory: Bool::YES];
            bookmark_of(url)
        })
    }

    /// Bookmark data for a file URL, base64. Call inside a pool.
    unsafe fn bookmark_of(url: *mut AnyObject) -> Option<String> {
        let raw = unsafe { bookmark_bytes(url, CREATE_SCOPED) }?;
        Some(base64::engine::general_purpose::STANDARD.encode(raw))
    }

    /// An implicitly scoped bookmark of a URL from [`resolve`] (accessing
    /// now): another process that resolves it gets the same access.
    pub fn implicit_bookmark(url: usize) -> Option<Vec<u8>> {
        autoreleasepool(|_| unsafe { bookmark_bytes(url as *mut AnyObject, 0) })
    }

    /// Bookmark bytes for a file URL with `options`. Call inside a pool.
    unsafe fn bookmark_bytes(url: *mut AnyObject, options: usize) -> Option<Vec<u8>> {
        if url.is_null() {
            return None;
        }
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let data: *mut AnyObject = unsafe {
            msg_send![
                url,
                bookmarkDataWithOptions: options,
                includingResourceValuesForKeys: std::ptr::null_mut::<AnyObject>(),
                relativeToURL: std::ptr::null_mut::<AnyObject>(),
                error: &mut error
            ]
        };
        if data.is_null() {
            return None;
        }
        let bytes: *const c_void = unsafe { msg_send![data, bytes] };
        let len: usize = unsafe { msg_send![data, length] };
        if bytes.is_null() || len == 0 {
            return None;
        }
        // SAFETY: NSData owns `len` bytes at `bytes` while the pool lives.
        let slice = unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), len) };
        Some(slice.to_vec())
    }

    /// Resolve bookmark data and start accessing it: the retained URL (as an
    /// address), its path, and fresh bookmark data (made from the resolved
    /// URL) when macOS called the old one stale and remaking it worked.
    pub fn resolve(saved: &str) -> Option<(usize, PathBuf, Option<String>)> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(saved)
            .ok()?;
        autoreleasepool(|_| unsafe {
            let data: *mut AnyObject = msg_send![
                class(c"NSData")?,
                dataWithBytes: raw.as_ptr().cast::<c_void>(),
                length: raw.len()
            ];
            let mut stale = Bool::NO;
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let url: *mut AnyObject = msg_send![
                class(c"NSURL")?,
                URLByResolvingBookmarkData: data,
                options: RESOLVE_SCOPED,
                relativeToURL: std::ptr::null_mut::<AnyObject>(),
                bookmarkDataIsStale: &mut stale,
                error: &mut error
            ];
            if url.is_null() {
                return None;
            }
            let started: Bool = msg_send![url, startAccessingSecurityScopedResource];
            if !started.as_bool() {
                return None;
            }
            let ns: *mut AnyObject = msg_send![url, path];
            let utf8: *const std::ffi::c_char = msg_send![ns, UTF8String];
            if utf8.is_null() {
                let _: () = msg_send![url, stopAccessingSecurityScopedResource];
                return None;
            }
            let path = PathBuf::from(CStr::from_ptr(utf8).to_string_lossy().into_owned());
            // Stale data still resolved: remake it while access lasts.
            let fresh = if stale.as_bool() {
                bookmark_of(url)
            } else {
                None
            };
            let _: *mut AnyObject = msg_send![url, retain];
            Some((url as usize, path, fresh))
        })
    }

    /// Stop accessing a URL from [`resolve`] and release it.
    pub fn release(url: usize) {
        let url = url as *mut AnyObject;
        if url.is_null() {
            return;
        }
        unsafe {
            let _: () = msg_send![url, stopAccessingSecurityScopedResource];
            let _: () = msg_send![url, release];
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    use std::path::{Path, PathBuf};

    pub fn bookmark(_: &Path) -> Option<String> {
        None
    }
    pub fn resolve(_: &str) -> Option<(usize, PathBuf, Option<String>)> {
        None
    }
    pub fn release(_: usize) {}
    pub fn implicit_bookmark(_: usize) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_folders_macos_guards_need_a_grant() {
        let home = Path::new("/Users/me");
        assert!(guarded(Path::new("/Users/me/Documents/site"), home));
        assert!(guarded(Path::new("/Users/me/Desktop"), home));
        assert!(guarded(
            Path::new("/Users/me/Library/Mobile Documents/com~apple~CloudDocs/a"),
            home
        ));
        assert!(guarded(Path::new("/Volumes/USB/x"), home));
        assert!(!guarded(Path::new("/Users/me/code/site"), home));
        assert!(!guarded(Path::new("/Users/me/Documentsy"), home));
    }

    #[test]
    fn only_a_backend_started_before_a_change_misses_it() {
        assert!(misses_change(3, 2, true));
        // Started with this list, or none runs: nothing to restart.
        assert!(!misses_change(3, 3, true));
        assert!(!misses_change(3, 2, false));
    }

    #[test]
    fn the_hand_over_is_framed_as_the_backend_reads_it() {
        assert_eq!(framed(&[]), None);
        assert_eq!(
            framed(&["0102".into(), "03".into()]).as_deref(),
            Some("zlo-folder-grants 1 2\n0102\n03\nend\n")
        );
    }

    #[test]
    fn a_granted_folder_covers_what_it_holds_only() {
        let granted = [Path::new("/Users/me/Documents/site")];
        assert!(covered(
            granted.into_iter(),
            Path::new("/Users/me/Documents/site/src")
        ));
        assert!(covered(
            granted.into_iter(),
            Path::new("/Users/me/Documents/site")
        ));
        assert!(!covered(
            granted.into_iter(),
            Path::new("/Users/me/Documents/other")
        ));
        assert!(!covered(
            granted.into_iter(),
            Path::new("/Users/me/Documents")
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_bookmark_opens_the_same_folder_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let data = mac::bookmark(&path).expect("bookmark");
        let (url, resolved, fresh) = mac::resolve(&data).expect("resolve");
        assert_eq!(resolved.canonicalize().unwrap(), path);
        assert!(fresh.is_none(), "a new bookmark is not stale");
        assert!(mac::implicit_bookmark(url).is_some_and(|b| !b.is_empty()));
        mac::release(url);
    }
}
