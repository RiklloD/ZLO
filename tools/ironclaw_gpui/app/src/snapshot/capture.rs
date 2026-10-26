//! One SnapShot: the front window of another app, its picture, its app
//! icon, and (when asked) its accessibility text. Blocking work; the caller
//! runs it on a background thread.
//!
//! The picture comes from `/usr/sbin/screencapture -l <window id>`, as the
//! computer-use live view does. On current macOS that tool is built on
//! ScreenCaptureKit, reads exactly one window (so Numoh's own windows are
//! never in it, even when they overlap), and is charged to Numoh's Screen
//! Recording grant because Numoh launches it. Driving SCScreenshotManager
//! directly would need Objective-C blocks and a new binding crate for the
//! same pixels.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

use super::cf::{self, CfRef, Owned};

/// Wider pictures are scaled down: plenty for a model to read UI text.
const MAX_WIDTH: u32 = 1600;
const JPEG_QUALITY: u8 = 82;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(6);

/// The window a SnapShot is taken of.
#[derive(Clone, Debug)]
pub(super) struct Target {
    pub window_id: u32,
    pub pid: i32,
    pub app: String,
    pub title: String,
    /// x, y, width, height in screen points, origin top-left.
    pub bounds: (f64, f64, f64, f64),
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CfRef;
}

unsafe extern "C" {
    // libproc, part of libSystem.
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
}

const ON_SCREEN_ONLY: u32 = 1;
const EXCLUDE_DESKTOP_ELEMENTS: u32 = 16;

/// The frontmost normal window that is not Numoh's: CoreGraphics lists
/// on-screen windows front to back, so the first layer-0 window of another
/// process is the one the user is looking at.
pub(super) fn front_window() -> Result<Target, String> {
    let own = std::process::id() as i32;
    // SAFETY: a copy (create rule), released by `Owned`; null is handled.
    let list = Owned::new(unsafe {
        CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, 0)
    })
    .ok_or("macOS returned no window list")?;
    // SAFETY: `list` is a live CFArray of CFDictionaries until dropped.
    let count = unsafe { cf::CFArrayGetCount(list.0) };
    for index in 0..count {
        // SAFETY: index < count.
        let dict = unsafe { cf::CFArrayGetValueAtIndex(list.0, index) };
        // SAFETY: plain type id query.
        if !cf::type_is(dict, unsafe { cf::CFDictionaryGetTypeID() }) {
            continue;
        }
        let num = |key: &str| cf::number(cf::dict_get(dict, key));
        let (Some(window_id), Some(pid)) = (num("kCGWindowNumber"), num("kCGWindowOwnerPID"))
        else {
            continue;
        };
        let pid = pid as i32;
        if pid == own || num("kCGWindowLayer").unwrap_or(0.) != 0. {
            continue;
        }
        if num("kCGWindowAlpha").is_some_and(|a| a <= 0.) {
            continue;
        }
        let bounds = cf::dict_get(dict, "kCGWindowBounds");
        // SAFETY: plain type id query.
        if !cf::type_is(bounds, unsafe { cf::CFDictionaryGetTypeID() }) {
            continue;
        }
        let side = |key: &str| cf::number(cf::dict_get(bounds, key));
        let (Some(x), Some(y), Some(w), Some(h)) =
            (side("X"), side("Y"), side("Width"), side("Height"))
        else {
            continue;
        };
        if w < 60. || h < 40. {
            continue;
        }
        return Ok(Target {
            window_id: window_id as u32,
            pid,
            app: cf::string(cf::dict_get(dict, "kCGWindowOwnerName")).unwrap_or_default(),
            // Empty without Screen Recording; the AX walk fills it in then.
            title: cf::string(cf::dict_get(dict, "kCGWindowName")).unwrap_or_default(),
            bounds: (x, y, w, h),
        });
    }
    Err("No window of another app is on screen.".into())
}

/// The window as a JPEG (bytes, width, height).
pub(super) fn picture(window_id: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let path = scratch(&format!("shot-{window_id}.png"))?;
    let status = run_with_timeout(
        Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-t", "png"])
            .arg(format!("-l{window_id}"))
            .arg(&path),
        CAPTURE_TIMEOUT,
    );
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path); // silent-ok: the file may not exist when the capture failed
    status?;
    let bytes = bytes.map_err(|e| format!("The window capture produced no image ({e})."))?;
    let image = image::load_from_memory(&bytes)
        .map_err(|e| format!("The window capture is unreadable ({e})."))?;
    if looks_blank(&image) {
        return Err(
            "The capture came back blank. Allow Numoh under System Settings › \
                    Privacy & Security › Screen Recording, then try again."
                .into(),
        );
    }
    let image = if image.width() > MAX_WIDTH {
        let height = (u64::from(image.height()) * u64::from(MAX_WIDTH)
            / u64::from(image.width().max(1)))
        .max(1);
        image.resize_exact(MAX_WIDTH, height as u32, FilterType::Triangle)
    } else {
        image
    };
    let rgb = image.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| format!("Could not encode the capture ({e})."))?;
    Ok((out, rgb.width(), rgb.height()))
}

/// The app's icon as a small PNG in a cache folder, best effort: the chip
/// falls back to a plain icon without it.
pub(super) fn app_icon(pid: i32, app: &str) -> Option<PathBuf> {
    let bundle = bundle_of(pid)?;
    let slug: String = app
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(40)
        .collect();
    let out = scratch(&format!(
        "icon-{}.png",
        if slug.is_empty() {
            "app"
        } else {
            slug.as_str()
        }
    ))
    .ok()?;
    if out.exists() {
        return Some(out);
    }
    let plist = bundle.join("Contents/Info.plist");
    let name = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIconFile", "raw", "-o", "-"])
        .arg(&plist)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())?;
    let file = if name.ends_with(".icns") {
        name
    } else {
        format!("{name}.icns")
    };
    let icns = bundle.join("Contents/Resources").join(file);
    run_with_timeout(
        Command::new("/usr/bin/sips")
            .args(["-s", "format", "png", "-Z", "64"])
            .arg(&icns)
            .arg("--out")
            .arg(&out),
        Duration::from_secs(3),
    )
    .ok()?;
    out.exists().then_some(out)
}

/// The `.app` folder the process runs from.
fn bundle_of(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer and its length match; the result is the byte count.
    let len = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let len = usize::try_from(len).ok().filter(|l| *l > 0)?;
    buffer.truncate(len);
    let exe = PathBuf::from(String::from_utf8_lossy(&buffer).into_owned());
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// A file under Numoh's own temp folder.
fn scratch(name: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("numoh-snapshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("No temp folder for the capture ({e})."))?;
    Ok(dir.join(name))
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<(), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not start the capture ({e})."))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "The window capture failed ({status}). The window may have closed or moved to another Space."
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill(); // silent-ok: it may have exited just now
                let _ = child.wait(); // silent-ok: reaps the killed child
                return Err("The window capture timed out.".into());
            }
            Err(e) => return Err(format!("The window capture failed ({e}).")),
        }
    }
}

/// A capture without Screen Recording permission is one flat colour.
fn looks_blank(image: &image::DynamicImage) -> bool {
    let small = image.resize_exact(16, 16, FilterType::Nearest).to_luma8();
    let (lo, hi) = small.pixels().fold((u8::MAX, u8::MIN), |(lo, hi), p| {
        (lo.min(p[0]), hi.max(p[0]))
    });
    hi.saturating_sub(lo) < 3
}
