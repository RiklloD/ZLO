//! Motion catalog adapted from Zeron's `motion.rs` (MIT): CSS timing curves
//! evaluated exactly, and the few entrances the app uses.
//!
//! Every helper goes through gpui's `with_animation`, which honors
//! `App::reduce_motion`: the battery saver flips that flag and all of these
//! snap to their end state without scheduling a frame.

use std::time::Duration;

use gpui_kit::{Animation, AnimationExt, ElementId, IntoElement, Styled, px};

/// A CSS `cubic-bezier(x1, y1, x2, y2)`, solved by Newton iteration with a
/// bisection fallback.
#[derive(Clone, Copy)]
pub struct CubicBezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3. * a;
        let bb = 3. * (b - a) - c;
        (1. - c - bb, bb, c)
    }

    fn sample(p1: f32, p2: f32, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(p1, p2);
        ((a * t + b) * t + c) * t
    }

    fn solve_t(&self, x: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        let mut t = x;
        for _ in 0..8 {
            let err = Self::sample(self.x1, self.x2, t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = (3. * a * t + 2. * b) * t + c;
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0_f32, 1_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.;
            if Self::sample(self.x1, self.x2, mid) < x {
                lo = mid
            } else {
                hi = mid
            }
        }
        (lo + hi) / 2.
    }

    /// Eased output for progress `x` in 0..1. Clamped: gpui asserts the
    /// delta stays in range and f32 rounding can overshoot by a hair.
    pub fn eval(&self, x: f32) -> f32 {
        if x <= 0. {
            return 0.;
        }
        if x >= 1. {
            return 1.;
        }
        Self::sample(self.y1, self.y2, self.solve_t(x)).clamp(0., 1.)
    }
}

/// Zeron's entrance curve, `cubic-bezier(0.16, 1, 0.3, 1)`.
pub const EASE_OUT_EXPO: CubicBezier = CubicBezier::new(0.16, 1., 0.3, 1.);
/// CSS `ease`.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.);

#[derive(Clone, Copy)]
pub struct MotionSpec {
    pub duration_ms: u64,
    pub curve: CubicBezier,
}

impl MotionSpec {
    pub const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self { duration_ms, curve }
    }

    pub fn animation(self) -> Animation {
        Animation::new(Duration::from_millis(self.duration_ms))
            .with_easing(move |d| self.curve.eval(d))
    }
}

/// Entrances: 0.5 s expo-out fade plus a 4 px rise.
pub const FADE_IN: MotionSpec = MotionSpec::new(500, EASE_OUT_EXPO);
/// Quick fade: 0.15 s.
pub const FADE_QUICK: MotionSpec = MotionSpec::new(150, EASE);

/// Fade an element in with a short rise. The rise is a relative `top` inset,
/// so like a CSS transform it never moves siblings. `id` must change when
/// the entrance should replay (a new page, a new thread).
pub fn fade_in<E>(
    element: E,
    id: impl Into<ElementId>,
    spec: MotionSpec,
    rise: f32,
) -> impl IntoElement
where
    E: IntoElement + Styled + 'static,
{
    element
        .relative()
        .with_animation(id, spec.animation(), move |el, d| {
            el.opacity(d).top(px(rise * (1. - d)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_hit_their_endpoints_and_stay_monotonic() {
        for curve in [EASE_OUT_EXPO, EASE] {
            assert_eq!(curve.eval(0.), 0.);
            assert_eq!(curve.eval(1.), 1.);
            let mut last = 0.;
            for i in 1..100 {
                let y = curve.eval(i as f32 / 100.);
                assert!(y >= last);
                last = y;
            }
        }
        // ease-out-expo is front-loaded: past 80% at the halfway mark.
        assert!(EASE_OUT_EXPO.eval(0.5) > 0.8);
    }
}
