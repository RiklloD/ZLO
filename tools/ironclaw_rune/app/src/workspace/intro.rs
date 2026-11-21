//! The launch intro: ZLO's wordmark assembling in the middle of the window
//! while the gateway boots, then irising open onto the workspace.
//!
//! It plays once per launch (`main` arms it; [`Intro::default`] is spent, so
//! tests, headless scenes and later reconnects never see it):
//!
//! 1. **Converge** (0–0.55 s): sixty-four hairlines rush in from past the
//!    window's edges and gather on a ring around the centre, which draws
//!    itself on as they land.
//! 2. **Assemble** (0.38–1.0 s): Z, L and O trace on as thick monoline
//!    strokes, flying in from wide apart and settling from 135 % with a
//!    spring's overshoot, while the ring bursts outward and fades.
//! 3. **Tick** (0.98–1.15 s): the accent's one appearance, a small square
//!    full stop after the O.
//! 4. **Reveal** (0.42 s, from 1.15 s or whenever the connection settles,
//!    whichever is later): the stage irises open from the centre onto the
//!    workspace, which settles from 103 %, while the wordmark shrinks and
//!    flies onto the sidebar's "ZLO" title, fading into it as it lands.
//!
//! A slow start holds the assembled mark still (the tick breathes on the
//! small loaders' 15 Hz clock) with "Starting ZLO" fading in late; nothing
//! restarts. Reduced motion is a 0.2 s fade in and a 0.25 s fade out.
//!
//! Everything is drawn from the elapsed time `t` alone ([`stage`]), in
//! window points on two canvases, so a frame is a pure function of `t`:
//! [`poll`] steps `t` from the frame clock and asks for the next frame only
//! while something moves (`Tx::wake_after(ZERO)`, the display's rate),
//! and the `launch-*` scenes freeze it at chosen times. Flat by rule: the
//! palette's text colour on the shell, no gradients or glows; spectacle
//! comes from scale, geometry and timing.

use std::cell::Cell;
use std::f32::consts::{PI, TAU};
use std::time::Duration;

use rune::{Anim, Color, Cycle, El, Shape, Tx, canvas, col, el, row};

use super::ui::{SM, t as label};
use crate::design::Palette;
use crate::state::{Conn, State};

/// Assembly ends: the settled mark, the tick in.
pub const ASSEMBLE: f64 = 1.15;
/// The iris and the flight to the title.
pub const REVEAL: f64 = 0.42;
// The whole intro stays under 1.8 s.
const _: () = assert!(ASSEMBLE + REVEAL <= 1.8);

/// Reduced motion: the settled mark fades in…
pub const FADE_IN: f64 = 0.2;
/// …and the stage fades out.
pub const FADE_OUT: f64 = 0.25;

/// The window before the first layout reports it (the default window).
const DEFAULT_SIZE: [f32; 2] = [1280., 820.];

/// The launch intro's clock and phase.
#[derive(Clone, Debug)]
pub struct Intro {
    /// Frame clock at the first poll.
    start: Option<f64>,
    /// Seconds since `start`, as the view draws them.
    pub t: f64,
    /// When the reveal starts (on `t`): set once the connection settles,
    /// never before the assembly has played.
    reveal_at: Option<f64>,
    /// Played out (or never armed): the shell draws no intro.
    pub done: bool,
    /// Reduced motion, as the view last saw it (`Cx::is_reduced_motion`).
    pub reduced: Cell<bool>,
    /// A scene's fixed frame: the poll leaves it be.
    frozen: bool,
    /// The stage's laid-out size in points.
    pub size: [f32; 2],
}

impl Default for Intro {
    /// Spent: nothing plays.
    fn default() -> Self {
        Self {
            start: None,
            t: 0.,
            reveal_at: None,
            done: true,
            reduced: Cell::new(false),
            frozen: false,
            size: DEFAULT_SIZE,
        }
    }
}

impl Intro {
    /// Armed for this launch: plays from the first poll.
    pub fn launch() -> Self {
        Self {
            done: false,
            ..Self::default()
        }
    }

    /// A scene's frame at `t`, the reveal starting at `reveal_at`.
    pub fn frozen(t: f64, reveal_at: Option<f64>, reduced: bool) -> Self {
        Self {
            t,
            reveal_at,
            done: false,
            reduced: Cell::new(reduced),
            frozen: true,
            start: Some(0.),
            size: DEFAULT_SIZE,
        }
    }

    /// The shell draws the intro.
    pub fn playing(&self) -> bool {
        !self.done
    }

    /// The assembly's length for the current motion setting.
    fn settle(&self) -> f64 {
        if self.reduced.get() {
            FADE_IN
        } else {
            ASSEMBLE
        }
    }

    fn reveal_len(&self) -> f64 {
        if self.reduced.get() { FADE_OUT } else { REVEAL }
    }

    /// How far the reveal has got, 0 to 1 (0 before it starts).
    fn reveal(&self) -> f32 {
        self.reveal_at.map_or(0., |at| {
            ((self.t - at) / self.reveal_len()).clamp(0., 1.) as f32
        })
    }

    /// Assembled, and waiting on the connection.
    pub fn waiting(&self) -> bool {
        !self.done && self.reveal_at.is_none() && self.t >= self.settle()
    }

    /// The stage still hides the whole window (before the reveal): the
    /// shell mounts nothing under it and takes no input, so nothing
    /// concealed can be seen through it or pressed.
    pub fn covers(&self) -> bool {
        self.playing() && !self.revealing()
    }

    /// The reveal has started.
    pub fn revealing(&self) -> bool {
        !self.done && self.reveal_at.is_some_and(|at| self.t >= at)
    }

    /// Step the clock to `clock`. `connecting` holds the reveal back.
    /// Returns whether the view changed and whether to draw the next frame.
    fn step(&mut self, clock: f64, connecting: bool) -> (bool, bool) {
        if self.done || self.frozen {
            return (false, false);
        }
        let start = *self.start.get_or_insert(clock);
        let before = self.t;
        self.t = (clock - start).max(0.);
        if self.reveal_at.is_none() && !connecting {
            // Fast connects still see the whole assembly; a slow one
            // reveals as soon as it lands.
            self.reveal_at = Some(self.t.max(self.settle()));
        }
        if let Some(at) = self.reveal_at
            && self.t >= at + self.reveal_len()
        {
            self.done = true;
            return (true, false);
        }
        let moving = self.t < self.settle() || self.reveal_at.is_some();
        // While waiting the mark is still: no frames until the gateway
        // answers (its reply wakes the app).
        (moving || before < self.settle(), moving)
    }
}

/// Step the intro from the frame clock (`main::poll`).
pub fn poll(s: &mut State, clock: f64, tx: &mut Tx) -> bool {
    let connecting = matches!(s.conn, Conn::Connecting);
    let (changed, next) = s.intro.step(clock, connecting);
    if next {
        tx.wake_after(Duration::ZERO);
    }
    changed
}

// ---- easing -------------------------------------------------------------

fn clamp01(x: f32) -> f32 {
    x.clamp(0., 1.)
}

/// `x` from `a` to `b` as 0 to 1.
fn span(x: f32, a: f32, b: f32) -> f32 {
    clamp01((x - a) / (b - a))
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn out_cubic(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

fn out_quart(t: f32) -> f32 {
    1. - (1. - t).powi(4)
}

fn out_expo(t: f32) -> f32 {
    if t >= 1. {
        1.
    } else {
        1. - 2f32.powf(-10. * t)
    }
}

fn in_quad(t: f32) -> f32 {
    t * t
}

fn in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4. * t * t * t
    } else {
        1. - (-2. * t + 2.).powi(3) / 2.
    }
}

/// A damped spring from `1 + amp` to 1: overshoots below 1 once, then
/// rests (about 0.8 s).
fn spring(tau: f32, amp: f32) -> f32 {
    if tau <= 0. {
        return 1. + amp;
    }
    1. + amp * (-tau / 0.13).exp() * (tau * TAU / 0.6).cos()
}

/// A small integer hash to 0..1, for the rays' fixed jitter.
fn jitter(i: u32, salt: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ salt.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.
}

// ---- geometry -----------------------------------------------------------

/// Stroke width as a share of the letter height.
const WEIGHT: f32 = 0.15;
/// Letter widths and the gap between them, in letter heights.
const Z_W: f32 = 0.74;
const L_W: f32 = 0.56;
const O_W: f32 = 1.0;
const GAP: f32 = 0.2;
/// The wordmark's width in letter heights (the tick hangs past it).
const WIDTH: f32 = Z_W + GAP + L_W + GAP + O_W;

/// The three letters' centre lines in letter heights, from the
/// wordmark's top-left corner; each one traces from its first point.
fn letters() -> [Vec<[f32; 2]>; 3] {
    let s = WEIGHT / 2.;
    let z = vec![[s, s], [Z_W - s, s], [s, 1. - s], [Z_W - s, 1. - s]];
    let lx = Z_W + GAP;
    let l = vec![[lx + s, s], [lx + s, 1. - s], [lx + L_W - s, 1. - s]];
    let ox = lx + L_W + GAP + O_W / 2.;
    let r = O_W / 2. - s;
    let o = (0..=96)
        .map(|i| {
            let a = -PI / 2. + i as f32 / 96. * TAU;
            [ox + r * a.cos(), 0.5 + r * a.sin()]
        })
        .collect();
    [z, l, o]
}

/// The part of the polyline `pts` up to `frac` of its length.
fn trim(pts: &[[f32; 2]], frac: f32) -> Vec<[f32; 2]> {
    if frac >= 1. {
        return pts.to_vec();
    }
    let seg = |a: [f32; 2], b: [f32; 2]| ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
    let total: f32 = pts.windows(2).map(|w| seg(w[0], w[1])).sum();
    let mut left = total * frac.max(0.);
    let mut out = vec![pts[0]];
    for w in pts.windows(2) {
        let d = seg(w[0], w[1]);
        if left >= d {
            out.push(w[1]);
            left -= d;
            continue;
        }
        if left > 0. && d > 0. {
            let k = left / d;
            out.push([lerp(w[0][0], w[1][0], k), lerp(w[0][1], w[1][1], k)]);
        }
        break;
    }
    out
}

fn circle(c: [f32; 2], r: f32, from: f32, frac: f32) -> Vec<[f32; 2]> {
    let n = 160;
    (0..=n)
        .map(|i| {
            let a = from + i as f32 / n as f32 * TAU * frac;
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        })
        .collect()
}

/// Where the sidebar's "ZLO" title sits, and the height its capitals
/// stand at: the wordmark lands there. `None` with no title on screen.
fn title_target(s: &State) -> Option<([f32; 2], f32)> {
    let shown =
        matches!(s.conn, Conn::Ready) && s.workspace.sidebar.is_open() && !super::in_settings(s);
    // `shell::sidebar`'s title: past the traffic lights and the sidebar
    // toggle, centred in the titlebar band, 14 pt semibold.
    let x = crate::design::TRAFFIC_LIGHTS_WIDTH + crate::design::panels::TOGGLE_SIZE + 10.;
    let band = rune_kit::shell::TOOLBAR_HEIGHT;
    let cap = SM * 0.72;
    shown.then_some(([x + WIDTH * cap / 2., band / 2.], cap))
}

/// One frame of the intro: what [`stage`] draws.
struct Pose {
    /// The wordmark's centre and letter height.
    centre: [f32; 2],
    height: f32,
    /// Each letter's drawn share and horizontal offset (letter heights).
    draw: [f32; 3],
    shift: [f32; 3],
    /// The wordmark's opacity.
    mark: f32,
    /// The tick's scale (0 = hidden) and opacity.
    tick: f32,
    tick_alpha: f32,
}

fn pose(i: &Intro, size: [f32; 2], target: Option<([f32; 2], f32)>) -> Pose {
    let t = i.t as f32;
    let [w, h] = size;
    let base = (w.min(h) * 0.2).clamp(72., 210.);
    let mid = [w / 2., h / 2.];
    let u = i.reveal();
    if i.reduced.get() {
        let fade = span(t, 0., FADE_IN as f32) * (1. - u);
        return Pose {
            centre: mid,
            height: base,
            draw: [1.; 3],
            shift: [0.; 3],
            mark: fade,
            tick: 1.,
            tick_alpha: fade,
        };
    }
    // Letters trace on in turn, flying in from wide apart.
    let starts = [0.38, 0.47, 0.55];
    let spread = [-0.9, 0., 0.9];
    let mut draw = [0.; 3];
    let mut shift = [0.; 3];
    for k in 0..3 {
        let p = span(t, starts[k], starts[k] + 0.42);
        draw[k] = out_quart(p);
        shift[k] = spread[k] * (1. - out_expo(span(t, starts[k], starts[k] + 0.5)));
    }
    let settle = spring(t - 0.38, 0.35);
    // The reveal: shrink and fly onto the title, or away into the centre.
    let fly = in_out_cubic(u);
    let (centre, height, mark) = match target {
        Some((to, cap)) => (
            [lerp(mid[0], to[0], fly), lerp(mid[1], to[1], fly)],
            // Geometric, so the shrink reads even all the way down.
            base * settle * (cap / (base * settle)).powf(fly),
            1. - span(u, 0.72, 1.),
        ),
        None => (
            mid,
            base * settle * lerp(1., 0.6, fly),
            1. - span(u, 0.1, 0.7),
        ),
    };
    let pop = span(t, 0.98, 1.15);
    // Pops past full size and back.
    let tick = if pop <= 0. {
        0.
    } else {
        out_cubic(span(pop, 0., 0.55)) + 0.35 * (pop * PI).sin()
    };
    Pose {
        centre,
        height,
        draw,
        shift,
        mark: mark.clamp(0., 1.),
        tick,
        tick_alpha: 1. - span(u, 0., 0.35),
    }
}

/// The backdrop canvas: the iris during the reveal, the converging rays
/// and the ring. Window points.
fn backdrop(i: &Intro, p: &Palette, shell: Color, size: [f32; 2], base: f32) -> Shape {
    let t = i.t as f32;
    let [w, h] = size;
    let mid = [w / 2., h / 2.];
    let far = (w * w + h * h).sqrt() / 2.;
    let mut shape = Shape::new();
    // The stage itself only matters once the workspace is behind it: a
    // disc of the shell's fill over everything, its hole growing from
    // the centre (an annulus as one wide stroke).
    if i.revealing() {
        let u = i.reveal();
        if i.reduced.get() {
            // The fade is the view's opacity.
            shape = shape.fill(vec![[0., 0.], [w, 0.], [w, h], [0., h]], shell);
        } else {
            let outer = far + 4.;
            let inner = outer * in_out_cubic(u).powf(1.15);
            if inner < outer {
                shape = annulus(shape, mid, inner, outer, shell);
                if inner > 0.5 {
                    // The iris's rim: one hairline riding the opening.
                    shape = shape.stroke_with(
                        circle(mid, inner, 0., 1.),
                        true,
                        1.,
                        rune::LineCap::Butt,
                        // Only once it has cleared the wordmark.
                        ink(p, 0.3 * (1. - u) * span(u, 0.18, 0.38)),
                    );
                }
            }
        }
    }
    if i.reduced.get() {
        return shape;
    }
    // Hairlines rushing in from past the edges, landing on the ring.
    let ring = base * WIDTH * 0.62;
    const RAYS: u32 = 64;
    for k in 0..RAYS {
        let delay = jitter(k, 1) * 0.12;
        let q = span(t, delay, delay + 0.4);
        if q <= 0. || q >= 1. {
            continue;
        }
        let a = k as f32 / RAYS as f32 * TAU + (jitter(k, 2) - 0.5) * 0.1;
        let len = far * (0.25 + 0.35 * jitter(k, 3));
        // In from the window's edges at once (the corners first), then
        // gathering speed.
        let from = far * 0.78;
        let head = lerp(from, ring, in_quad(q));
        let tail = head + len * (1. - q).powf(0.7);
        let dir = [a.cos(), a.sin()];
        let at = |r: f32| [mid[0] + dir[0] * r, mid[1] + dir[1] * r];
        let weight = 1. + 1.5 * jitter(k, 4);
        let alpha = 0.3 + 0.6 * jitter(k, 5);
        shape = shape.line(
            at(head),
            at(tail),
            weight,
            rune::LineCap::Round,
            ink(p, alpha),
        );
    }
    // The ring draws itself on as they land, then bursts outward.
    let on = out_cubic(span(t, 0.22, 0.5));
    let burst = span(t, 0.5, 1.25);
    if on > 0. && burst < 1. {
        let r = lerp(ring, far * 1.1, out_cubic(burst));
        let width = lerp(2.5, 0.75, burst);
        let alpha = 0.7 * (1. - burst).powf(1.6);
        shape = shape.stroke(circle(mid, r, -PI / 2., on), width, ink(p, alpha));
        // An echo a beat behind it.
        let echo = span(t, 0.6, 1.3);
        if echo > 0. && echo < 1. {
            let r = lerp(ring, far * 0.9, out_cubic(echo));
            shape = shape.stroke(
                circle(mid, r, 0., 1.),
                1.,
                ink(p, 0.35 * (1. - echo).powi(2)),
            );
        }
    }
    shape
}

/// The text colour `amount` of the way from the shell's fill: a faded
/// stroke that stays opaque, so a ring's joints don't blend twice.
fn ink(p: &Palette, amount: f32) -> Color {
    crate::design::motion::mix(p.surface, p.text, amount)
}

/// The ring between `inner` and `outer` about `c`, filled: two half rings
/// overlapping a little, so no seam shows where they meet.
fn annulus(mut shape: Shape, c: [f32; 2], inner: f32, outer: f32, color: Color) -> Shape {
    const N: usize = 72;
    for half in 0..2 {
        let from = half as f32 * PI - 0.02;
        let sweep = PI + 0.04;
        let at = |r: f32, k: usize| {
            let a = from + k as f32 / N as f32 * sweep;
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        };
        let mut pts: Vec<[f32; 2]> = (0..=N).map(|k| at(outer, k)).collect();
        if inner > 0.5 {
            pts.extend((0..=N).rev().map(|k| at(inner, k)));
        } else {
            pts.push(c);
        }
        shape = shape.fill(pts, color);
    }
    shape
}

/// The wordmark canvas: the letters and the accent's tick.
fn wordmark(pose: &Pose, p: &Palette) -> Shape {
    let hgt = pose.height;
    let left = [pose.centre[0] - WIDTH * hgt / 2., pose.centre[1] - hgt / 2.];
    let at = |k: usize, q: [f32; 2]| [left[0] + (q[0] + pose.shift[k]) * hgt, left[1] + q[1] * hgt];
    let mut shape = Shape::new();
    for (k, pts) in letters().iter().enumerate() {
        if pose.draw[k] <= 0. {
            continue;
        }
        let drawn: Vec<[f32; 2]> = trim(pts, pose.draw[k])
            .into_iter()
            .map(|q| at(k, q))
            .collect();
        if drawn.len() >= 2 {
            shape = shape.stroke(drawn, WEIGHT * hgt, p.text);
        }
    }
    if pose.tick > 0. && pose.tick_alpha > 0. {
        // A square full stop on the baseline, a stroke's width across.
        let side = WEIGHT * hgt * pose.tick;
        let cx = left[0] + (WIDTH + 0.12 + WEIGHT / 2.) * hgt;
        let cy = left[1] + (1. - WEIGHT / 2.) * hgt;
        let d = side / 2.;
        shape = shape.fill(
            vec![
                [cx - d, cy - d],
                [cx + d, cy - d],
                [cx + d, cy + d],
                [cx - d, cy + d],
            ],
            p.accent.alpha(pose.tick_alpha),
        );
    }
    shape
}

/// The intro over the whole window: the stage, the wordmark, and while
/// waiting the late caption. Inert: clicks reach nothing until it ends.
pub fn stage(s: &State, p: &Palette, shell: Color) -> El {
    let i = &s.intro;
    let size = i.size;
    let base = (size[0].min(size[1]) * 0.2).clamp(72., 210.);
    let pose = pose(i, size, title_target(s));
    let full = |e: El| e.pin(Some(0.), Some(0.), Some(0.), Some(0.)).inert();
    let mut root = full(el().key("intro"))
        .on_layout(note_size)
        .a11y_hidden()
        .child(full(
            canvas(backdrop(i, p, shell, size, base)).key("intro-stage"),
        ));
    let mark = canvas(wordmark(&pose, p)).key("intro-mark");
    root = root.child(full(if pose.mark < 1. {
        mark.opacity(pose.mark)
    } else {
        mark
    }));
    if i.reduced.get() && i.revealing() {
        root = root.opacity(1. - i.reveal());
    }
    if i.waiting() {
        root = root.child(waiting(p, &pose));
    }
    root
}

/// The calm hold while a slow gateway boots: the tick breathes on the
/// small loaders' clock and the caption fades in late.
fn waiting(p: &Palette, pose: &Pose) -> El {
    let top = pose.centre[1] + pose.height / 2. + 56.;
    let hgt = pose.height;
    let side = WEIGHT * hgt;
    let x = pose.centre[0] + (WIDTH / 2. + 0.12) * hgt;
    let y = pose.centre[1] + (0.5 - WEIGHT) * hgt;
    col()
        .pin(Some(0.), Some(0.), Some(0.), Some(0.))
        .inert()
        .child(
            // Over the canvas's tick, breathing it.
            el().key("intro-tick")
                .pin(Some(x), Some(y), None, None)
                .size(side, side)
                .fill(p.accent)
                .cycle(Cycle {
                    period: 2.4,
                    phase: 0.,
                    stops: &crate::design::kit::loops::BREATH_35,
                    still: 1.,
                    fps: 15.,
                }),
        )
        .child(
            row()
                .pin(Some(0.), Some(top), Some(0.), None)
                .justify_center()
                .child(
                    label("Starting ZLO", SM, p.text_muted)
                        .key("intro-caption")
                        .sequence(rune::Sequence::new(CAPTION_IN)),
                ),
        )
}

/// The caption's entrance: hidden 700 ms, then in over 400 ms.
const CAPTION_IN: &[rune::Beat] = &[
    rune::Beat::new(0.0).opacity(0.0),
    rune::Beat::new(0.7).opacity(0.0),
    rune::Beat::new(1.1),
];

/// The stage's (or loading screen's) laid-out size, for the next frame.
fn note_size(s: &mut State, r: [f32; 4]) {
    if r[2] > 0. && r[3] > 0. {
        s.intro.size = [r[2], r[3]];
    }
}

/// The settled mark, still, for a later reconnect's loading screen.
pub fn still_mark(p: &Palette, size: [f32; 2]) -> El {
    let i = Intro::frozen(ASSEMBLE, None, false);
    let mut pose = pose(&i, size, None);
    pose.height *= 0.5;
    canvas(wordmark(&pose, p))
        .key("loading-mark")
        .pin(Some(0.), Some(0.), Some(0.), Some(0.))
        .inert()
        .a11y_hidden()
        .on_layout(note_size)
}

/// Wrap the body for the reveal: from 103 % down to rest, instantly per
/// frame (the poll steps it).
pub fn settle_body(s: &State, body: El) -> El {
    let i = &s.intro;
    if !i.revealing() || i.reduced.get() {
        return body;
    }
    let k = lerp(1.03, 1., out_cubic(i.reveal()));
    body.scale(k).scale_motion(Anim::INSTANT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1. / 120.;

    /// Run frames from 0 until `until`, with `connected_at` the time the
    /// connection settles. Returns the intro and the frames it asked for.
    fn run(mut i: Intro, connected_at: f64, until: f64) -> (Intro, usize) {
        let mut asked = 0;
        let mut clock = 10.;
        let start = clock;
        while clock - start <= until {
            let connecting = clock - start < connected_at;
            let (_, next) = i.step(clock, connecting);
            asked += next as usize;
            clock += FRAME;
        }
        (i, asked)
    }

    #[test]
    fn a_fast_connect_still_sees_the_whole_assembly() {
        let (i, _) = run(Intro::launch(), 0.1, 1.0);
        assert!(
            !i.done && !i.revealing(),
            "connected at 0.1 s, still assembling at 1 s"
        );
        let (i, _) = run(Intro::launch(), 0.1, ASSEMBLE + 0.05);
        assert!(i.revealing(), "the reveal starts once the mark is in");
        assert_eq!(i.reveal_at, Some(ASSEMBLE));
        let (i, _) = run(Intro::launch(), 0.1, ASSEMBLE + REVEAL + 0.02);
        assert!(i.done, "and hands off once the reveal ends");
    }

    #[test]
    fn a_slow_connect_waits_still_then_reveals_without_restarting() {
        let (i, asked) = run(Intro::launch(), 3.0, 2.9);
        assert!(i.waiting(), "assembled and holding");
        let assembly_frames = (ASSEMBLE / FRAME).ceil() as usize + 1;
        assert!(
            asked <= assembly_frames,
            "no frames asked while waiting ({asked})"
        );
        let (i, _) = run(Intro::launch(), 3.0, 3.1);
        assert!(
            i.revealing() && i.reveal_at.unwrap() >= 3.0,
            "reveals when it lands"
        );
        assert!(i.t > ASSEMBLE, "the clock carried on: nothing replayed");
        let (i, _) = run(Intro::launch(), 3.0, 3.0 + REVEAL + 0.05);
        assert!(i.done, "never blocks past the reveal once ready");
    }

    #[test]
    fn it_plays_once() {
        let (mut i, _) = run(Intro::launch(), 0.0, 2.0);
        assert!(i.done);
        // A later reconnect: connecting again changes nothing.
        let (changed, next) = i.step(100., true);
        assert!(!changed && !next && i.done && !i.playing());
        assert!(!Intro::default().playing(), "only `launch` arms it");
    }

    #[test]
    fn reduced_motion_is_a_short_fade() {
        let i = Intro::launch();
        i.reduced.set(true);
        let (i, _) = run(i, 0.0, FADE_IN + FADE_OUT + 0.02);
        assert!(i.done, "fade in, fade out, done ({})", i.t);
        let p = pose(&Intro::frozen(0.1, None, true), DEFAULT_SIZE, None);
        assert_eq!(p.draw, [1.; 3], "no stroke drawing: the whole mark fades");
        assert_eq!(p.shift, [0.; 3]);
        assert!(p.mark > 0. && p.mark < 1.);
    }

    #[test]
    fn a_failed_connection_reveals_too() {
        // `poll` holds the reveal only while connecting: a failure lets
        // the intro hand off to the error screen.
        let (i, _) = run(Intro::launch(), 0.5, 2.0);
        assert!(i.done);
    }

    #[test]
    fn the_wordmark_lands_on_the_title() {
        let target = ([140., 24.], 10.);
        let mut i = Intro::frozen(ASSEMBLE + REVEAL, Some(ASSEMBLE), false);
        let p = pose(&i, DEFAULT_SIZE, Some(target));
        assert!((p.centre[0] - 140.).abs() < 0.01 && (p.centre[1] - 24.).abs() < 0.01);
        assert!((p.height - 10.).abs() < 0.01);
        assert_eq!(p.mark, 0., "faded into the real title");
        i.t = ASSEMBLE;
        let p = pose(&i, DEFAULT_SIZE, Some(target));
        assert_eq!(p.centre, [640., 410.], "settled in the middle first");
        assert_eq!(p.draw, [1.; 3]);
    }

    #[test]
    fn trim_cuts_along_the_length() {
        let line = [[0., 0.], [10., 0.], [10., 10.]];
        assert_eq!(trim(&line, 0.25), vec![[0., 0.], [5., 0.]]);
        assert_eq!(trim(&line, 0.75), vec![[0., 0.], [10., 0.], [10., 5.]]);
        assert_eq!(trim(&line, 1.), line.to_vec());
    }
}
