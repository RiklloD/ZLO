//! SnapShots, after T3 Code's: press both Shift keys (or the fallback combo
//! from Settings) and Numoh captures the frontmost window of another app.
//! The picture and a text part (app name, window title and the window's
//! accessibility tree) go into the current chat draft as one chip, so the
//! agent can read the screen, not only look at it. Off by default.
//!
//! The pieces: `hotkey` arms the trigger, `capture` finds the window and
//! takes its picture, `ax` walks its accessibility tree, and this file turns
//! the result into two staged files that the composer shows as one chip.
//! Client-only: the gateway sees ordinary attachments.

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

use gpui_kit::AnyWindowHandle;
use ironclaw_gpui_client::attachments::{self, StagedFile};
use ironclaw_gpui_client::wire::AttachmentLimits;
use serde::{Deserialize, Serialize};

use crate::ui::*;
use crate::workspace::Workspace;

/// The fallback combos Settings offers, as `hotkey` strings.
pub(crate) const FALLBACKS: [(&str, &str); 4] = [
    ("none", "Off"),
    ("ctrl-alt-s", "⌃⌥S"),
    ("cmd-shift-2", "⇧⌘2"),
    ("ctrl-alt-cmd-s", "⌃⌥⌘S"),
];

const IMAGE_SUFFIX: &str = ".snapshot.jpg";
const TEXT_SUFFIX: &str = ".snapshot.txt";

/// SnapShot preferences, saved as `snapshots.json` next to `prefs.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SnapshotPrefs {
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

impl Global for SnapshotPrefs {}

impl SnapshotPrefs {
    fn file() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("Numoh").join("snapshots.json"))
    }

    fn load() -> Self {
        Self::file()
            .and_then(|f| std::fs::read(f).ok()) // silent-ok: no file yet means the defaults
            .and_then(|b| serde_json::from_slice(&b).ok()) // silent-ok: an unreadable file falls back to the defaults
            .unwrap_or_default()
    }

    pub fn of(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    /// Edit, save off the UI thread, re-arm the trigger and redraw.
    pub fn update(cx: &mut App, edit: impl FnOnce(&mut Self)) {
        let mut prefs = Self::of(cx);
        edit(&mut prefs);
        let snapshot = prefs.clone();
        cx.set_global(prefs);
        cx.background_spawn(async move {
            let Some(file) = Self::file() else { return };
            let saved = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    let json =
                        serde_json::to_vec_pretty(&snapshot).map_err(std::io::Error::other)?;
                    std::fs::write(&file, json)
                });
            if let Err(e) = saved {
                tracing::warn!("could not save SnapShot preferences: {e}");
            }
        })
        .detach();
        arm(cx);
        cx.refresh_windows();
    }
}

/// What a SnapShot chip shows, by image file name.
#[derive(Default)]
struct Chips(HashMap<String, Chip>);

impl Global for Chips {}

#[derive(Clone)]
struct Chip {
    app: String,
    title: String,
    icon: Option<PathBuf>,
}

/// The armed trigger and where captures go.
struct Service {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    #[cfg(target_os = "macos")]
    _trigger: Option<hotkey::Trigger>,
    _listen: Option<Task<()>>,
    busy: bool,
    /// Why the fallback combo is not armed, for Settings.
    warning: Option<String>,
}

impl Global for Service {}

/// Load the preferences and arm the trigger. Called once by the workspace,
/// after the quick composer's shortcut is registered (so this hotkey
/// handler runs first and passes on presses that are not its own).
pub(crate) fn start(workspace: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    cx.set_global(SnapshotPrefs::load());
    cx.set_global(Service {
        workspace,
        window,
        #[cfg(target_os = "macos")]
        _trigger: None,
        _listen: None,
        busy: false,
        warning: None,
    });
    arm(cx);
}

/// Why the fallback shortcut is not working, if it is not.
pub(crate) fn warning(cx: &App) -> Option<String> {
    cx.try_global::<Service>().and_then(|s| s.warning.clone())
}

/// (Re)arm or disarm the trigger to match the preferences.
fn arm(cx: &mut App) {
    if !cx.has_global::<Service>() {
        return;
    }
    let prefs = SnapshotPrefs::of(cx);
    let service = cx.global_mut::<Service>();
    // Drop the old trigger first so its combo is free to register again.
    service._listen = None;
    service.warning = None;
    #[cfg(target_os = "macos")]
    {
        service._trigger = None;
        if !prefs.enabled {
            return;
        }
        let (trigger, mut presses, warning) = hotkey::Trigger::new(&prefs.fallback);
        service._trigger = Some(trigger);
        service.warning = warning;
        let listen = cx.spawn(async move |cx| {
            use futures::StreamExt as _;
            while presses.next().await.is_some() {
                cx.update(take);
            }
        });
        cx.global_mut::<Service>()._listen = Some(listen);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = prefs;
}

/// A finished capture, ready to stage.
pub(crate) struct Snapshot {
    app: String,
    title: String,
    bounds: (f64, f64, f64, f64),
    jpeg: Vec<u8>,
    size: (u32, u32),
    /// The rendered accessibility tree.
    tree: Option<String>,
    /// Why the tree is missing, when it was asked for.
    note: Option<String>,
    icon: Option<PathBuf>,
    at: chrono::DateTime<chrono::Local>,
}

/// Capture on a background thread, then hand the result to the workspace.
fn take(cx: &mut App) {
    let Some(service) = cx.try_global::<Service>() else {
        return;
    };
    if service.busy {
        return;
    }
    let (workspace, window) = (service.workspace.clone(), service.window);
    let include_text = SnapshotPrefs::of(cx).include_text;
    cx.global_mut::<Service>().busy = true;
    let job = cx.background_spawn(async move { capture_now(include_text) });
    cx.spawn(async move |cx| {
        let result = job.await;
        cx.update(|cx| {
            cx.global_mut::<Service>().busy = false;
            cx.activate(true);
            let _ = window.update(cx, |_, window, cx| {
                window.activate_window();
                let _ = workspace.update(cx, |ws, cx| ws.stage_snapshot(result, window, cx));
            });
        });
    })
    .detach();
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
        icon: capture::app_icon(pid, &target.app),
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
pub(crate) fn stage_into(
    staged: &mut Vec<StagedFile>,
    snap: Snapshot,
    limits: &AttachmentLimits,
    cx: &mut App,
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
    cx.default_global::<Chips>().0.insert(
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
pub(crate) fn is_companion(file: &StagedFile, staged: &[StagedFile]) -> bool {
    file.filename.strip_suffix(TEXT_SUFFIX).is_some_and(|stem| {
        let image = format!("{stem}{IMAGE_SUFFIX}");
        staged.iter().any(|f| f.filename == image)
    })
}

/// Whether `text` is the text half of the SnapShot picture `image`.
pub(crate) fn pairs(text: &StagedFile, image: &StagedFile) -> bool {
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
pub(crate) fn names(file: &StagedFile, cx: &App) -> Option<(String, String)> {
    let info = cx.try_global::<Chips>()?.0.get(&file.filename)?;
    let label = if info.title.is_empty() {
        format!("SnapShot of {}", info.app)
    } else {
        format!("SnapShot of {} · {}", info.app, info.title)
    };
    Some((info.app.clone(), label))
}

/// A SnapShot picture's app icon, small enough for an inline card.
pub(crate) fn icon(file: &StagedFile, cx: &App) -> Option<AnyElement> {
    if !file.filename.ends_with(IMAGE_SUFFIX) {
        return None;
    }
    let info = cx.try_global::<Chips>()?.0.get(&file.filename)?;
    Some(match info.icon.clone() {
        Some(path) => img(path).size(px(14.)).into_any_element(),
        None => Icon::new(IconName::Frame).xsmall().into_any_element(),
    })
}
