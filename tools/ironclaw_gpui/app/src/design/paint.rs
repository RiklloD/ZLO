//! Wallpaper painting on the CPU, free of GPUI so it runs on a background
//! thread (and in a plain test): a painted sky scene or a picture, redrawn
//! through one of Zeron's background effects, then faded into the panel
//! color at the bottom so the new-chat screen dissolves into the app.
//!
//! The work happens once per wallpaper, window size or theme change. The
//! result is a single static texture; nothing runs while the app is idle.

use image::{Rgba, RgbaImage};

pub type Rgb = [f32; 3];

/// Painted scenes: (id, name). "auto" picks Sky on light and Dusk on dark.
pub const SCENES: [(&str, &str); 3] = [("sky", "Sky"), ("dusk", "Dusk"), ("night", "Night")];

/// Background effects: (id, name).
pub const EFFECTS: [(&str, &str); 4] = [
    ("none", "None"),
    ("dither", "Dither"),
    ("halftone", "Halftone"),
    ("scanlines", "Scanlines"),
];

/// A floating-point picture, row-major.
#[derive(Clone)]
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    px: Vec<Rgb>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![[0.; 3]; (w * h) as usize],
        }
    }

    pub fn from_image(img: &RgbaImage) -> Self {
        let mut c = Self::new(img.width(), img.height());
        for (i, p) in img.pixels().enumerate() {
            c.px[i] = [p[0] as f32 / 255., p[1] as f32 / 255., p[2] as f32 / 255.];
        }
        c
    }

    pub fn to_image(&self) -> RgbaImage {
        let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
        RgbaImage::from_fn(self.w, self.h, |x, y| {
            let c = self.px[(y * self.w + x) as usize];
            Rgba([byte(c[0]), byte(c[1]), byte(c[2]), 255])
        })
    }

    fn at(&self, x: u32, y: u32) -> Rgb {
        self.px[(y * self.w + x) as usize]
    }

    /// Bilinear resample to `w`×`h`.
    fn resized(&self, w: u32, h: u32) -> Self {
        let mut out = Self::new(w, h);
        let (sx, sy) = (self.w as f32 / w as f32, self.h as f32 / h as f32);
        for y in 0..h {
            let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0., (self.h - 1) as f32);
            let (y0, ty) = (fy.floor() as u32, fy.fract());
            let y1 = (y0 + 1).min(self.h - 1);
            for x in 0..w {
                let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0., (self.w - 1) as f32);
                let (x0, tx) = (fx.floor() as u32, fx.fract());
                let x1 = (x0 + 1).min(self.w - 1);
                let top = mix(self.at(x0, y0), self.at(x1, y0), tx);
                let bottom = mix(self.at(x0, y1), self.at(x1, y1), tx);
                out.px[(y * w + x) as usize] = mix(top, bottom, ty);
            }
        }
        out
    }
}

/// Paint a scene at `w`×`h`. Unknown ids paint Sky.
pub fn scene(id: &str, w: u32, h: u32) -> Canvas {
    let look = match id {
        "dusk" => &DUSK,
        "night" => &NIGHT,
        _ => &SKY,
    };
    // Clouds are soft, so they are computed at half size and scaled up;
    // the effects then work at full size.
    let (hw, hh) = ((w / 2).max(2), (h / 2).max(2));
    let aspect = hw as f32 / hh as f32;
    let mut density = vec![0f32; (hw * hh) as usize];
    for y in 0..hh {
        for x in 0..hw {
            let (u, v) = (x as f32 / hw as f32, y as f32 / hh as f32);
            density[(y * hw + x) as usize] = clouds(u * aspect, v, look.seed) + framing(u, v);
        }
    }
    // Light falls from `sun`: a cloud is lit where density drops toward it.
    let reach = (hh as f32 * 0.025).max(1.);
    let (dx, dy) = (
        (look.sun[0] * reach).round() as i32,
        (look.sun[1] * reach).round() as i32,
    );
    let sample = |x: i32, y: i32| {
        let x = x.clamp(0, hw as i32 - 1) as u32;
        let y = y.clamp(0, hh as i32 - 1) as u32;
        density[(y * hw + x) as usize]
    };
    let mut small = Canvas::new(hw, hh);
    for y in 0..hh {
        let v = y as f32 / hh as f32;
        let sky = gradient(look.sky, v);
        for x in 0..hw {
            let d = sample(x as i32, y as i32);
            let alpha = smoothstep(0.52, 0.6, d);
            let lit =
                ((d - sample(x as i32 + dx, y as i32 + dy)) * 6. + look.ambient).clamp(0., 1.);
            let body = mix(look.shadow, look.lit, lit);
            let body = mix(body, look.glow, smoothstep(0.7, 1., lit) * 0.8);
            // Thin edges let the sky through, as in a painted cloud.
            let edge = smoothstep(0.6, 0.52, d) * 0.3;
            small.px[(y * hw + x) as usize] = mix(sky, mix(body, sky, edge), alpha);
        }
    }
    let mut out = small.resized(w, h);
    if look.stars {
        for y in 0..h {
            for x in 0..w {
                let n = hash(x as i32, y as i32, 7);
                if n > 0.9982 {
                    let v = y as f32 / h as f32;
                    let k = (n - 0.9982) / 0.0018 * smoothstep(0.7, 0.2, v);
                    let c = &mut out.px[(y * w + x) as usize];
                    // Stars hide behind clouds: only bright where the sky is dark.
                    let dark = 1. - luma(*c).min(1.) * 2.;
                    for ch in c.iter_mut() {
                        *ch += k * dark.max(0.);
                    }
                }
            }
        }
    }
    out
}

/// Pull a picture toward the theme so the composer and text stay legible.
pub fn tone(c: &mut Canvas, bg: Rgb, dark: bool) {
    let k = if dark { 0.3 } else { 0.08 };
    for p in &mut c.px {
        *p = mix(*p, bg, k);
    }
}

/// The thread backdrop: the same picture as the new-chat screen under one
/// even veil, so it runs edge to edge (behind the right pane too) and text
/// reads anywhere on it. The veil is heavier on the tones that fight the
/// text (highlights in dark themes, shadows in light ones), so clouds stop
/// washing out words while the picture keeps its color, and it deepens a
/// little toward the composer.
pub fn thread(c: &Canvas, bg: Rgb, dark: bool) -> Canvas {
    let mut out = c.clone();
    let (w, h) = (c.w, c.h);
    for y in 0..h {
        let low = 0.12 * smoothstep(0.7, 1., y as f32 / h as f32);
        for x in 0..w {
            let i = (y * w + x) as usize;
            let l = luma(out.px[i]).clamp(0., 1.);
            let glare = if dark { l } else { 1. - l };
            let k = (0.5 + 0.38 * glare + low).min(0.92);
            out.px[i] = mix(out.px[i], bg, k);
        }
    }
    out
}

/// Redraw the picture through an effect id from [`EFFECTS`].
pub fn effect(c: &mut Canvas, id: &str) {
    match id {
        "dither" => dither(c),
        "halftone" => halftone(c),
        "scanlines" => scanlines(c),
        _ => {}
    }
}

/// Tint the lower part toward `bg`, from `from` to `to` (0..1 of height),
/// reaching `max` at the bottom edge. A sub-step dither keeps the long
/// dark ramp from banding once it is cut to 8 bits.
pub fn fade(c: &mut Canvas, bg: Rgb, from: f32, to: f32, max: f32) {
    for y in 0..c.h {
        let t = max * smoothstep(from, to, y as f32 / c.h as f32);
        if t <= 0. {
            continue;
        }
        for x in 0..c.w {
            let i = (y * c.w + x) as usize;
            let n = (hash(x as i32, y as i32, 7) - 0.5) / 255.;
            c.px[i] = mix(c.px[i], bg, t).map(|ch| ch + n);
        }
    }
}

/// Ordered (Bayer 4×4) dither onto six levels a channel: the artwork comes
/// back as a fine dot pattern, Zeron's "dithered color palette".
fn dither(c: &mut Canvas) {
    const BAYER: [[f32; 4]; 4] = [
        [0., 8., 2., 10.],
        [12., 4., 14., 6.],
        [3., 11., 1., 9.],
        [15., 7., 13., 5.],
    ];
    const LEVELS: f32 = 5.;
    for y in 0..c.h {
        for x in 0..c.w {
            let t = (BAYER[(y % 4) as usize][(x % 4) as usize] + 0.5) / 16.;
            let p = &mut c.px[(y * c.w + x) as usize];
            for ch in p.iter_mut() {
                *ch = ((ch.clamp(0., 1.) * LEVELS + t).floor() / LEVELS).min(1.);
            }
        }
    }
}

/// Print-style halftone on a 45° grid: each cell becomes one dot of its
/// own color, larger where the picture is darker, on a paler paper.
fn halftone(c: &mut Canvas) {
    const CELL: f32 = 7.;
    let src = Canvas {
        w: c.w,
        h: c.h,
        px: c.px.clone(),
    };
    let (s, co) = (
        std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
    );
    for y in 0..c.h {
        for x in 0..c.w {
            let (fx, fy) = (x as f32, y as f32);
            // Rotate into the grid, find the cell center, rotate back.
            let (gx, gy) = ((fx * co + fy * s) / CELL, (-fx * s + fy * co) / CELL);
            let (cx, cy) = (gx.floor() + 0.5, gy.floor() + 0.5);
            let (ox, oy) = ((cx * co - cy * s) * CELL, (cx * s + cy * co) * CELL);
            let center = src.at(
                (ox.round().clamp(0., (c.w - 1) as f32)) as u32,
                (oy.round().clamp(0., (c.h - 1) as f32)) as u32,
            );
            let dist = ((gx - cx).powi(2) + (gy - cy).powi(2)).sqrt();
            let radius = 0.28 + 0.42 * (1. - luma(center)).clamp(0., 1.).sqrt();
            let ink = smoothstep(radius + 0.06, radius - 0.06, dist);
            let paper = mix(center, [1.; 3], 0.22 * luma(center));
            let dot = mix(center, [0.; 3], 0.12);
            c.px[(y * c.w + x) as usize] = mix(paper, dot, ink);
        }
    }
}

/// CRT scanlines: every third row darkened, the one above it brightened.
fn scanlines(c: &mut Canvas) {
    for y in 0..c.h {
        let k = match y % 3 {
            2 => 0.72,
            1 => 1.06,
            _ => 1.,
        };
        for x in 0..c.w {
            let p = &mut c.px[(y * c.w + x) as usize];
            for ch in p.iter_mut() {
                *ch *= k;
            }
        }
    }
}

struct Look {
    seed: u32,
    /// (stop 0..1 of height, color) top to bottom.
    sky: &'static [(f32, Rgb)],
    lit: Rgb,
    glow: Rgb,
    shadow: Rgb,
    /// Where the light comes from, in pixels per step (y grows down).
    sun: [f32; 2],
    /// Lighting at rest: low keeps most of a cloud in its shadow color.
    ambient: f32,
    stars: bool,
}

/// Pastel day sky: periwinkle above, pink and peach cumulus at the sides.
const SKY: Look = Look {
    seed: 11,
    sky: &[
        (0., [0.30, 0.40, 0.86]),
        (0.32, [0.50, 0.58, 0.95]),
        (0.58, [0.80, 0.80, 0.97]),
        (0.85, [0.93, 0.91, 0.95]),
    ],
    lit: [1.0, 0.82, 0.80],
    glow: [1.0, 0.96, 0.91],
    shadow: [0.66, 0.60, 0.88],
    sun: [-1., -1.],
    ambient: 0.5,
    stars: false,
};

/// Late sunset: indigo above, an ember horizon lighting clouds from below.
const DUSK: Look = Look {
    seed: 23,
    sky: &[
        (0., [0.06, 0.06, 0.17]),
        (0.28, [0.16, 0.11, 0.30]),
        (0.48, [0.46, 0.22, 0.40]),
        (0.64, [0.90, 0.52, 0.42]),
    ],
    lit: [0.98, 0.56, 0.46],
    glow: [1.0, 0.80, 0.58],
    shadow: [0.20, 0.13, 0.30],
    sun: [0.4, 1.],
    ambient: 0.18,
    stars: true,
};

/// Moonlit night: deep blue, silver-edged clouds and a few stars.
const NIGHT: Look = Look {
    seed: 37,
    sky: &[
        (0., [0.02, 0.03, 0.09]),
        (0.45, [0.05, 0.08, 0.19]),
        (0.8, [0.11, 0.15, 0.29]),
    ],
    lit: [0.48, 0.54, 0.72],
    glow: [0.80, 0.84, 0.95],
    shadow: [0.06, 0.07, 0.16],
    sun: [-0.6, -1.],
    ambient: 0.3,
    stars: true,
};

/// Cloud density near 0.5, warped so the shapes billow.
fn clouds(u: f32, v: f32, seed: u32) -> f32 {
    let wx = fbm(u * 1.1, v * 1.1, 3, seed + 101);
    let wy = fbm(u * 1.1 + 5.2, v * 1.1 + 1.3, 3, seed + 202);
    // Billows: the absolute value folds the noise into rounded lobes.
    let base = fbm(u * 2.0 + 0.7 * wx, v * 2.6 + 0.7 * wy, 5, seed);
    let puff = 1. - (fbm(u * 5.0, v * 6.0, 3, seed + 303) - 0.5).abs() * 2.;
    base * 0.82 + puff * 0.18
}

/// Where clouds gather: banks at both sides, wisps across the top, a clear
/// middle for the composer and none in the lower half.
fn framing(u: f32, v: f32) -> f32 {
    let side = smoothstep(0.2, 0.95, (u - 0.5).abs() * 2.);
    let top = smoothstep(0.5, 0.0, v);
    let keep = 1. - smoothstep(0.42, 0.7, v);
    ((0.8 * side + 0.32 * top).min(1.) * keep - 0.62) * 0.42
}

fn gradient(stops: &[(f32, Rgb)], v: f32) -> Rgb {
    let mut prev = stops[0];
    for &(at, c) in stops {
        if v <= at {
            let t = if at > prev.0 {
                (v - prev.0) / (at - prev.0)
            } else {
                1.
            };
            return mix(prev.1, c, smoothstep(0., 1., t));
        }
        prev = (at, c);
    }
    prev.1
}

fn fbm(mut x: f32, mut y: f32, octaves: u32, seed: u32) -> f32 {
    let (mut sum, mut amp, mut norm) = (0., 0.5, 0.);
    for i in 0..octaves {
        sum += amp * noise(x, y, seed.wrapping_add(i));
        norm += amp;
        // Rotate each octave so the grid never lines up.
        let (nx, ny) = (0.8 * x - 0.6 * y, 0.6 * x + 0.8 * y);
        x = nx * 2.03 + 1.7;
        y = ny * 2.03 + 9.2;
        amp *= 0.5;
    }
    sum / norm
}

fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = (x - fx, y - fy);
    let (ux, uy) = (tx * tx * (3. - 2. * tx), ty * ty * (3. - 2. * ty));
    let (ix, iy) = (fx as i32, fy as i32);
    let a = hash(ix, iy, seed);
    let b = hash(ix + 1, iy, seed);
    let c = hash(ix, iy + 1, seed);
    let d = hash(ix + 1, iy + 1, seed);
    a + (b - a) * ux + (c - a) * uy + (a - b - c + d) * ux * uy
}

fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x27d4_eb2d)
        ^ (y as u32).wrapping_mul(0x1656_67b1)
        ^ seed.wrapping_mul(0x9e37_79b9);
    h = (h ^ (h >> 15)).wrapping_mul(0x85eb_ca6b);
    h = (h ^ (h >> 13)).wrapping_mul(0xc2b2_ae35);
    (h ^ (h >> 16)) as f32 / u32::MAX as f32
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0., 1.);
    t * t * (3. - 2. * t)
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn luma(c: Rgb) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}
