//! The wallpaper, the way shalldie's vscode-background (MIT) does it in
//! VS Code: one picture laid over the whole window, the conversation, the
//! sidebar or the right pane at a fixed opacity, never catching the mouse.
//! The picture is a painted scene or the person's own, redrawn through a
//! background effect. The pixels come from `paint` on a background thread
//! and land as one static texture sized to its area in points, so the
//! effect's dots stay crisp. Nothing runs while the app is idle; a resize
//! rebuilds once the window settles, and the opacity is free to change. A
//! new picture crossfades in over the old one; a folder can move on to its
//! next picture on a timer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{
    App, AppContext as _, Div, Global, ObjectFit, ParentElement as _, RenderImage, Styled as _,
    StyledImage as _, div, img, px,
};
use image::{Frame, RgbaImage, imageops};

use super::Palette;
use super::motion::{self, CubicBezier, MotionSpec};
use super::paint::{self, Canvas};
use super::prefs::{Prefs, Wallpaper};

/// Zeron's wallpaper replacement: an immediate attack, a short soft landing.
const CROSSFADE: MotionSpec = MotionSpec::new(180, CubicBezier::new(1. / 3., 1., 2. / 3., 1.));

/// Smallest and largest texture side, in points.
const MIN_SIDE: u32 = 120;
const MAX_SIDE: u32 = 2400;
/// A resize waits this long for the window to settle before repainting.
const SETTLE: Duration = Duration::from_millis(280);

#[derive(Default)]
pub struct WallpaperImage {
    image: Option<Arc<RenderImage>>,
    /// The picture being replaced, drawn under the new one while it fades in.
    previous: Option<Arc<RenderImage>>,
    /// Bumped by every replacement; keys the crossfade.
    serial: u64,
    /// The panel size the next picture is painted at.
    wanted: (u32, u32),
    /// Bumped by every request; a finished paint from an older one is
    /// dropped.
    generation: u64,
    /// Which picture of a wallpaper folder is showing.
    shuffle: u64,
    /// Bumped whenever the folder timer is re-armed; an older timer stops.
    rotation: u64,
}

impl Global for WallpaperImage {}

/// The wallpaper over an area `width`×`height` points: an absolutely
/// placed layer the caller anchors, or `None` while there is no picture.
/// Images take no hitbox, so the layer never steals a click or a hover.
pub fn overlay(width: f32, height: f32, cx: &mut App) -> Option<Div> {
    let prefs = Prefs::of(cx);
    if prefs.wallpaper == Wallpaper::None || prefs.wallpaper_opacity <= 0. {
        return None;
    }
    let opacity = prefs.wallpaper_opacity.clamp(0., 1.);
    let contain = prefs.wallpaper_fit == "contain";
    ensure(width, height, cx);
    let layers = layers(cx.reduce_motion(), cx);
    if layers.is_empty() {
        return None;
    }
    Some(
        div()
            .absolute()
            .w(px(width))
            .h(px(height))
            .children(layers.into_iter().map(|(image, k)| {
                img(image)
                    .absolute()
                    .inset_0()
                    .size_full()
                    .opacity(k * opacity)
                    .object_fit(if contain {
                        ObjectFit::Contain
                    } else {
                        ObjectFit::Cover
                    })
            })),
    )
}

/// What to draw this frame, bottom first, with each layer's share. A
/// replaced picture stays underneath until the new one has faded in, then
/// is released.
fn layers(reduced: bool, cx: &mut App) -> Vec<(Arc<RenderImage>, f32)> {
    let Some(state) = cx.try_global::<WallpaperImage>() else {
        return Vec::new();
    };
    let Some(image) = state.image.clone() else {
        return Vec::new();
    };
    let t = motion::reveal(
        &format!("wallpaper:{}", state.serial),
        CROSSFADE,
        0,
        reduced,
    );
    let mut out = Vec::with_capacity(2);
    if t < 1.
        && let Some(old) = &state.previous
    {
        out.push((old.clone(), 1.));
    }
    out.push((image, t));
    if t >= 1.
        && let Some(old) = cx.default_global::<WallpaperImage>().previous.take()
    {
        cx.drop_image(old, None);
    }
    out
}

/// Report the area size (points) from render. Repaints when it changed;
/// the first paint starts at once, a resize waits for the drag to settle.
fn ensure(width: f32, height: f32, cx: &mut App) {
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
    rotate(cx);
    if Prefs::of(cx).wallpaper == Wallpaper::None {
        replace(None, cx);
        cx.refresh_windows();
        return;
    }
    schedule(Duration::ZERO, cx);
}

/// Re-arm the folder timer (`wallpaper_interval` minutes): each tick moves
/// the folder on by one picture. Off, or not a folder, nothing waits.
fn rotate(cx: &mut App) {
    let state = cx.default_global::<WallpaperImage>();
    state.rotation += 1;
    let rotation = state.rotation;
    let prefs = Prefs::of(cx);
    let minutes = prefs.wallpaper_interval;
    if minutes == 0 || !matches!(prefs.wallpaper, Wallpaper::Folder { .. }) {
        return;
    }
    let every = Duration::from_secs(u64::from(minutes) * 60);
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(every).await;
            let live = cx.update(|cx| {
                let live = cx.default_global::<WallpaperImage>().rotation == rotation;
                if live {
                    reshuffle(cx);
                }
                live
            });
            if !live {
                break;
            }
        }
    })
    .detach();
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
    let contain = prefs.wallpaper_fit == "contain";
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
        let mut canvas = match picture.map(|p| load(&p, w, h, contain).map_err(|e| (p, e))) {
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
        to_bgra(canvas.to_image())
    });
    cx.spawn(async move |cx| {
        let image = Arc::new(RenderImage::new(vec![Frame::new(task.await)]));
        cx.update(|cx| {
            if cx.default_global::<WallpaperImage>().generation == generation {
                replace(Some(image), cx);
                cx.refresh_windows();
            }
        });
    })
    .detach();
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

/// Swap the picture. The outgoing one stays as `previous` for the
/// crossfade; anything older goes back to GPUI, which keeps a drawn
/// `RenderImage` in its atlas until told otherwise.
fn replace(image: Option<Arc<RenderImage>>, cx: &mut App) {
    let state = cx.default_global::<WallpaperImage>();
    let incoming = image.is_some();
    let outgoing = std::mem::replace(&mut state.image, image);
    let mut stale: Vec<_> = Vec::new();
    match outgoing {
        Some(old) if incoming => {
            stale.extend(state.previous.replace(old));
            state.serial += 1;
        }
        old => {
            stale.extend(old);
            stale.extend(state.previous.take());
        }
    }
    for old in stale {
        cx.drop_image(old, None);
    }
}

/// gpui textures are BGRA.
fn to_bgra(mut img: RgbaImage) -> RgbaImage {
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    img
}
