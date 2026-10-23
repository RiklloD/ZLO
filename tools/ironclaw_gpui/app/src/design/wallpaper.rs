//! The picture behind the whole window. Built once on a background thread
//! (aurora generated from the theme, or a user picture blurred and dimmed),
//! then drawn as one static texture: no per-frame cost.
//!
//! It is rendered small (960x600) on purpose. The content is soft by
//! design, so GPU upscaling is invisible and the texture stays at ~2 MB.

use std::path::Path;
use std::sync::Arc;

use gpui_kit::{App, AppContext as _, Global, Hsla, RenderImage};
use image::{Frame, Rgba, RgbaImage, imageops};

use super::Palette;
use super::prefs::{Prefs, Wallpaper};

const W: u32 = 960;
const H: u32 = 600;

#[derive(Default)]
pub struct WallpaperImage(pub Option<Arc<RenderImage>>);

impl Global for WallpaperImage {}

pub fn current(cx: &App) -> Option<Arc<RenderImage>> {
    cx.try_global::<WallpaperImage>().and_then(|w| w.0.clone())
}

/// Rebuild from the current prefs and palette. Cheap to call; the old
/// picture stays up until the new one is ready.
pub fn reload(cx: &mut App) {
    let choice = Prefs::of(cx).wallpaper.clone();
    let palette = *Palette::of(cx);
    if choice == Wallpaper::None {
        cx.set_global(WallpaperImage(None));
        cx.refresh_windows();
        return;
    }
    let task = cx.background_spawn(async move {
        match &choice {
            Wallpaper::Image { path } => picture(path, &palette).unwrap_or_else(|e| {
                tracing::warn!("wallpaper {} unusable, using aurora: {e}", path.display());
                aurora(&palette)
            }),
            _ => aurora(&palette),
        }
    });
    cx.spawn(async move |cx| {
        let pixels = task.await;
        let image = Arc::new(RenderImage::new(vec![Frame::new(to_bgra(pixels))]));
        cx.update(|cx| {
            cx.set_global(WallpaperImage(Some(image)));
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Soft colour fields from the accent and two neighbouring hues on the
/// theme background, with a vignette and a little dither against banding.
fn aurora(p: &Palette) -> RgbaImage {
    let base = rgb(p.bg);
    let shift = |h: Hsla, by: f32| Hsla {
        h: (h.h + by).rem_euclid(1.),
        ..h
    };
    // (centre x, centre y, radius, strength, colour), in 0..1 of the image.
    let blobs = [
        (0.12, 0.05, 0.62, 0.55, rgb(p.accent)),
        (0.92, 0.30, 0.55, 0.42, rgb(shift(p.accent, 0.11))),
        (0.40, 1.08, 0.70, 0.38, rgb(shift(p.accent, -0.14))),
        (0.70, 0.75, 0.35, 0.18, rgb(p.accent_strong)),
    ];
    RgbaImage::from_fn(W, H, |x, y| {
        let (u, v) = (x as f32 / W as f32, y as f32 / H as f32);
        let aspect = W as f32 / H as f32;
        let mut c = base;
        for (bx, by, r, k, col) in blobs {
            let (dx, dy) = ((u - bx) * aspect, v - by);
            let w = k * (-(dx * dx + dy * dy) / (r * r) * 2.2).exp();
            for i in 0..3 {
                c[i] += (col[i] - c[i]) * w;
            }
        }
        let (cu, cv) = (u - 0.5, v - 0.5);
        let vignette = 1. - 0.35 * (cu * cu + cv * cv) * 2.;
        let noise = (hash(x, y) - 0.5) / 255.;
        let px = |ch: f32| ((ch * vignette + noise).clamp(0., 1.) * 255.).round() as u8;
        Rgba([px(c[0]), px(c[1]), px(c[2]), 255])
    })
}

fn picture(path: &Path, p: &Palette) -> Result<RgbaImage, image::ImageError> {
    let img = image::open(path)?.resize_to_fill(W, H, imageops::FilterType::Triangle);
    let mut img = imageops::fast_blur(&img.to_rgba8(), 14.);
    // Dim toward the theme background so text on glass panels stays legible.
    let base = rgb(p.bg);
    for px in img.pixels_mut() {
        for i in 0..3 {
            let ch = px[i] as f32 / 255.;
            px[i] = ((ch + (base[i] - ch) * 0.5) * 255.).round() as u8;
        }
        px[3] = 255;
    }
    Ok(img)
}

/// gpui textures are BGRA.
fn to_bgra(mut img: RgbaImage) -> RgbaImage {
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    img
}

fn rgb(c: Hsla) -> [f32; 3] {
    let c = c.to_rgb();
    [c.r, c.g, c.b]
}

fn hash(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(374_761_393) ^ y.wrapping_mul(668_265_263);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    (h ^ (h >> 16)) as f32 / u32::MAX as f32
}
