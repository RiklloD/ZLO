//! Looping marks: the working dots, Zeron's gradient spinner, the shimmer,
//! the live status dot and the claw mark (GPUI `kit.rs` on `pulse.rs`).
//!
//! GPUI ran them on a shared 30 Hz lease clock (`pulse.rs`) that notified
//! the views painting a loop. Rune's loops already do that job: every
//! `El::cycle`/`El::looping` track runs on the one render clock, wakes the
//! window only while visible (at the cycle's own `fps`), stays in phase with
//! every loop of the same period, and holds its `still` pose under reduced
//! motion, which the battery saver sets. So each mark is a static tree with
//! opacity cycles; nothing here touches app state.
//!
//! Approximations: Rune cycles animate opacity only, so the pulse dots no
//! longer lift two points at their peak and the status dot's halo pulses in
//! place instead of growing.

use rune::tok::TextStyle;
use rune::{Color, Cycle, El, col, el, row, text};

use super::{lift, neutral};
use crate::design::motion::{GRADIENT_SPIN_MS, GSPIN_STOPS, gspin_opacity};
use crate::design::styles::Style;
use crate::state::State;

/// Loop frame rate for small loaders (GPUI `Rate::Half`).
const HALF_FPS: f32 = 15.;

/// A sine bump 0.25 → 1 → 0.25 over the first half of a period, resting at
/// 0.25 for the second (GPUI `0.25 + 0.75 * max(sin, 0)`).
const DOT_WAVE: &[(f32, f32)] = &[
    (0., 0.25),
    (0.083, 0.625),
    (0.25, 1.),
    (0.417, 0.625),
    (0.5, 0.25),
    (1., 0.25),
];

/// Three dots breathing in sequence: the "working" mark. Each peaks a beat
/// after the one before it.
pub fn pulse_dots(key: &str, color: Color) -> El {
    let mut dots = row().key(key.to_string()).gap(3.).center_y().a11y_hidden();
    for i in 0..3 {
        dots = dots.child(
            el().key(format!("{key}-{i}"))
                .size(5., 5.)
                .radius(2.5)
                .fill(color)
                .cycle(Cycle {
                    period: 1.2,
                    phase: i as f32 * 0.18,
                    stops: DOT_WAVE,
                    still: 0.25,
                    fps: HALF_FPS,
                }),
        );
    }
    dots
}

/// Zeron's working glyph: a 3×3 grid of dots tinted blue, amber and pink by
/// row, with a brightness wave travelling up from the bottom edge every
/// 750 ms. Reduced motion freezes it at rest.
pub fn gradient_spinner(key: &str, cell: f32) -> El {
    const TINTS: [u32; 3] = [0xB6D3EF, 0xEDB185, 0xF888A0];
    let gap = cell / 2.;
    let period = GRADIENT_SPIN_MS as f32 / 1000.;
    let mut grid = col().key(key.to_string()).gap(gap).a11y_hidden();
    for (r, tint) in TINTS.iter().enumerate() {
        let mut line = row().gap(gap);
        for c in 0..3 {
            // Distance from the wave origin (bottom row, centre column).
            let d = 2. - r as f32 + (c as f32 - 1.).abs();
            line = line.child(
                el().key(format!("{key}-{r}-{c}"))
                    .size(cell, cell)
                    .radius(cell / 2.)
                    .fill(crate::design::rgb(*tint))
                    .cycle(Cycle {
                        period,
                        phase: -d / 4.,
                        stops: GSPIN_STOPS,
                        still: gspin_opacity(d / 4., 0.1),
                        fps: HALF_FPS,
                    }),
            );
        }
        grid = grid.child(line);
    }
    grid
}

/// A sine 0 → 1 → 0 over a period, for the shimmer's bright copy.
const SHIMMER: &[(f32, f32)] = &[
    (0., 0.5),
    (0.125, 0.854),
    (0.25, 1.),
    (0.375, 0.854),
    (0.5, 0.5),
    (0.625, 0.146),
    (0.75, 0.),
    (0.875, 0.146),
    (1., 0.5),
];

/// A shimmer sweeping across a label while something streams in: the faint
/// label, and the bright one over it breathing in and out.
pub fn shimmer_text(s: &State, key: &str, label: &str, font: TextStyle) -> El {
    let p = &s.palette;
    el().key(key.to_string())
        .label(label.to_string())
        .child(
            text(label.to_string())
                .font(font)
                .color(p.text_faint)
                .single_line(),
        )
        .child(
            text(label.to_string())
                .key(format!("{key}-lit"))
                .font(font)
                .color(p.text)
                .single_line()
                .absolute(0., 0.)
                .a11y_hidden()
                .cycle(Cycle {
                    period: 1.6,
                    phase: 0.,
                    stops: SHIMMER,
                    still: 0.5,
                    fps: HALF_FPS,
                }),
        )
}

/// The live halo fading out from full.
const HALO: &[(f32, f32)] = &[(0., 0.45), (1., 0.)];

/// A small status dot that can pulse a soft halo.
pub fn status_dot(key: &str, color: Color, live: bool) -> El {
    let mut dot = el().key(key.to_string()).size(6., 6.).a11y_hidden();
    if live {
        dot = dot.child(
            el().key(format!("{key}-halo"))
                .absolute(-3., -3.)
                .size(12., 12.)
                .radius(6.)
                .fill(color)
                .cycle(Cycle {
                    period: 1.8,
                    phase: 0.,
                    stops: HALO,
                    still: 0.,
                    fps: HALF_FPS,
                }),
        );
    }
    dot.child(el().absolute(0., 0.).size(6., 6.).radius(3.).fill(color))
}

/// The IronClaw mark: three claw strokes on a raised tile, lit in the
/// accent. Plain quads, so it scales with the style at no cost.
pub fn claw_mark(s: &State) -> El {
    let st = Style::of(&s.prefs);
    let p = &s.palette;
    let tile = 52.;
    let stroke = |h: f32, dy: f32| {
        el().w(6.)
            .h(h)
            .mt(dy)
            .radius(3.)
            .gradient(lift(p.accent, 0.12), p.accent_strong)
    };
    let mut plate = neutral(&st, p, 1.);
    plate.hover = Color::TRANSPARENT;
    plate.apply(
        row()
            .size(tile, tile)
            .radius(tile * 0.3)
            .items_start()
            .justify_center()
            .pt(12.)
            .gap(5.)
            .a11y_hidden()
            .child(stroke(22., 4.))
            .child(stroke(28., 0.))
            .child(stroke(22., 4.)),
    )
}
