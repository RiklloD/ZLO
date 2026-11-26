//! The wallpaper, the way shalldie's vscode-background (MIT) does it in
//! VS Code: one picture laid over the whole window, the conversation, the
//! sidebar or the right pane at a fixed opacity, never catching the mouse.
//! The picture is a painted scene or the person's own (one file, or a folder
//! it walks through), redrawn through a background effect (`paint`). The
//! pixels are painted on a background thread (`jobs::blocking`) at the
//! area's size in points, so the effect's dots stay crisp, and land in one
//! `rune::Pixels`. Nothing runs while the app is idle; a resize repaints once
//! the area has been still for [`SETTLE`]; opacity and fit change for free.
//! A picture the person just picked crossfades in over the old one (a
//! presence fade on its key, asked by [`reload_picked`]); any other repaint
//! lands at once. A folder can move on to its next picture on a timer.
//!
//! Rune does the compositing itself: the layer's opacity
//! applies to the composited pair (an isolated group), so a crossfade never
//! dips; there is no texture atlas to release by hand (dropping the last
//! `Pixels` handle frees it).

use std::path::{Path, PathBuf};
use std::time::Duration;

use image::imageops;
use rune::{El, Pixels, Spring, SpringPair, SurfaceFit, Transition, Tx, el};

use super::paint::{self, Canvas};
use super::prefs::{Prefs, Wallpaper};
use crate::jobs;
use crate::state::State;

/// Zeron's wallpaper replacement: an immediate attack, a short soft landing.
const CROSSFADE: Spring = Spring::new(0.18, 0.0);
/// The replaced picture is released once the new one has surely landed.
const RELEASE_AFTER: Duration = Duration::from_millis(600);

/// Smallest and largest texture side, in points.
const MIN_SIDE: u32 = 120;
const MAX_SIDE: u32 = 2400;
/// A resize waits this long for the area to settle before repainting.
const SETTLE: Duration = Duration::from_millis(280);

#[derive(Default)]
pub struct WallpaperState {
    image: Option<Pixels>,
    /// The picture being replaced, drawn under the new one while it fades in.
    previous: Option<Pixels>,
    /// Bumped by every new picture; keys the crossfade.
    serial: u64,
    /// The area size the next picture is painted at.
    wanted: (u32, u32),
    /// Bumped by every request; a finished paint from an older one is dropped.
    generation: u64,
    /// Which picture of a wallpaper folder is showing.
    shuffle: u64,
    /// Bumped whenever the folder timer is re-armed; an older timer stops.
    rotation: u64,
    /// The palette brightness the current picture was painted for.
    painted_light: Option<bool>,
    /// The request the person just made (a picture, effect or fit picked):
    /// its paint crossfades in. Any other paint (launch, resize, folder
    /// timer, system appearance) lands at once, as background commits do.
    picked: Option<u64>,
}

impl WallpaperState {}

/// Whether the wallpaper shows at all.
pub fn enabled(prefs: &Prefs) -> bool {
    prefs.wallpaper != Wallpaper::None && prefs.wallpaper_opacity > 0.
}

/// Whether the wallpaper lies over the main composer: the `window` and
/// `conversation` areas hold both the new-thread and the docked chat
/// composer. A `matched` flight between them draws above the whole window,
/// so over this layer: the card would show unwashed (dark) until it landed
/// back under it (see `chat::composer::dock_matched`).
pub fn covers_composer(prefs: &Prefs) -> bool {
    enabled(prefs) && matches!(prefs.wallpaper_area.as_str(), "window" | "conversation")
}

/// The wallpaper layer for `area`, when the preferences put it there: an
/// out-of-flow element pinned to all four edges of its parent, inert (it
/// takes no clicks, hovers or scrolls) and hidden from accessibility. Add it
/// as the LAST child of the area's container so it lies over the content;
/// give the container `.clip_rounded(r)` or pass the radius to
/// [`layer_rounded`]. It reports its size, which is what the picture is
/// painted at, so it is returned (empty) even before the first picture.
pub fn layer(s: &State, area: &str) -> Option<El> {
    layer_rounded(s, area, 0.)
}

/// [`layer`] with the picture's corners rounded to `radius`.
pub fn layer_rounded(s: &State, area: &str, radius: f32) -> Option<El> {
    let prefs = &s.prefs;
    if !enabled(prefs) || prefs.wallpaper_area != area {
        return None;
    }
    let w = &s.look.wallpaper;
    let fit = if prefs.wallpaper_fit == "contain" {
        SurfaceFit::Fit
    } else {
        SurfaceFit::Fill
    };
    let picture = |pixels: &Pixels, serial: u64| {
        el().key(format!("wallpaper-{serial}"))
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .radius(radius)
            .pixels_fit(pixels, fit)
    };
    let mut layer = el()
        .key(format!("wallpaper-{area}"))
        .pin(Some(0.), Some(0.), Some(0.), Some(0.))
        .inert()
        .a11y_hidden()
        .opacity(prefs.wallpaper_opacity.clamp(0., 1.))
        // A new strength (Appearance's slider, a step) glides; the slider's
        // drag moves still land at once, as drags do.
        .opacity_motion(CROSSFADE)
        // Turning the wallpaper off or moving it to another area fades the
        // old layer out (and the new one in) on the crossfade's spring.
        .transition(Transition::opacity().springs(SpringPair::with_exit_ratio(CROSSFADE, 0.8)))
        .on_layout_tx(|s: &mut State, r: [f32; 4], tx: &mut Tx| {
            if !measured(s, r[2], r[3]) {
                tx.live_only();
            }
        });
    if let Some(old) = &w.previous {
        layer = layer.child(picture(old, w.serial.wrapping_sub(1)));
    }
    if let Some(image) = &w.image {
        layer = layer.child(
            picture(image, w.serial)
                .transition(Transition::opacity().springs(SpringPair::new(CROSSFADE, CROSSFADE))),
        );
    }
    Some(layer)
}

/// The area's size (points) from layout. Repaints when it changed: the
/// first paint starts at once, a resize waits for the drag to settle.
/// Returns whether anything changed.
fn measured(s: &mut State, width: f32, height: f32) -> bool {
    let size = (
        (width.round() as u32).clamp(MIN_SIDE, MAX_SIDE),
        (height.round() as u32).clamp(MIN_SIDE, MAX_SIDE),
    );
    let w = &mut s.look.wallpaper;
    if w.wanted == size {
        return false;
    }
    let first = w.wanted == (0, 0);
    w.wanted = size;
    schedule(s, if first { Duration::ZERO } else { SETTLE });
    true
}

/// Repaint after a preference or the palette changed. The old picture
/// stays up until the new one is ready.
pub fn reload(s: &mut State) {
    rotate(s);
    if s.prefs.wallpaper == Wallpaper::None {
        replace(s, None);
        return;
    }
    schedule(s, Duration::ZERO);
}

/// [`reload`] for a change the person just made: the new picture's
/// landing crossfades over the old one instead of swapping at once.
pub fn reload_picked(s: &mut State) {
    reload(s);
    let w = &mut s.look.wallpaper;
    w.picked = Some(w.generation);
}

/// Re-arm the folder timer (`wallpaper_interval` minutes): each tick moves
/// the folder on by one picture. Off, or not a folder, nothing waits.
pub fn rotate(s: &mut State) {
    s.look.wallpaper.rotation += 1;
    let rotation = s.look.wallpaper.rotation;
    let minutes = s.prefs.wallpaper_interval;
    if minutes == 0 || !matches!(s.prefs.wallpaper, Wallpaper::Folder { .. }) {
        return;
    }
    arm(s, rotation, Duration::from_secs(u64::from(minutes) * 60));
}

fn arm(s: &State, rotation: u64, every: Duration) {
    jobs::after(s, every, move |s, _| {
        if s.look.wallpaper.rotation == rotation {
            reshuffle(s);
            arm(s, rotation, every);
        }
    });
}

/// A new chat opened: a wallpaper folder moves on to another picture.
pub fn reshuffle(s: &mut State) {
    if matches!(s.prefs.wallpaper, Wallpaper::Folder { .. }) {
        s.look.wallpaper.shuffle += 1;
        schedule(s, Duration::ZERO);
    }
}

/// The palette's brightness changed since the picture was painted (scheme
/// switch, system appearance): the auto scene and the effects follow it.
pub fn stale_for(s: &State) -> bool {
    let w = &s.look.wallpaper;
    w.image.is_some() && w.painted_light.is_some_and(|l| l != s.palette.light)
}

fn schedule(s: &mut State, delay: Duration) {
    s.look.wallpaper.generation += 1;
    let generation = s.look.wallpaper.generation;
    if delay.is_zero() {
        paint_now(s, generation);
        return;
    }
    jobs::after(s, delay, move |s, _| {
        if s.look.wallpaper.generation == generation {
            paint_now(s, generation);
        }
    });
}

/// Everything a paint needs, so it can run on any thread.
struct Job {
    choice: Wallpaper,
    effect: String,
    contain: bool,
    light: bool,
    size: (u32, u32),
    shuffle: u64,
}

impl Job {
    fn of(s: &State) -> Option<Self> {
        let w = &s.look.wallpaper;
        if w.wanted.0 == 0 || s.prefs.wallpaper == Wallpaper::None {
            // No area measured yet; `measured` paints on the first layout.
            return None;
        }
        Some(Self {
            choice: s.prefs.wallpaper.clone(),
            effect: s.prefs.wallpaper_effect.clone(),
            contain: s.prefs.wallpaper_fit == "contain",
            light: s.palette.light,
            size: w.wanted,
            shuffle: w.shuffle,
        })
    }

    /// The picture as straight-alpha RGBA: (width, height, bytes).
    fn run(self) -> (u32, u32, Vec<u8>) {
        let (w, h) = self.size;
        let auto_scene = if self.light { "sky" } else { "dusk" };
        let picture = match &self.choice {
            Wallpaper::Image { path } => Some(path.clone()),
            Wallpaper::Folder { path } => pick_from(path, self.shuffle),
            _ => None,
        };
        let loaded = picture.map(|p| load(&p, w, h, self.contain).map_err(|e| (p, e)));
        let mut canvas = match loaded {
            Some(Ok(c)) => c,
            other => {
                if let Some(Err((path, e))) = other {
                    tracing::warn!(
                        "wallpaper {} unusable, painting a scene: {e}",
                        path.display()
                    );
                }
                let name = match &self.choice {
                    Wallpaper::Scene { name } if name != "auto" => name.as_str(),
                    _ => auto_scene,
                };
                paint::scene(name, w, h)
            }
        };
        paint::effect(&mut canvas, &self.effect, self.light);
        let img = canvas.to_image();
        (img.width(), img.height(), img.into_raw())
    }
}

fn paint_now(s: &mut State, generation: u64) {
    let Some(job) = Job::of(s) else { return };
    let light = job.light;
    jobs::blocking(
        s,
        move || job.run(),
        move |s, (w, h, rgba), tx| painted(s, generation, (w, h, rgba), light, tx),
    );
}

/// A paint finished: the latest request's picture lands, crossfading when
/// the person asked for it (a job reply lands at once otherwise).
fn painted(
    s: &mut State,
    generation: u64,
    (w, h, rgba): (u32, u32, Vec<u8>),
    light: bool,
    tx: &mut Tx,
) {
    let wp = &mut s.look.wallpaper;
    if wp.generation != generation {
        return;
    }
    if wp.picked.take() == Some(generation) {
        tx.animated();
    }
    land(s, w, h, rgba, light);
}

fn land(s: &mut State, w: u32, h: u32, rgba: Vec<u8>, light: bool) {
    let pixels = Pixels::new();
    pixels.set(w, h, rgba);
    s.look.wallpaper.painted_light = Some(light);
    replace(s, Some(pixels));
}

/// Paint at `size` on this thread and show it at once. For headless
/// fixtures, whose async replies are dropped.
pub fn paint_blocking(s: &mut State, size: (u32, u32)) {
    s.look.wallpaper.wanted = size;
    if let Some(job) = Job::of(s) {
        let light = job.light;
        let (w, h, rgba) = job.run();
        land(s, w, h, rgba, light);
    }
}

/// The picture at the area's size: cropped to fill it, or whole inside it
/// (`contain`), its aspect kept either way.
fn load(path: &Path, w: u32, h: u32, contain: bool) -> Result<Canvas, image::ImageError> {
    let img = image::open(path)?;
    let filter = imageops::FilterType::CatmullRom;
    let img = if contain {
        img.resize(w, h, filter)
    } else {
        img.resize_to_fill(w, h, filter)
    };
    Ok(Canvas::from_image(&img.to_rgba8()))
}

/// The `shuffle`-th picture of a folder, in a stable but scrambled order.
fn pick_from(dir: &Path, shuffle: u64) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "webp"
                )
            })
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    files.sort();
    // Start somewhere different on every launch, then walk on.
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 3600);
    let ix = (start.wrapping_mul(2_654_435_761).wrapping_add(shuffle * 7)) as usize % files.len();
    Some(files.swap_remove(ix))
}

/// Swap the picture. The outgoing one stays as `previous` for the crossfade
/// and is let go once the new one has landed.
fn replace(s: &mut State, image: Option<Pixels>) {
    let w = &mut s.look.wallpaper;
    let incoming = image.is_some();
    let outgoing = std::mem::replace(&mut w.image, image);
    match outgoing {
        Some(old) if incoming => {
            w.previous = Some(old);
            w.serial += 1;
            let serial = w.serial;
            jobs::after(s, RELEASE_AFTER, move |s, _| {
                if s.look.wallpaper.serial == serial {
                    s.look.wallpaper.previous = None;
                }
            });
        }
        _ => {
            if incoming {
                w.serial += 1;
            } else {
                w.painted_light = None;
            }
            w.previous = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the paint the person asked for crossfades in; a background
    /// repaint (launch, resize, timer) lands at once, and a superseded
    /// paint is dropped.
    #[test]
    fn only_a_picked_picture_crossfades_in() {
        let mut s = crate::chat::fixtures::test_state();
        s.prefs.wallpaper = Wallpaper::Aurora;
        let px = || (1, 1, vec![0u8; 4]);

        reload_picked(&mut s);
        let picked = s.look.wallpaper.generation;
        let mut tx = Tx::default();
        painted(&mut s, picked, px(), true, &mut tx);
        assert!(crate::workspace::asked_motion(&tx), "a pick crossfades");
        assert!(s.look.wallpaper.image.is_some());

        reload(&mut s);
        let background = s.look.wallpaper.generation;
        let mut tx = Tx::default();
        painted(&mut s, background, px(), true, &mut tx);
        assert!(
            !crate::workspace::asked_motion(&tx),
            "a repaint lands at once"
        );

        let serial = s.look.wallpaper.serial;
        reload_picked(&mut s);
        let mut tx = Tx::default();
        painted(&mut s, background, px(), true, &mut tx);
        assert_eq!(
            s.look.wallpaper.serial, serial,
            "a superseded paint is dropped"
        );
        assert!(!crate::workspace::asked_motion(&tx));
    }

    /// The composers' matched flight draws over the wallpaper, so it is off
    /// wherever the wallpaper lies over them.
    #[test]
    fn the_wallpaper_covers_the_composer_in_the_main_areas_only() {
        let mut prefs = Prefs {
            wallpaper: Wallpaper::Aurora,
            wallpaper_opacity: 0.4,
            ..Prefs::default()
        };
        for (area, covers) in [
            ("window", true),
            ("conversation", true),
            ("sidebar", false),
            ("pane", false),
        ] {
            prefs.wallpaper_area = area.into();
            assert_eq!(covers_composer(&prefs), covers, "{area}");
        }
        prefs.wallpaper_area = "window".into();
        prefs.wallpaper_opacity = 0.;
        assert!(
            !covers_composer(&prefs),
            "an invisible wallpaper covers nothing"
        );
        prefs.wallpaper_opacity = 0.4;
        prefs.wallpaper = Wallpaper::None;
        assert!(!covers_composer(&prefs));
    }

    #[test]
    fn a_folder_without_pictures_picks_nothing() {
        let dir = std::env::temp_dir().join("zlo-rune-empty-wallpaper-folder");
        let _ = std::fs::create_dir_all(&dir);
        assert!(pick_from(&dir, 0).is_none());
    }

    #[test]
    fn a_scene_job_paints_the_requested_size() {
        let job = Job {
            choice: Wallpaper::Scene {
                name: "night".into(),
            },
            effect: "dither".into(),
            contain: false,
            light: false,
            size: (160, 120),
            shuffle: 0,
        };
        let (w, h, rgba) = job.run();
        assert_eq!((w, h), (160, 120));
        assert_eq!(rgba.len(), 160 * 120 * 4);
    }
}
