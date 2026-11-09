//! SnapShots, after T3 Code's: press both Shift keys (or the fallback combo
//! from Settings) and ZLO captures the frontmost window of another app.
//! The picture and a text part (app name, window title and the window's
//! accessibility tree) go into the current chat draft as one chip, so the
//! agent can read the screen, not only look at it. Off by default.
//!
//! The pieces: `hotkey` watches both Shift keys (the fallback combo is a
//! Rune global shortcut), `capture` finds the window and takes its picture,
//! `ax` walks its accessibility tree, and this file turns the result into
//! two staged files that the composer shows as one chip. Client-only: the
//! gateway sees ordinary attachments.
//!
//! Rune port of the GPUI `snapshot/mod.rs`: the `SnapshotPrefs`, `Chips`
//! and `Service` globals became [`SnapshotState`] in `State::snapshot`.

#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod cf;
#[cfg(target_os = "macos")]
mod hotkey;

use std::collections::HashMap;
use std::path::PathBuf;

use rune::{El, Tx, el, icon as glyph};
use serde::{Deserialize, Serialize};
use zlo_client::attachments::{self, StagedFile};
use zlo_client::wire::AttachmentLimits;

use crate::state::State;

/// The fallback combos Settings offers, as `hotkey` strings.
pub const FALLBACKS: [(&str, &str); 4] = [
    ("none", "Off"),
    ("ctrl-alt-s", "⌃⌥S"),
    ("cmd-shift-2", "⇧⌘2"),
    ("ctrl-alt-cmd-s", "⌃⌥⌘S"),
];

const IMAGE_SUFFIX: &str = ".snapshot.jpg";
const TEXT_SUFFIX: &str = ".snapshot.txt";

/// SnapShot preferences, saved as `snapshots.json` next to `prefs.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotPrefs {
    pub enabled: bool,
    /// Add the window's accessibility text (needs Accessibility).
    pub include_text: bool,
    /// A `ctrl-alt-s` style combo next to both-Shift, or "none".
    pub fallback: String,
}

impl Default for SnapshotPrefs {
    fn default() -> Self {
        Self {
            enabled: false,
            include_text: true,
            fallback: "ctrl-alt-s".into(),
        }
    }
}

impl SnapshotPrefs {
    fn file() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("ZLO").join("snapshots.json"))
    }

    fn load() -> Self {
        Self::file()
            .and_then(|f| std::fs::read(f).ok()) // silent-ok: no file yet means the defaults
            .and_then(|b| serde_json::from_slice(&b).ok()) // silent-ok: an unreadable file falls back to the defaults
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(file) = Self::file() else { return };
        match serde_json::to_vec_pretty(self) {
            Ok(json) => crate::persist::global().write(file, json),
            Err(e) => tracing::warn!("could not encode SnapShot preferences: {e}"),
        }
    }
}

/// What a SnapShot chip shows, by image file name.
#[derive(Clone)]
struct Chip {
    app: String,
    title: String,
    icon: Option<rune::Pixels>,
}

/// The `State::snapshot` slot: preferences, the armed trigger and chips.
#[derive(Default)]
pub struct SnapshotState {
    pub prefs: SnapshotPrefs,
    chips: HashMap<String, Chip>,
    busy: bool,
    /// Why the fallback combo is not armed, for Settings.
    warning: Option<String>,
    #[cfg(target_os = "macos")]
    shift: Option<hotkey::ShiftWatcher>,
    combo: Option<rune::ShortcutId>,
    /// Bumped on every (re)arm: presses from an older trigger are dropped.
    generation: u64,
    started: bool,
}

/// Load the preferences and arm the trigger. Called once by the workspace
/// when it starts (area B: `workspace::start`).
pub fn start(s: &mut State) {
    s.snapshot.prefs = SnapshotPrefs::load();
    s.snapshot.started = true;
    if rune::is_headless() {
        return;
    }
    // Global shortcuts register once the app's host runs: queue it.
    s.rt.handle.update(|s, _| arm(s));
}

/// Nothing to poll: presses land through `AppHandle::update`.
pub fn poll(_s: &mut State, _tx: &mut Tx) -> bool {
    false
}

/// Edit the preferences, save them off the UI thread and re-arm the
/// trigger (GPUI `SnapshotPrefs::update`). Settings → SnapShots (area D)
/// calls this.
pub fn update_prefs(s: &mut State, edit: impl FnOnce(&mut SnapshotPrefs)) {
    edit(&mut s.snapshot.prefs);
    s.snapshot.prefs.save();
    if s.snapshot.started && !rune::is_headless() {
        arm(s);
    }
}

/// Why the fallback shortcut is not working, if it is not.
pub fn warning(s: &State) -> Option<String> {
    s.snapshot.warning.clone()
}

/// (Re)arm or disarm the trigger to match the preferences.
fn arm(s: &mut State) {
    let prefs = s.snapshot.prefs.clone();
    let handle = s.rt.handle.clone();
    let snap = &mut s.snapshot;
    // Drop the old trigger first so its combo is free to register again.
    snap.generation += 1;
    snap.warning = None;
    if let Some(id) = snap.combo.take()
        && let Err(e) = handle.unregister_shortcut(id)
    {
        tracing::debug!(error = %e, "SnapShot shortcut was already gone");
    }
    #[cfg(target_os = "macos")]
    {
        snap.shift = None;
        if !prefs.enabled {
            return;
        }
        let generation = snap.generation;
        let shift_handle = handle.clone();
        match hotkey::ShiftWatcher::new(move || {
            shift_handle.update(move |s, _| {
                if s.snapshot.generation == generation {
                    take(s);
                }
            });
        }) {
            Ok(watcher) => snap.shift = Some(watcher),
            Err(e) => snap.warning = Some(e),
        }
        let combo = prefs.fallback.trim();
        if !matches!(combo, "" | "none") {
            let registered = hotkey::shortcut(combo).and_then(|shortcut| {
                handle
                    .register_shortcut(shortcut.clone(), move |s: &mut State, _: &mut Tx| {
                        if s.snapshot.generation == generation {
                            take(s);
                        }
                    })
                    .map_err(|e| match e {
                        rune::ShortcutError::Conflict => match handle.shortcut_owner(&shortcut) {
                            Some(owner) => format!("{} is taken by {owner}.", hotkey::label(combo)),
                            None => format!("{} is taken by another app.", hotkey::label(combo)),
                        },
                        other => format!("The SnapShot shortcut could not be set up ({other})."),
                    })
            });
            match registered {
                Ok(id) => s.snapshot.combo = Some(id),
                Err(e) => s.snapshot.warning = Some(e),
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = prefs;
}

/// A finished capture, ready to stage.
pub struct Snapshot {
    app: String,
    title: String,
    bounds: (f64, f64, f64, f64),
    jpeg: Vec<u8>,
    size: (u32, u32),
    /// The rendered accessibility tree.
    tree: Option<String>,
    /// Why the tree is missing, when it was asked for.
    note: Option<String>,
    icon: Option<rune::Pixels>,
    at: chrono::DateTime<chrono::Local>,
}

/// Capture on a background thread, then bring ZLO forward and hand the
/// result to the composer it belongs in.
pub fn take(s: &mut State) {
    if s.snapshot.busy {
        return;
    }
    let include_text = s.snapshot.prefs.include_text;
    s.snapshot.busy = true;
    crate::jobs::blocking(
        s,
        move || capture_now(include_text),
        |s, result, _| {
            s.snapshot.busy = false;
            s.rt.handle.open();
            crate::workspace::stage_snapshot(s, result);
        },
    );
}

#[cfg(target_os = "macos")]
fn capture_now(include_text: bool) -> Result<Snapshot, String> {
    let target = capture::front_window()?;
    let pid = target.pid;
    // The tree walk and the picture run side by side.
    let (picture, tree) = std::thread::scope(|scope| {
        let tree = include_text.then(|| scope.spawn(move || ax::walk(pid)));
        let picture = capture::picture(target.window_id);
        let tree = tree.map(|t| {
            t.join()
                .unwrap_or_else(|_| Err("The accessibility walk failed.".into()))
        });
        (picture, tree)
    });
    let (jpeg, width, height) = picture?;
    let (mut title, mut text, mut note) = (target.title.clone(), None, None);
    match tree {
        Some(Ok(tree)) => {
            if title.is_empty() {
                title = tree.title.clone();
            }
            text = Some(ax::render(&tree.nodes, tree.partial, ax::TEXT_BUDGET));
        }
        Some(Err(e)) => note = Some(e),
        None => {}
    }
    Ok(Snapshot {
        icon: capture::app_icon(pid, &target.app).and_then(|path| icon_pixels(&path)),
        app: target.app,
        title,
        bounds: target.bounds,
        jpeg,
        size: (width, height),
        tree: text,
        note,
        at: chrono::Local::now(),
    })
}

#[cfg(not(target_os = "macos"))]
fn capture_now(_include_text: bool) -> Result<Snapshot, String> {
    Err("SnapShots need macOS.".into())
}

/// Decode the app icon PNG `capture` wrote, for the chip.
fn icon_pixels(path: &std::path::Path) -> Option<rune::Pixels> {
    let img = image::open(path)
        .map_err(|e| tracing::debug!(error = %e, "SnapShot app icon unreadable"))
        .ok()?
        .into_rgba8();
    let pixels = rune::Pixels::new();
    pixels.set(img.width(), img.height(), img.into_raw());
    Some(pixels)
}

impl Snapshot {
    fn stem(&self) -> String {
        let slug: String = self
            .app
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .take(32)
            .collect();
        let slug = slug.trim_matches('-');
        // A per-process sequence keeps two captures in one second apart.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!(
            "{}-{}-{seq}",
            if slug.is_empty() { "window" } else { slug },
            self.at.format("%Y%m%d-%H%M%S")
        )
    }

    /// The text part: what was captured, then the accessibility tree.
    fn text(&self, image_name: &str) -> String {
        let (x, y, w, h) = self.bounds;
        let mut out = format!(
            "SnapShot of a macOS window, taken {}.\n\
             App: {}\n\
             Window title: {}\n\
             Window frame: {x:.0},{y:.0} {w:.0}x{h:.0} (screen points, origin top-left)\n\
             Picture: {image_name} ({}x{} px)\n",
            self.at.format("%d.%m.%Y %H:%M"),
            self.app,
            if self.title.is_empty() {
                "(none)"
            } else {
                self.title.as_str()
            },
            self.size.0,
            self.size.1,
        );
        match (&self.tree, &self.note) {
            (Some(tree), _) => {
                out.push_str(
                    "\nAccessibility tree of the window, one element per line: \
                     Role \"label\" = \"value\" @x,y widthxheight (screen points), \
                     indented under its parent.\n\n",
                );
                out.push_str(tree);
            }
            (None, Some(note)) => out.push_str(&format!("\nApp text left out: {note}\n")),
            (None, None) => {}
        }
        out
    }
}

/// Stage `snap` as one picture and its text part, within the limits.
/// Both or neither: a picture without its text would read as a plain image.
/// Callers pass their own staged list, e.g.
/// `stage_into(&mut s.snapshot, &mut chat.staged, snap, &limits)`.
pub fn stage_into(
    state: &mut SnapshotState,
    staged: &mut Vec<StagedFile>,
    snap: Snapshot,
    limits: &AttachmentLimits,
) -> Result<(), String> {
    let stem = snap.stem();
    let image_name = format!("{stem}{IMAGE_SUFFIX}");
    let image_mime = attachments::mime_for(&image_name, limits)
        .or_else(|| attachments::mime_for("a.jpeg", limits))
        .ok_or("This gateway does not accept JPEG pictures.")?;
    let text_name = format!("{stem}{TEXT_SUFFIX}");
    let text_mime = attachments::mime_for(&text_name, limits)
        .or_else(|| attachments::mime_for("a.md", limits))
        .ok_or("This gateway accepts no text attachments.")?;
    let image = StagedFile::from_bytes(image_name.clone(), image_mime, snap.jpeg.clone());
    let text = StagedFile::from_bytes(text_name, text_mime, snap.text(&image_name).into_bytes());
    attachments::check(&image, staged, limits)?;
    staged.push(image);
    if let Err(e) = attachments::check(&text, staged, limits) {
        staged.pop();
        return Err(e);
    }
    staged.push(text);
    state.chips.insert(
        image_name,
        Chip {
            app: snap.app,
            title: snap.title,
            icon: snap.icon,
        },
    );
    Ok(())
}

/// The text half of a SnapShot staged with its picture: it has no chip of
/// its own. A `.snapshot.txt` picked by hand keeps its plain chip.
pub fn is_companion(file: &StagedFile, staged: &[StagedFile]) -> bool {
    file.filename.strip_suffix(TEXT_SUFFIX).is_some_and(|stem| {
        let image = format!("{stem}{IMAGE_SUFFIX}");
        staged.iter().any(|f| f.filename == image)
    })
}

/// Whether `text` is the text half of the SnapShot picture `image`.
pub fn pairs(text: &StagedFile, image: &StagedFile) -> bool {
    match (
        text.filename.strip_suffix(TEXT_SUFFIX),
        image.filename.strip_suffix(IMAGE_SUFFIX),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// A SnapShot picture's app name, for its card, and the spoken label
/// that adds the window title.
pub fn names(s: &State, file: &StagedFile) -> Option<(String, String)> {
    let info = s.snapshot.chips.get(&file.filename)?;
    let label = if info.title.is_empty() {
        format!("SnapShot of {}", info.app)
    } else {
        format!("SnapShot of {} · {}", info.app, info.title)
    };
    Some((info.app.clone(), label))
}

/// A SnapShot picture's app icon, small enough for an inline card.
pub fn icon(s: &State, file: &StagedFile) -> Option<El> {
    if !file.filename.ends_with(IMAGE_SUFFIX) {
        return None;
    }
    let info = s.snapshot.chips.get(&file.filename)?;
    Some(match &info.icon {
        Some(pixels) => el().size(14., 14.).pixels(pixels),
        None => glyph(&crate::icons::FRAME, 12.).color(s.palette.text_muted),
    })
}

/// Fixture chips for headless scenes (`scenes.rs`): a staged SnapShot pair
/// and its chip, as `stage_into` leaves them.
pub mod fixtures {
    use super::*;

    pub fn staged_pair(s: &mut State) -> Vec<StagedFile> {
        let snap = Snapshot {
            app: "Safari".into(),
            title: "Rune — README".into(),
            bounds: (120., 80., 1280., 800.),
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            size: (2560, 1600),
            tree: Some("Window \"Rune — README\" @120,80 1280x800\n".into()),
            note: None,
            icon: None,
            at: chrono::Local::now(),
        };
        let mut staged = Vec::new();
        let limits = AttachmentLimits {
            accept: ["image/jpeg", ".jpg", ".jpeg", "text/plain", ".txt"]
                .map(String::from)
                .to_vec(),
            ..AttachmentLimits::default()
        };
        if let Err(e) = stage_into(&mut s.snapshot, &mut staged, snap, &limits) {
            tracing::debug!(error = %e, "fixture SnapShot not staged");
        }
        staged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> StagedFile {
        StagedFile::from_bytes(name.into(), "text/plain".into(), Vec::new())
    }

    #[test]
    fn the_text_half_pairs_with_its_picture_only() {
        let image = file("Safari-1.snapshot.jpg");
        let text = file("Safari-1.snapshot.txt");
        let other = file("Notes-2.snapshot.txt");
        assert!(pairs(&text, &image));
        assert!(!pairs(&other, &image));
        let staged = vec![image.clone(), text.clone()];
        assert!(is_companion(&text, &staged));
        assert!(!is_companion(&other, &staged));
        assert!(!is_companion(&image, &staged));
    }
}
