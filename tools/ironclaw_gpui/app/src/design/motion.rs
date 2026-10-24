//! Motion catalog adapted from Zeron's `motion.rs` (MIT): CSS timing curves
//! evaluated exactly, springs, and two wall-clock tween stores (hover fades
//! and state morphs) that make interaction feel continuous.
//!
//! Entrances go through gpui's `with_animation`, which honors
//! `App::reduce_motion`: the battery saver flips that flag and they snap to
//! their end state without scheduling a frame. The tween stores read the
//! same flag and snap too.
//!
//! gpui's `.hover()` style swaps colors the frame the pointer enters. The
//! stores below replace that with a short fade: an element reports hover
//! through [`hover_listener`], reads its progress with [`hover_t`], and the
//! window root calls [`pump`] once per frame so frames keep coming only
//! while something is mid-flight. Idle windows schedule nothing.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::{
    Animation, AnimationExt, App, ElementId, Hsla, IntoElement, Rgba, SharedString, Styled, Window,
    px,
};

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

    /// Eased output for progress `x` in 0..1, clamped against f32 rounding.
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
pub const EASE_OUT_QUINT: CubicBezier = CubicBezier::new(0.22, 1., 0.36, 1.);
/// CSS `ease`.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.);
/// CSS `ease-out`.
pub const EASE_OUT: CubicBezier = CubicBezier::new(0., 0., 0.58, 1.);
/// CSS `ease-in-out`.
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0., 0.58, 1.);

// Zeron's transcript timings (crates/ui/src/transcript.rs, MIT).
/// Zeron's quick fade: hover reveals of secondary controls.
pub const FADE_QUICK: MotionSpec = MotionSpec::new(150, EASE);
/// A tool group opening or closing.
pub const TOOL_FOLD: MotionSpec = MotionSpec::new(140, EASE_OUT);
/// A new activity row growing into the tree.
pub const TOOL_ROW_REVEAL: MotionSpec = MotionSpec::new(360, EASE_OUT_EXPO);
/// The working glyph's wave period.
pub const GRADIENT_SPIN_MS: u64 = 750;
/// A word rolling into the place of another.
/// Rail-tick / jump-to-latest glide: Zeron's 500 ms ease-in-out.
pub const SCROLL_GLIDE: MotionSpec = MotionSpec::new(500, EASE_IN_OUT);
/// How far the rows travel as they settle after a jump to latest.
pub const JUMP_TRAVEL: f32 = 48.;
/// Disclosure height morph (folds, new sidebar rows): Zeron's 180 ms.
pub const COLLAPSE: MotionSpec = MotionSpec::new(180, EASE_OUT);
/// Segmented-control highlight slide: Zeron's 150 ms tab slide.
pub const TAB_SLIDE: MotionSpec = MotionSpec::new(150, EASE_OUT);
pub const ROLL: MotionSpec = MotionSpec::new(550, EASE_OUT_QUINT);

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

#[derive(Clone, Copy)]
pub struct MotionSpec {
    pub duration_ms: u64,
    pub curve: CubicBezier,
}

impl MotionSpec {
    pub const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self { duration_ms, curve }
    }

    pub fn duration(self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    pub fn animation(self) -> Animation {
        Animation::new(self.duration()).with_easing(move |d| self.curve.eval(d))
    }
}

/// A damped spring squeezed into a fixed duration. `damping` 1 is critical
/// (no overshoot); below 1 it overshoots and swings back once or twice.
/// Its output leaves 0..1 mid-flight, so callers clamp opacity.
#[derive(Clone, Copy)]
pub struct Spring {
    pub damping: f32,
    pub duration_ms: u64,
}

impl Spring {
    pub const fn new(damping: f32, duration_ms: u64) -> Self {
        Self {
            damping,
            duration_ms,
        }
    }

    pub fn eval(self, t: f32) -> f32 {
        if t >= 1. {
            return 1.;
        }
        if t <= 0. {
            return 0.;
        }
        let z = self.damping.clamp(0.05, 1.);
        // The envelope reaches e^-7 (0.1 %) at t = 1.
        let decay = 7.;
        if z >= 0.999 {
            let w = decay;
            return 1. - (-w * t).exp() * (1. + w * t);
        }
        let w = decay / z;
        let wd = w * (1. - z * z).sqrt();
        1. - (-z * w * t).exp() * ((wd * t).cos() + (z * w / wd) * (wd * t).sin())
    }

    pub fn animation(self) -> Animation {
        Animation::new(Duration::from_millis(self.duration_ms)).with_easing(move |d| self.eval(d))
    }
}

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

/// [`fade_in`] that waits `delay_ms` first: list rows use it to arrive in
/// a short cascade, one orchestrated moment instead of a jump.
pub fn fade_in_after<E>(
    element: E,
    id: impl Into<ElementId>,
    spec: MotionSpec,
    rise: f32,
    delay_ms: u64,
) -> impl IntoElement
where
    E: IntoElement + Styled + 'static,
{
    let total = (spec.duration_ms + delay_ms).max(1) as f32;
    let lead = delay_ms as f32 / total;
    let animation = Animation::new(Duration::from_millis(spec.duration_ms + delay_ms))
        .with_easing(move |t| spec.curve.eval(((t - lead) / (1. - lead)).clamp(0., 1.)));
    element
        .relative()
        .with_animation(id, animation, move |el, d| {
            el.opacity(d).top(px(rise * (1. - d)))
        })
}

/// Popover and menu entrance on a spring: fades in while travelling `from`
/// px toward its resting place (negative starts above it).
pub fn pop_in<E>(
    element: E,
    id: impl Into<ElementId>,
    spring: Spring,
    from: f32,
) -> impl IntoElement
where
    E: IntoElement + Styled + 'static,
{
    element
        .relative()
        .with_animation(id, spring.animation(), move |el, d| {
            el.opacity((d * 1.6).clamp(0., 1.)).top(px(from * (1. - d)))
        })
}

pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// Blend two colors the way a browser transitions them: sRGB with
/// premultiplied alpha, so a wash fading in from transparent brightens
/// without passing through grey.
pub fn mix(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0., 1.);
    if t <= 0. {
        return from;
    }
    if t >= 1. {
        return to;
    }
    let (f, g) = (Rgba::from(from), Rgba::from(to));
    let a = lerp(f.a, g.a, t);
    if a <= f32::EPSILON {
        return Hsla::from(Rgba { a: 0., ..g });
    }
    Hsla::from(Rgba {
        r: lerp(f.r * f.a, g.r * g.a, t) / a,
        g: lerp(f.g * f.a, g.g * g.a, t) / a,
        b: lerp(f.b * f.a, g.b * g.a, t) / a,
        a,
    })
}

// Wall-clock tween stores. Main-thread `thread_local`s, so free element
// builders can blend colors without threading `cx` through. An element
// that unmounts mid-hover never sends its leave event, so every read stamps
// the entry with the frame counter and [`pump`] drops entries that went a
// whole frame unread.

#[derive(Clone, Copy)]
struct Tween {
    origin: f32,
    target: f32,
    started: Instant,
    duration: Duration,
    curve: CubicBezier,
    seen: u64,
}

impl Tween {
    fn value(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.target;
        }
        let raw = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        lerp(self.origin, self.target, self.curve.eval(raw))
    }

    fn settled(&self, now: Instant) -> bool {
        self.origin == self.target || now.saturating_duration_since(self.started) >= self.duration
    }
}

#[derive(Default)]
struct Store {
    hovers: HashMap<SharedString, Tween>,
    states: HashMap<SharedString, Tween>,
    frame: u64,
    /// A reveal or roll drew a mid-flight frame and needs the next one.
    pending_frame: bool,
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

/// Record a hover flip for `key`. Reduced motion snaps.
pub fn set_hover(key: &SharedString, hovered: bool, spec: MotionSpec, reduced: bool) {
    let now = Instant::now();
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let target = if hovered { 1. } else { 0. };
        let current = s.hovers.get(key).map(|t| t.value(now));
        if !hovered && current.is_none() {
            return;
        }
        let seen = s.frame;
        s.hovers.insert(
            key.clone(),
            Tween {
                origin: if reduced {
                    target
                } else {
                    current.unwrap_or(0.)
                },
                target,
                started: now,
                duration: spec.duration(),
                curve: spec.curve,
                seen,
            },
        );
    });
}

/// Hover progress (0..1) for `key` this frame.
pub fn hover_t(key: &str) -> f32 {
    let now = Instant::now();
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s.frame;
        match s.hovers.get_mut(key) {
            Some(t) => {
                t.seen = frame;
                t.value(now)
            }
            None => 0.,
        }
    })
}

/// An `.on_hover` listener driving the fade for `key`; pair it with
/// [`hover_t`] reads of the same key.
pub fn hover_listener(
    key: SharedString,
    spec: MotionSpec,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    move |hovered, window, cx| {
        set_hover(&key, *hovered, spec, cx.reduce_motion());
        // Event context: `request_animation_frame` is draw-only, so mark the
        // window dirty; the root's `pump` keeps frames coming from there.
        window.refresh();
    }
}

/// Progress (0 off, 1 on) of `key` toward `on`. The first read snaps to the
/// state, so mounting never replays a morph; a flip mid-flight reverses
/// from the drawn value.
pub fn state_t(key: &str, on: bool, spec: MotionSpec, reduced: bool) -> f32 {
    let now = Instant::now();
    let target = if on { 1. } else { 0. };
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s.frame;
        let entry = s.states.entry(key.to_string().into()).or_insert(Tween {
            origin: target,
            target,
            started: now,
            duration: spec.duration(),
            curve: spec.curve,
            seen: frame,
        });
        if entry.target != target {
            let current = entry.value(now);
            *entry = Tween {
                origin: if reduced { target } else { current },
                target,
                started: now,
                duration: spec.duration(),
                curve: spec.curve,
                seen: frame,
            };
        }
        entry.seen = frame;
        entry.value(now)
    })
}

/// Like [`state_t`] for a continuous value (a slot index, a width): glides
/// from the drawn value to `to` whenever `to` changes.
pub fn glide(key: &str, to: f32, spec: MotionSpec, reduced: bool) -> f32 {
    let now = Instant::now();
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s.frame;
        let entry = s.states.entry(key.to_string().into()).or_insert(Tween {
            origin: to,
            target: to,
            started: now,
            duration: spec.duration(),
            curve: spec.curve,
            seen: frame,
        });
        if entry.target != to {
            let current = entry.value(now);
            *entry = Tween {
                origin: if reduced { to } else { current },
                target: to,
                started: now,
                duration: spec.duration(),
                curve: spec.curve,
                seen: frame,
            };
        }
        entry.seen = frame;
        entry.value(now)
    })
}

/// When `key` was first drawn. Rows that arrive while the reader watches
/// animate in; rows that were already there (a reloaded history, a row
/// scrolled back into view) are older than one reveal and draw settled.
pub fn born(key: &str) -> Instant {
    BORN.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() > 4_096 {
            b.clear();
        }
        *b.entry(key.to_string().into()).or_insert_with(Instant::now)
    })
}

/// Eased 0..1 progress of a reveal that started at `born(key)` after
/// `delay_ms`; 1 when motion is reduced.
pub fn reveal(key: &str, spec: MotionSpec, delay_ms: u64, reduced: bool) -> f32 {
    if reduced {
        return 1.;
    }
    let elapsed = born(key).elapsed().as_millis() as u64;
    let raw = elapsed.saturating_sub(delay_ms) as f32 / spec.duration_ms.max(1) as f32;
    let t = spec.curve.eval(raw);
    if t < 1. {
        STORE.with(|s| s.borrow_mut().pending_frame = true);
    }
    t
}

/// Mark `key` as born long ago, so it draws settled from its first frame.
pub fn settle(key: &str) {
    let past = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    BORN.with(|b| {
        b.borrow_mut().entry(key.to_string().into()).or_insert(past);
    });
}

/// A disclosure that morphs its height between closed and open over
/// `spec`, like Zeron's collapses. The content's natural height comes from
/// its last prepaint, so the morph lands on the real size; the first open
/// frame has none yet and draws at zero, which `t` near 0 hides anyway.
/// `None` once fully closed, so a shut fold builds nothing.
pub fn collapse(
    key: &str,
    open: bool,
    spec: MotionSpec,
    reduced: bool,
    content: impl FnOnce() -> gpui_kit::AnyElement,
) -> Option<gpui_kit::AnyElement> {
    let t = state_t(&format!("{key}:collapse"), open, spec, reduced);
    if t <= 0. {
        return None;
    }
    Some(clip(key, t, content()))
}

/// A row that grows from nothing to its measured height the first time
/// `key` is drawn, pushing its neighbours aside smoothly. Rows already
/// there ([`settle`]d) draw at full size.
pub fn grow(
    key: &str,
    spec: MotionSpec,
    reduced: bool,
    content: impl gpui_kit::IntoElement,
) -> gpui_kit::AnyElement {
    let t = reveal(key, spec, 0, reduced);
    clip(key, t, content)
}

/// Clip `content` to `t` of the height it measured last frame, recording
/// the new measurement as it prepaints.
fn clip(key: &str, t: f32, content: impl gpui_kit::IntoElement) -> gpui_kit::AnyElement {
    use gpui_kit::{IntoElement as _, ParentElement as _, div};
    let slot: SharedString = key.to_string().into();
    let wrap = div().w_full().on_children_prepainted(move |bounds, _, _| {
        if let Some(b) = bounds.first() {
            let h = f32::from(b.size.height);
            HEIGHTS.with(|m| {
                let mut m = m.borrow_mut();
                if m.len() > 4_096 {
                    m.clear();
                }
                m.insert(slot.clone(), h);
            });
        }
    });
    let inner = div().w_full().flex_none().child(content);
    if t >= 1. {
        return wrap.child(inner).into_any_element();
    }
    let natural = HEIGHTS.with(|m| m.borrow().get(key).copied()).unwrap_or(0.);
    wrap.h(px(natural * t))
        .overflow_hidden()
        .opacity(t.max(0.2))
        .child(inner)
        .into_any_element()
}

/// A label's current text and, mid-roll, the text it is leaving.
type RollState = (SharedString, Option<(SharedString, Instant)>);

thread_local! {
    static BORN: RefCell<HashMap<SharedString, Instant>> = RefCell::new(HashMap::new());
    static ROLLS: RefCell<HashMap<SharedString, RollState>> = RefCell::new(HashMap::new());
    static HEIGHTS: RefCell<HashMap<SharedString, f32>> = RefCell::new(HashMap::new());
}

/// A label that rolls when its text changes: the old text lifts away and
/// fades while the new one rises in from below, as Zeron's working word
/// does. Zeron rolls per glyph with a blur; this rolls the whole label,
/// which reads the same at 12 px and costs two text runs.
pub fn roll_text(
    key: impl Into<SharedString>,
    text: SharedString,
    reduced: bool,
) -> gpui_kit::AnyElement {
    use gpui_kit::{IntoElement as _, ParentElement as _, div};
    let key = key.into();
    let now = Instant::now();
    let roll = ROLLS.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() > 256 && !r.contains_key(&key) {
            r.retain(|_, (_, roll)| roll.is_some());
        }
        let entry = r.entry(key).or_insert_with(|| (text.clone(), None));
        if entry.0 != text {
            let from = std::mem::replace(&mut entry.0, text.clone());
            entry.1 = (!reduced).then_some((from, now));
        }
        if entry
            .1
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= ROLL.duration())
        {
            entry.1 = None;
        }
        entry.1.clone()
    });
    let Some((from, at)) = roll else {
        return div().child(text).into_any_element();
    };
    STORE.with(|s| s.borrow_mut().pending_frame = true);
    let t = ROLL
        .curve
        .eval(now.saturating_duration_since(at).as_secs_f32() / ROLL.duration().as_secs_f32());
    let travel = 5.;
    div()
        .relative()
        .child(
            div()
                .relative()
                .top(px(travel * (1. - t)))
                .opacity(t)
                .child(text),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(-travel * t))
                .opacity((1. - t * 1.6).max(0.))
                .whitespace_nowrap()
                .child(from),
        )
        .into_any_element()
}

/// Once per window frame, from the root view's render: prune unmounted
/// entries and ask for the next frame while any tween is mid-flight.
pub fn pump(window: &mut Window) {
    let now = Instant::now();
    let active = STORE.with(|s| {
        let mut s = s.borrow_mut();
        s.frame += 1;
        let frame = s.frame;
        let mut active = std::mem::take(&mut s.pending_frame);
        s.hovers.retain(|_, t| {
            if t.seen + 1 < frame {
                return false;
            }
            let settled = t.settled(now);
            active |= !settled;
            !(settled && t.target == 0.)
        });
        s.states.retain(|_, t| {
            if t.seen + 1 < frame {
                return false;
            }
            active |= !t.settled(now);
            true
        });
        active
    });
    if active {
        window.request_animation_frame();
    }
}

/// The window root's last child. It paints after every view has rendered,
/// so a reveal or roll that started anywhere this frame still gets its next
/// frame; an idle window asks for nothing.
pub fn frame_tail() -> impl IntoElement {
    gpui_kit::canvas(
        |_, _, _| (),
        |_, _, window, _| {
            let now = Instant::now();
            let busy = STORE.with(|s| {
                let s = s.borrow();
                s.pending_frame
                    || s.states.values().any(|t| !t.settled(now))
                    || s.hovers.values().any(|t| !t.settled(now))
            });
            if busy {
                window.request_animation_frame();
            }
        },
    )
    .absolute()
    .size_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_hit_their_endpoints_and_stay_monotonic() {
        for curve in [EASE_OUT_EXPO, EASE, EASE_OUT_QUINT, EASE_IN_OUT] {
            assert_eq!(curve.eval(0.), 0.);
            assert_eq!(curve.eval(1.), 1.);
            let mut last = 0.;
            for i in 1..100 {
                let y = curve.eval(i as f32 / 100.);
                assert!(y >= last);
                last = y;
            }
        }
        assert!(EASE_OUT_EXPO.eval(0.5) > 0.8);
    }

    #[test]
    fn springs_settle_and_only_underdamped_ones_overshoot() {
        let soft = Spring::new(0.6, 400);
        let firm = Spring::new(1., 400);
        assert_eq!(soft.eval(1.), 1.);
        assert!((soft.eval(0.98) - 1.).abs() < 0.01);
        assert!((0..100).any(|i| soft.eval(i as f32 / 100.) > 1.01));
        assert!((0..100).all(|i| firm.eval(i as f32 / 100.) <= 1.0001));
    }
}
