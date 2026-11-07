//! Mouse-wheel notches glide instead of jumping (GPUI `design/smooth_scroll.rs`).
//!
//! Rune scrolls a wheel 1:1: a notch arrives as one 40 pt delta and the
//! viewport jumps by it, while a trackpad streams small precise deltas and
//! already glides. GPUI wrapped its window root and replayed each notch over
//! the next frames on Chrome's wheel curve (ease-in-out, 200 ms, a notch in
//! flight moving the target on at the current speed).
//!
//! Here the same feel is a thin wrapper per scroll viewport, [`glide`]:
//! the viewport's `on_wheel` takes each notch (an imprecise delta with no
//! ⌘/⌃/⌥ held) and asks Rune to glide to the new offset with
//! `Tx::scroll_to_with` on [`WHEEL_GLIDE`]. A notch mid-glide moves the
//! target on from where the glide was headed, and Rune keeps the glide's
//! speed into the new leg, so a spin is one motion, as Chrome's retarget
//! does. Precise input (trackpads, free-spinning wheels) and modified
//! notches (zoom) pass through untouched, as in GPUI. Under reduced motion
//! Rune makes the glide a jump, so GPUI's "off under reduced motion" holds.
//!
//! Rune routes a wheel to the `on_wheel` handlers between the pointer and
//! the nearest scroll viewport, the viewport included, so the wrapper sits
//! on each viewport rather than once on the window root as in GPUI. Put it
//! on every vertical `.scroll(..)` the user reads
//! (`.scroll(..).wheel_glide(key)`, with the viewport's own key);
//! rune-kit's own popup lists keep the 1:1 wheel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use rune::{El, ScrollPos, Spring, Tx, WheelEvent};

/// Chrome's wheel glide: about 200 ms and critically damped (GPUI's
/// ease-in-out over 12 frames at 60 Hz).
pub const WHEEL_GLIDE: Spring = Spring::new(0.20, 0.0);

/// A notch this soon after the last one continues the glide: it moves the
/// target on rather than starting from where the view is. A little longer
/// than the glide, so a steady spin never restarts from a lagging offset.
const CHAIN: Duration = Duration::from_millis(260);

/// At most this many viewports are tracked; keys are per page or thread,
/// so the map is cleared when it grows past it.
const MAX_TRACKED: usize = 64;

/// One viewport's scroll position and the glide in flight.
#[derive(Clone, Copy, Default)]
struct Track {
    /// Where the viewport is, as it last drew (`on_scroll_offset`).
    offset: [f32; 2],
    /// How far it scrolls on each axis.
    max: [f32; 2],
    /// The offset the glide is heading for, and when its last notch came.
    target: Option<([f32; 2], Instant)>,
}

thread_local! {
    /// Per viewport key. The view rebuilds the handlers every run, so the
    /// glide lives here (UI thread only, like every handler).
    static TRACKS: RefCell<HashMap<String, Track>> = RefCell::new(HashMap::new());
}

/// Make wheel notches over the scroll viewport `viewport` glide. `key` is
/// the viewport's key (set here, so the glide can address it); apply it
/// after `.scroll(..)`.
pub fn glide(viewport: El, key: impl Into<String>) -> El {
    glide_with::<crate::state::State>(viewport, key.into())
}

/// [`glide`] as a builder step: `.scroll(..).wheel_glide(key)`.
pub trait WheelGlide {
    /// See [`glide`]; `key` is the viewport's own key.
    fn wheel_glide(self, key: impl Into<String>) -> El;
}

impl WheelGlide for El {
    fn wheel_glide(self, key: impl Into<String>) -> El {
        glide(self, key)
    }
}

/// [`glide`] for a window whose state type is `S` (tests).
fn glide_with<S: 'static>(viewport: El, key: String) -> El {
    let seen = key.clone();
    let wheel = key.clone();
    viewport
        .key(key)
        .on_scroll_offset(move |at: ScrollPos| {
            with_track(&seen, |t| {
                t.offset = at.offset;
                t.max = at.max;
            });
        })
        .on_wheel(move |_: &mut S, ev: WheelEvent, tx: &mut Tx| wheel_notch(&wheel, ev, tx))
}

/// Run `f` on `key`'s track, made on first use.
fn with_track<R>(key: &str, f: impl FnOnce(&mut Track) -> R) -> R {
    TRACKS.with(|tracks| {
        let mut tracks = tracks.borrow_mut();
        if tracks.len() >= MAX_TRACKED && !tracks.contains_key(key) {
            tracks.clear();
        }
        f(tracks.entry(key.to_string()).or_default())
    })
}

/// A wheel delta over the viewport: take a notch and glide, or let anything
/// else scroll as Rune does.
fn wheel_notch(key: &str, ev: WheelEvent, tx: &mut Tx) -> bool {
    let now = Instant::now();
    let Some(to) = with_track(key, |t| step(t, ev, now)) else {
        return false;
    };
    tx.scroll_to_with(key, to[0], to[1], WHEEL_GLIDE);
    true
}

/// Whether `ev` is a notch the glide takes: a mouse wheel's line step, with
/// no modifier that makes it something else (⌘/⌃ zoom, ⌥). Shift only picks
/// the axis, and macOS already turned the delta for it.
fn is_notch(ev: &WheelEvent) -> bool {
    !ev.precise && !ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt && (ev.dx != 0. || ev.dy != 0.)
}

/// The offset a notch glides to, or `None` to leave the delta to Rune. Any
/// other input ends a glide in flight, so the next notch starts from where
/// the view is.
fn step(t: &mut Track, ev: WheelEvent, now: Instant) -> Option<[f32; 2]> {
    if !is_notch(&ev) {
        t.target = None;
        return None;
    }
    let from = match t.target {
        Some((target, at)) if now.saturating_duration_since(at) < CHAIN => target,
        _ => t.offset,
    };
    // A positive delta moves the content down and right: the offset shrinks.
    let axis = |i: usize, d: f32| (from[i] - d).clamp(0., t.max[i].max(0.));
    let to = [axis(0, ev.dx), axis(1, ev.dy)];
    if to == from && t.target.is_none() {
        // At an edge with nothing in flight: nothing to glide.
        return None;
    }
    t.target = Some((to, now));
    Some(to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Cx, Env, Runtime, ScrollSpec, col, el};

    fn list(_: &(), _: &mut Cx<()>) -> El {
        let rows = (0..40).map(|i| el().key(format!("row-{i}")).w_full().h(40.));
        glide_with::<()>(
            col()
                .w_full()
                .h(200.)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(col().w_full().children(rows)),
            "test-list".into(),
        )
    }

    fn row_y(rt: &Runtime<()>) -> f32 {
        rt.presented("row-1").map_or(f32::NAN, |(r, _)| r[1])
    }

    #[test]
    fn a_wheel_notch_glides_where_a_trackpad_moves_at_once() {
        let mut rt = Runtime::new(
            (),
            list,
            (300., 200.),
            Env {
                system_dark: true,
                scale: 2.0,
            },
        );
        rt.frame(0.0);
        let rest = row_y(&rt);

        rt.wheel_event(WheelEvent::new(0., -40.).precise(false).at(100., 100.));
        rt.frame(1.0 / 60.0);
        rt.frame(1.0 / 60.0);
        let mid = row_y(&rt);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let end = row_y(&rt);
        assert!(
            mid < rest && mid > rest - 40.,
            "partway after two frames: {rest} → {mid}"
        );
        assert!(
            (rest - 40. - end).abs() < 0.5,
            "and one notch on: {rest} → {end}"
        );

        // A trackpad's precise delta moves the content in the same frame.
        rt.wheel_event(WheelEvent::new(0., -40.).precise(true).at(100., 100.));
        rt.frame(1.0 / 60.0);
        assert!(
            (end - 40. - row_y(&rt)).abs() < 0.5,
            "precise input stays 1:1"
        );
    }

    fn notch(dy: f32) -> WheelEvent {
        WheelEvent::new(0., dy).precise(false)
    }

    fn track(offset: f32, max: f32) -> Track {
        Track {
            offset: [0., offset],
            max: [0., max],
            target: None,
        }
    }

    #[test]
    fn a_notch_glides_one_line_step_down() {
        let mut t = track(100., 1000.);
        assert_eq!(step(&mut t, notch(-40.), Instant::now()), Some([0., 140.]));
    }

    #[test]
    fn a_notch_mid_glide_moves_the_target_on() {
        let mut t = track(100., 1000.);
        let now = Instant::now();
        step(&mut t, notch(-40.), now);
        // The view has only gone part of the way; the next notch adds on.
        t.offset = [0., 115.];
        assert_eq!(
            step(&mut t, notch(-40.), now + Duration::from_millis(30)),
            Some([0., 180.])
        );
        // Long after, a notch starts from where the view is.
        t.offset = [0., 180.];
        assert_eq!(
            step(&mut t, notch(40.), now + Duration::from_secs(2)),
            Some([0., 140.])
        );
    }

    #[test]
    fn targets_stay_inside_the_content() {
        let mut t = track(990., 1000.);
        assert_eq!(step(&mut t, notch(-40.), Instant::now()), Some([0., 1000.]));
        let mut top = track(0., 1000.);
        assert_eq!(step(&mut top, notch(40.), Instant::now()), None);
    }

    #[test]
    fn trackpads_and_modified_wheels_scroll_as_before() {
        let mut t = track(100., 1000.);
        let now = Instant::now();
        assert_eq!(
            step(&mut t, WheelEvent::new(0., -3.).precise(true), now),
            None
        );
        let zoom = WheelEvent::new(0., -40.).precise(false).mods(rune::Mods {
            cmd: true,
            ..rune::Mods::default()
        });
        assert_eq!(step(&mut t, zoom, now), None);
        // Precise input in between ends the glide: the next notch starts
        // from the view.
        step(&mut t, notch(-40.), now);
        step(&mut t, WheelEvent::new(0., -3.).precise(true), now);
        t.offset = [0., 50.];
        assert_eq!(step(&mut t, notch(-40.), now), Some([0., 90.]));
    }
}
