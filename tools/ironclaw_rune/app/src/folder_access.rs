//! Folders on this Mac that ZLO may use, one at a time instead of macOS's
//! whole-folder switches.
//!
//! When the backend (ZLO's child, so macOS charges it to ZLO) first touches
//! Desktop, Documents, Downloads, iCloud Drive or another volume, macOS asks
//! once for the *whole* folder. That prompt cannot be narrowed. What macOS
//! does offer is the open panel (the "Powerbox"): a folder the person picks
//! there is readable and writable without the whole-folder switch. ZLO
//! keeps each pick as a security-scoped bookmark and opens it again at
//! launch, before it starts the backend, so the backend inherits exactly
//! those folders.
//!
//! The list is saved next to the appearance prefs as `folder-access.json`
//! (path plus bookmark, base64).
//!
//! A backend already running keeps the folders it started with: a change
//! reaches it only when it restarts. Nothing here restarts it (that would
//! stop running threads); [`pending`] tells Settings to offer Restart
//! IronClaw, and otherwise the change applies at the next start.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// The resolved `NSURL` (retained, accessing), as an address.
    url: usize,
}

static GRANTS: Mutex<Vec<Grant>> = Mutex::new(Vec::new());

/// The pid of the backend that was running when the list last changed: it
/// does not have the change. Any later backend (a restart, a new launch)
/// does.
static CHANGED_UNDER: Mutex<Option<u32>> = Mutex::new(None);

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

fn save(list: &[Grant]) {
    let Some(file) = file() else { return };
    let saved: Vec<Saved> = list
        .iter()
        .map(|g| Saved {
            path: g.path.clone(),
            bookmark: g.bookmark.clone(),
        })
        .collect();
    let result = serde_json::to_vec_pretty(&saved)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&file, bytes)
        });
    if let Err(e) = result {
        tracing::warn!(error = %e, "could not save the folder list");
    }
}

/// Open every saved folder again. Call once at launch, before the backend
/// starts, so its process inherits them. A folder that is gone or whose
/// bookmark no longer resolves is dropped from the list.
pub fn restore() {
    let saved = load();
    let mut list = grants();
    let before = saved.len();
    let mut refreshed = false;
    for s in saved {
        match mac::resolve(&s.bookmark) {
            Some((url, path, fresh)) => {
                refreshed |= fresh.is_some();
                list.push(Grant {
                    path,
                    bookmark: fresh.unwrap_or(s.bookmark),
                    url,
                });
            }
            None => tracing::debug!(path = %s.path.display(), "granted folder no longer opens"),
        }
    }
    if refreshed || list.len() != before {
        save(&list);
    }
}

/// The list changed: the backend running now (if any) misses it until it
/// restarts.
fn mark_changed() {
    let running = crate::service::backend_pid();
    *CHANGED_UNDER.lock().unwrap_or_else(|e| e.into_inner()) = running;
}

/// Whether the running backend started before the last change to the list,
/// so only a restart gives it the change.
pub fn pending() -> bool {
    let changed_under = *CHANGED_UNDER.lock().unwrap_or_else(|e| e.into_inner());
    misses_change(changed_under, crate::service::backend_pid())
}

fn misses_change(changed_under: Option<u32>, running: Option<u32>) -> bool {
    changed_under.is_some() && changed_under == running
}

/// The granted folders, for Settings.
pub fn list() -> Vec<PathBuf> {
    grants().iter().map(|g| g.path.clone()).collect()
}

/// Keep a folder the person just picked in the open panel. `Ok(true)` when
/// it is newly covered (a running backend gets it at its next start, see
/// [`pending`]), `Ok(false)` when a granted folder already holds it.
pub fn add(path: &Path) -> Result<bool, String> {
    let mut list = grants();
    if covered(list.iter().map(|g| g.path.as_path()), path) {
        return Ok(false);
    }
    // The open panel gave this process access just now; a bookmark made
    // while it lasts carries that access to later launches.
    let bookmark = mac::bookmark(path).ok_or("macOS would not remember this folder")?;
    let (url, resolved, _) = mac::resolve(&bookmark).ok_or("macOS would not open this folder")?;
    // A new folder that holds older ones replaces them.
    list.retain(|g| {
        let inside = g.path.starts_with(&resolved);
        if inside {
            mac::release(g.url);
        }
        !inside
    });
    list.push(Grant {
        path: resolved,
        bookmark,
        url,
    });
    save(&list);
    mark_changed();
    Ok(true)
}

/// Forget a granted folder. `true` when it was in the list.
pub fn remove(path: &Path) -> bool {
    let mut list = grants();
    let Some(ix) = list.iter().position(|g| g.path == path) else {
        return false;
    };
    let gone = list.remove(ix);
    mac::release(gone.url);
    save(&list);
    mark_changed();
    true
}

/// A folder the open panel just handed over (a project folder, say): kept
/// when macOS guards where it lives and no granted folder holds it yet.
/// `true` when it was newly kept (see [`pending`]).
pub fn adopt_pick(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    if !guarded(path, &home) {
        return false;
    }
    match add(path) {
        Ok(new) => new,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not keep the picked folder");
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
    /// `NSURLBookmarkResolutionWithSecurityScope`.
    const RESOLVE_SCOPED: usize = 1 << 10;

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
            if url.is_null() {
                return None;
            }
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let data: *mut AnyObject = msg_send![
                url,
                bookmarkDataWithOptions: CREATE_SCOPED,
                includingResourceValuesForKeys: std::ptr::null_mut::<AnyObject>(),
                relativeToURL: std::ptr::null_mut::<AnyObject>(),
                error: &mut error
            ];
            if data.is_null() {
                return None;
            }
            let bytes: *const c_void = msg_send![data, bytes];
            let len: usize = msg_send![data, length];
            let slice = std::slice::from_raw_parts(bytes.cast::<u8>(), len);
            Some(base64::engine::general_purpose::STANDARD.encode(slice))
        })
    }

    /// Resolve bookmark data and start accessing it: the retained URL (as an
    /// address), its path, and fresh bookmark data when macOS called the
    /// old one stale.
    pub fn resolve(saved: &str) -> Option<(usize, PathBuf, Option<String>)> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(saved)
            .ok()?;
        let (url, path, stale) = autoreleasepool(|_| unsafe {
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
            let _: *mut AnyObject = msg_send![url, retain];
            Some((url as usize, path, stale.as_bool()))
        })?;
        // Stale data still resolved: remake it while access lasts.
        let fresh = if stale { bookmark(&path) } else { None };
        Some((url, path, fresh))
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
    fn only_the_backend_from_before_a_change_misses_it() {
        assert!(misses_change(Some(7), Some(7)));
        // Restarted since, or none was running: the new one has the list.
        assert!(!misses_change(Some(7), Some(8)));
        assert!(!misses_change(Some(7), None));
        assert!(!misses_change(None, Some(7)));
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
        mac::release(url);
    }
}
