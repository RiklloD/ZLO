//! Motion catalog, ported from the GPUI app's `design/motion.rs` (itself
//! adapted from Zeron's `motion.rs`, MIT): the timings and curves every area
//! animates on, as Rune curves and springs, plus the few helpers Rune does not
//! already have (the working-word vocabulary, elapsed-time labels, a
//! premultiplied colour mix).
//!
//! What GPUI needed hand-rolled and Rune does itself, so it is NOT ported:
//!
//! | GPUI (`design/…`) | Rune |
//! |---|---|
//! | `CubicBezier` solver | `rune::Easing::cubic` / `rune::CubicBezier` (solved like browsers) |
//! | `MotionSpec` (ms + curve) | `rune::Curve::tween(secs, easing)`; `Anim` via `.into()` |
//! | `Spring` (damping + ms) | `rune::Spring::new(duration, bounce)` |
//! | hover tween store (`set_hover`, `hover_t`, `hover_listener`) | `El::hover(color)` + `hover_motion` (paint-only, no commit) |
//! | state tweens (`state_t`, `glide`) | keyed elements: a changed fill, width or offset springs on `.motion(..)` |
//! | `fade_in`, `fade_in_after`, `pop_in` | `.transition(..)` presence ([`pop`]); nothing animates on the first frame |
//! | `born`, `reveal`, `arrive`, `settle`, `restart` | presence transitions and growth stacks (`El::growth`) |
//! | `collapse`, `grow` (height morphs) | presence (`Transition::reveal`), or a growth stack |
//! | `roll_text` | `rune_kit::RollingText` |
//! | `pump`, `frame_tail` (keep frames coming) | Rune's clock requests frames only while a track moves |
//! | `dock.rs` composer hand-off | `El::matched(COMPOSER_MATCH)` on both composers ([`COMPOSER_MATCH`]) |
//! | `flip.rs` resorted rows / sliding pills | keyed inbox rows on `.motion(ROW_RESORT)`; the side dock's tab pill on `.motion(PILL)` |
//! | `pulse.rs` shared 30 Hz loop clock | `El::looping` / `El::cycle` (one render clock, frames only while visible) |
//! | `smooth_scroll.rs` wheel glide | ported: Rune scrolls a wheel 1:1, so `design::smooth_scroll` glides notches per viewport (`.wheel_glide(key)`) |
//!
//! Reduced motion (the battery saver, `prefs.reduce_motion`) is one switch
//! for all of it: `look::apply_motion` sets `Cx::reduced_motion`, under which
//! travel snaps, loops hold their resting pose and colours take a short fade.

use rune::{Color, Easing, Spring, SpringPair, Transition};

// ---- Curves -------------------------------------------------------------

/// Tailwind's `transition-colors` curve.
pub const TAILWIND: Easing = Easing::cubic(0.4, 0., 0.2, 1.);

// ---- Zeron's transcript timings (crates/ui/src/transcript.rs, MIT) -------

/// The working glyph's wave period.
pub const GRADIENT_SPIN_MS: u64 = 750;
/// Segmented-control highlight slide: Zeron's 150 ms tab slide.
pub const TAB_SLIDE: Spring = Spring::new(0.15, 0.0);
// ---- Springs replacing GPUI's per-frame helpers ------------------------

/// A resorted row settling in its new place (GPUI `flip::ROW_SECS`).
pub const ROW_RESORT: Spring = Spring::new(0.32, 0.0);
/// A selection pill moving between tabs or rows (GPUI `flip::PILL_SECS`).
pub const PILL: Spring = Spring::new(0.30, 0.0);
/// The composer reaching its dock (GPUI `dock::DOCK_SECS`, critically damped).
pub const DOCK: Spring = Spring::new(0.42, 0.0);
/// `El::matched` id both composers carry, so the new-thread composer flies
/// to the chat's dock on the first message and back (GPUI `dock.rs`). Put
/// `.matched(COMPOSER_MATCH).motion(DOCK)` on the hero and docked composer
/// cards; Rune animates frame, radius and fill between them.
pub const COMPOSER_MATCH: &str = "composer";

// ---- Presence transitions (GPUI `fade_in` / `pop_in` / `collapse`) ------

/// Popover and menu entrance: fades in while travelling `from` points
/// toward its resting place (negative starts above it), on the style's
/// pop spring (a hair under critical, settles without a wobble).
pub fn pop(from: f32) -> Transition {
    Transition::opacity()
        .combined(Transition::offset(0., from))
        .springs(SpringPair::with_exit_ratio(super::styles::POP, 0.6))
}

// ---- The working glyph and vocabulary -----------------------------------

/// Gradient-spin cell opacity at local phase `t`: full at the cycle start,
/// down to `dim` by 45 %, resting there until 92 %, then back to full.
pub fn gspin_opacity(t: f32, dim: f32) -> f32 {
    let t = t.rem_euclid(1.);
    if t < 0.45 {
        lerp(1., dim, t / 0.45)
    } else if t < 0.92 {
        dim
    } else {
        lerp(dim, 1., (t - 0.92) / 0.08)
    }
}

/// [`gspin_opacity`] at dim 0.1 as `rune::Cycle` stops.
pub const GSPIN_STOPS: &[(f32, f32)] = &[(0., 1.), (0.45, 0.1), (0.92, 0.1), (1., 1.)];

pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// Blend two colours the way a browser transitions them: premultiplied
/// alpha, so a wash fading in from transparent brightens without passing
/// through grey. For static blends in a view (a hover-mixed ink); animated
/// colours are Rune's job.
pub fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0., 1.);
    if t <= 0. {
        return from;
    }
    if t >= 1. {
        return to;
    }
    let a = lerp(from.a, to.a, t);
    if a <= f32::EPSILON {
        return to.alpha(0.);
    }
    let ch = |f: f32, g: f32| lerp(f * from.a, g * to.a, t) / a;
    Color {
        r: ch(from.r, to.r),
        g: ch(from.g, to.g),
        b: ch(from.b, to.b),
        a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gspin_stops_match_the_curve() {
        for &(t, v) in GSPIN_STOPS {
            assert!((gspin_opacity(t.min(0.9999), 0.1) - v).abs() < 0.02 || t == 1.);
        }
    }

    #[test]
    fn a_wash_fading_in_keeps_its_colour() {
        let w = Color::WHITE;
        let m = mix(w.alpha(0.), w, 0.5);
        assert!((m.r - 1.).abs() < 1e-5 && (m.a - 0.5).abs() < 1e-5);
    }
}
