//! The new-chat wallpaper, as in Zeron: a painted scene or the person's own
//! picture, redrawn through a background effect in its own colors and
//! faded out by alpha toward the bottom. The pixels come from `paint` on a
//! background thread and land as one static texture, sized to the window
//! in points so the effect's dots stay crisp and a sidebar sliding over it
//! never changes its crop. Nothing runs while the app is idle; a resize
//! rebuilds once the window settles. Threads get a quieter copy of the same
//! picture (`paint::thread`) so text reads anywhere. A new picture
//! crossfades in over the old one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Global, RenderImage};
use image::{Frame, RgbaImage, imageops};

use super::Palette;
use super::motion::{self, CubicBezier, MotionSpec};
use super::paint::{self, Canvas};
use super::prefs::{Prefs, Wallpaper};

/// Zeron's wallpaper replacement: an immediate attack, a short soft landing.
const CROSSFADE: MotionSpec = MotionSpec::new(180, CubicBezier::new(1. / 3., 1., 2. / 3., 1.));

/// Smallest and largest texture side, in points.
const MIN_SIDE: u32 = 320;
const MAX_SIDE: u32 = 2400;
/// A resize waits this long for the window to settle before repainting.
const SETTLE: Duration = Duration::from_millis(280);

#[derive(Default)]
pub struct WallpaperImage {
    image: Option<Arc<RenderImage>>,
    /// The quieter copy shown behind threads.
    soft: Option<Arc<RenderImage>>,
    /// The pair being replaced, drawn under the new one while it fades in.
    previous: Option<[Arc<RenderImage>; 2]>,
    /// Bumped by every replacement; keys the crossfade.
    serial: u64,
    /// The panel size the next picture is painted at.
    wanted: (u32, u32),
    /// Bumped by every request; a finished paint from an older one is
    /// dropped.
    generation: u64,
    /// Which picture of a wallpaper folder is showing.
    shuffle: u64,
}

impl Global for WallpaperImage {}

/// What to draw this frame, bottom first, with each layer's opacity.
/// `soft` runs 0 (new-chat picture) to 1 (thread copy). A replaced pair
/// stays underneath until the new one has faded in, then is released.
pub fn layers(soft: f32, reduced: bool, cx: &mut App) -> Vec<(Arc<RenderImage>, f32)> {
    let Some(state) = cx.try_global::<WallpaperImage>() else {
        return Vec::new();
    };
    let pair = |image: &Arc<RenderImage>, thread: &Arc<RenderImage>, k: f32| {
        let mut out = Vec::with_capacity(2);
        if soft < 1. {
            out.push((image.clone(), (1. - soft) * k));
        }
        if soft > 0. {
            out.push((thread.clone(), soft * k));
        }
        out
    };
    let (Some(image), Some(thread)) = (&state.image, &state.soft) else {
        return Vec::new();
    };
    let t = motion::reveal(
        &format!("wallpaper:{}", state.serial),
        CROSSFADE,
        0,
        reduced,
    );
    let mut out = Vec::with_capacity(4);
    if t < 1.
        && let Some([old, old_thread]) = &state.previous
    {
        out.extend(pair(old, old_thread, 1.));
    }
    out.extend(pair(image, thread, t));
    if t >= 1. {
        let done = cx.default_global::<WallpaperImage>().previous.take();
        for old in done.into_iter().flatten() {
            cx.drop_image(old, None);
        }
    }
    out
}

/// Report the panel size (points) from render. Repaints when it changed;
/// the first paint starts at once, a resize waits for the drag to settle.
pub fn ensure(width: f32, height: f32, cx: &mut App) {
    let size = (
        (width.round() as u32).clamp(MIN_SIDE, MAX_SIDE),
        (height.round() as u32).clamp(MIN_SIDE, MAX_SIDE),
    );
    let state = cx.default_global::<WallpaperImage>();
    if state.wanted == size {
        return;
    }
    let first = state.wanted == (0, 0);
    state.wanted = size;
    schedule(if first { Duration::ZERO } else { SETTLE }, cx);
}

/// Repaint after a preference or the palette changed. The old picture
/// stays up until the new one is ready.
pub fn reload(cx: &mut App) {
    if Prefs::of(cx).wallpaper == Wallpaper::None {
        replace(None, None, cx);
        cx.refresh_windows();
        return;
    }
    schedule(Duration::ZERO, cx);
}

/// A new chat opened: a wallpaper folder moves on to another picture.
pub fn reshuffle(cx: &mut App) {
    if matches!(Prefs::of(cx).wallpaper, Wallpaper::Folder { .. }) {
        cx.default_global::<WallpaperImage>().shuffle += 1;
        schedule(Duration::ZERO, cx);
    }
}

fn schedule(delay: Duration, cx: &mut App) {
    let state = cx.default_global::<WallpaperImage>();
    state.generation += 1;
    let generation = state.generation;
    cx.spawn(async move |cx| {
        if !delay.is_zero() {
            cx.background_executor().timer(delay).await;
        }
        cx.update(|cx| {
            if cx.default_global::<WallpaperImage>().generation == generation {
                paint_now(generation, cx);
            }
        });
    })
    .detach();
}

fn paint_now(generation: u64, cx: &mut App) {
    let state = cx.default_global::<WallpaperImage>();
    let ((w, h), shuffle) = (state.wanted, state.shuffle);
    if w == 0 {
        // No panel measured yet; `ensure` paints on the first frame.
        return;
    }
    let prefs = Prefs::of(cx);
    let choice = prefs.wallpaper.clone();
    let effect = prefs.wallpaper_effect.clone();
    let palette = *Palette::of(cx);
    if choice == Wallpaper::None {
        return;
    }
    let task = cx.background_spawn(async move {
        let dark = !palette.light;
        let auto_scene = if dark { "dusk" } else { "sky" };
        let picture = match &choice {
            Wallpaper::Image { path } => Some(path.clone()),
            Wallpaper::Folder { path } => pick_from(path, shuffle),
            _ => None,
        };
        let mut canvas = match picture.map(|p| load(&p, w, h).map_err(|e| (p, e))) {
            Some(Ok(c)) => c,
            other => {
                if let Some(Err((path, e))) = other {
                    tracing::warn!(
                        "wallpaper {} unusable, painting a scene: {e}",
                        path.display()
                    );
                }
                let name = match &choice {
                    Wallpaper::Scene { name } if name != "auto" => name.as_str(),
                    _ => auto_scene,
                };
                paint::scene(name, w, h)
            }
        };
        paint::effect(&mut canvas, &effect, !dark);
        to_bgra(paint::hero(&canvas))
    });
    cx.spawn(async move |cx| {
        // Threads share the new-chat texture: one upload, same brightness.
        let image = Arc::new(RenderImage::new(vec![Frame::new(task.await)]));
        let soft = image.clone();
        cx.update(|cx| {
            if cx.default_global::<WallpaperImage>().generation == generation {
                replace(Some(image), Some(soft), cx);
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

fn load(path: &Path, w: u32, h: u32) -> Result<Canvas, image::ImageError> {
    let img = image::open(path)?.resize_to_fill(w, h, imageops::FilterType::CatmullRom);
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

/// Swap the pictures. The outgoing pair stays as `previous` for the
/// crossfade; anything older goes back to GPUI, which keeps a drawn
/// `RenderImage` in its atlas until told otherwise.
fn replace(image: Option<Arc<RenderImage>>, soft: Option<Arc<RenderImage>>, cx: &mut App) {
    let state = cx.default_global::<WallpaperImage>();
    let outgoing = match (
        std::mem::replace(&mut state.image, image),
        std::mem::replace(&mut state.soft, soft),
    ) {
        (Some(a), Some(b)) if state.image.is_some() => Some([a, b]),
        (a, b) => {
            let mut stale: Vec<_> = a.into_iter().chain(b).collect();
            stale.extend(state.previous.take().into_iter().flatten());
            for old in stale {
                cx.drop_image(old, None);
            }
            None
        }
    };
    if let Some(pair) = outgoing {
        let state = cx.default_global::<WallpaperImage>();
        let stale = state.previous.replace(pair);
        state.serial += 1;
        for old in stale.into_iter().flatten() {
            cx.drop_image(old, None);
        }
    }
}

/// gpui textures are BGRA.
fn to_bgra(mut img: RgbaImage) -> RgbaImage {
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    img
}
