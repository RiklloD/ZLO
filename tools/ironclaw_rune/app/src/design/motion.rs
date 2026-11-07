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
//! | `fade_in`, `fade_in_after`, `pop_in` | `.transition(..)` presence ([`enter`], [`pop`]); nothing animates on the first frame |
//! | `born`, `reveal`, `arrive`, `settle`, `restart` | presence transitions and growth stacks (`El::growth`) |
//! | `collapse`, `grow` (height morphs) | presence with [`fold`], or a growth stack |
//! | `roll_text` | `rune_kit::RollingText` ([`roll_text`]) |
//! | `pump`, `frame_tail` (keep frames coming) | Rune's clock requests frames only while a track moves |
//! | `dock.rs` composer hand-off | `El::matched(COMPOSER_MATCH)` on both composers ([`COMPOSER_MATCH`]) |
//! | `flip.rs` resorted rows / sliding pills | keyed inbox rows on `.motion(ROW_RESORT)`; the side dock's tab pill on `.motion(PILL)` |
//! | `pulse.rs` shared 30 Hz loop clock | `El::looping` / `El::cycle` (one render clock, frames only while visible) |
//! | `smooth_scroll.rs` wheel glide | ported: Rune scrolls a wheel 1:1, so `design::smooth_scroll` glides notches per viewport (`.wheel_glide(key)`) |
//!
//! Reduced motion (the battery saver, `prefs.reduce_motion`) is one switch
//! for all of it: `look::apply_motion` sets `Cx::reduced_motion`, under which
//! travel snaps, loops hold their resting pose and colours take a short fade.

use rune::{Color, Curve, Easing, Edge, Spring, SpringPair, Transition};

// ---- Curves -------------------------------------------------------------

/// Zeron's entrance curve, `cubic-bezier(0.16, 1, 0.3, 1)`.
pub const EASE_OUT_EXPO: Easing = Easing::cubic(0.16, 1., 0.3, 1.);
pub const EASE_OUT_QUINT: Easing = Easing::cubic(0.22, 1., 0.36, 1.);
/// CSS `ease`.
pub const EASE: Easing = Easing::EASE;
/// CSS `ease-out`.
pub const EASE_OUT: Easing = Easing::EASE_OUT;
/// CSS `ease-in-out`.
pub const EASE_IN_OUT: Easing = Easing::EASE_IN_OUT;
/// Tailwind's `transition-colors` curve.
pub const TAILWIND: Easing = Easing::cubic(0.4, 0., 0.2, 1.);

// ---- Zeron's transcript timings (crates/ui/src/transcript.rs, MIT) -------

/// Zeron's quick fade: hover reveals of secondary controls.
pub const FADE_QUICK: Curve = Curve::tween(0.150, EASE);
/// A tool group opening or closing.
pub const TOOL_FOLD: Curve = Curve::tween(0.140, EASE_OUT);
/// A new activity row growing into the tree.
pub const TOOL_ROW_REVEAL: Curve = Curve::tween(0.360, EASE_OUT_EXPO);
/// The tree's connector draws briskly, then eases into the branch tip.
pub const TOOL_CONNECTOR_REVEAL: Curve = Curve::tween(0.480, EASE_OUT_QUINT);
/// Rows arriving together cascade: a new group waits 90 ms for its header,
/// then each row starts 65 ms after the one above (use `rune::Stagger`).
pub const TOOL_FIRST_ROW_DELAY_MS: u64 = 90;
pub const TOOL_ROW_STAGGER_MS: u64 = 65;
/// The working glyph's wave period.
pub const GRADIENT_SPIN_MS: u64 = 750;
/// Rail-tick / jump-to-latest glide: Zeron's 500 ms ease-in-out. Pass it
/// as a spring to `Tx::scroll_to_with` via [`SCROLL_GLIDE_SPRING`].
pub const SCROLL_GLIDE: Curve = Curve::tween(0.500, EASE_IN_OUT);
/// [`SCROLL_GLIDE`] as the critically damped spring scroll requests take.
pub const SCROLL_GLIDE_SPRING: Spring = Spring::new(0.5, 0.0);
/// How far the rows travel as they settle after a jump to latest.
pub const JUMP_TRAVEL: f32 = 48.;
/// Disclosure height morph (folds, new sidebar rows): Zeron's 180 ms.
pub const COLLAPSE: Curve = Curve::tween(0.180, EASE_OUT);
/// Segmented-control highlight slide: Zeron's 150 ms tab slide.
pub const TAB_SLIDE: Spring = Spring::new(0.15, 0.0);
/// A word rolling into the place of another.
pub const ROLL: Spring = Spring::new(0.55, 0.0);

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

/// Page and thread entrances: fade in while rising `rise` points (GPUI
/// `Style::enter` + `enter_rise`, 460 ms ease-out-expo, as a spring).
pub fn enter(rise: f32) -> Transition {
    Transition::opacity()
        .combined(Transition::offset(0., rise))
        .springs(SpringPair::with_exit_ratio(Spring::new(0.46, 0.0), 0.5))
}

/// Popover and menu entrance: fades in while travelling `from` points
/// toward its resting place (negative starts above it), on the style's
/// pop spring (a hair under critical, settles without a wobble).
pub fn pop(from: f32) -> Transition {
    Transition::opacity()
        .combined(Transition::offset(0., from))
        .springs(SpringPair::with_exit_ratio(super::styles::POP, 0.6))
}

/// A disclosure opening downward from its top edge and folding back up
/// (GPUI `collapse`): put it on the content shown while open; removing the
/// content plays the exit.
pub fn fold() -> Transition {
    Transition::reveal(Edge::Top).springs(SpringPair::with_exit_ratio(Spring::new(0.18, 0.0), 1.0))
}

/// A label that rolls its changed characters (GPUI `roll_text`).
pub fn roll_text(
    key: impl Into<String>,
    text: impl Into<String>,
    font: rune::tok::TextStyle,
    color: Color,
    k: &rune_kit::Kit,
) -> rune::El {
    rune_kit::RollingText::new(key, text)
        .font(font)
        .color(color)
        .spring(ROLL)
        .build(k)
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

/// Zeron's rotating "working" vocabulary: 21 words, one every 7 s, seeded
/// per thread so two threads never chant in step.
pub const FLAVOUR_WORDS: [&str; 21] = [
    "Clawing",
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];
pub const FLAVOUR_ROTATE_SECS: u64 = 7;

pub fn flavour_word(seed: u64, elapsed_secs: u64) -> &'static str {
    let step = elapsed_secs / FLAVOUR_ROTATE_SECS;
    FLAVOUR_WORDS[(seed.wrapping_add(step) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// FNV-1a, a stable seed from a thread id.
pub fn seed(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// "12s", "3m 4s", "1h 2m": at most two units.
pub fn format_elapsed(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3_600, (secs % 3_600) / 60),
    }
}

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
    fn elapsed_labels_use_two_units() {
        assert_eq!(format_elapsed(12), "12s");
        assert_eq!(format_elapsed(184), "3m 4s");
        assert_eq!(format_elapsed(3_720), "1h 2m");
    }

    #[test]
    fn a_wash_fading_in_keeps_its_colour() {
        let w = Color::WHITE;
        let m = mix(w.alpha(0.), w, 0.5);
        assert!((m.r - 1.).abs() < 1e-5 && (m.a - 0.5).abs() < 1e-5);
    }
}
