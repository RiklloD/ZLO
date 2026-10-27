//! The composer hand-off, after Zeron's `composer_dock.rs` (MIT, see
//! `THIRD_PARTY_NOTICES.md`). Both composers record where they sit. When the
//! first message opens the thread, the chat's composer starts at the hero's
//! spot and glides down to its dock; going back to a new thread, the hero
//! rises from the dock. Position rides a critically damped spring on the
//! prepaint origin, so hitboxes, caret and selection travel with the pixels.
//! Width eases between the two cards' widths on the same clock.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window, point, px,
};

use super::motion;

/// Zeron's route clock: the composer reaches its dock in 420 ms.
pub const DOCK_SECS: f32 = 0.420;

/// Critically damped motion: no overshoot, and position and velocity both
/// survive a new target. Twelve time constants settle within a fraction of
/// a pixel over `duration`.
#[derive(Clone, Copy, Debug)]
pub struct Glide {
    pub value: f32,
    pub velocity: f32,
    target: f32,
}

impl Glide {
    pub fn new(value: f32) -> Self {
        Self {
            value,
            velocity: 0.,
            target: value,
        }
    }

    pub fn advance(&mut self, target: f32, seconds: f32, duration: f32) {
        self.target = target;
        let omega = 12. / duration;
        let displacement = self.value - target;
        let c = self.velocity + omega * displacement;
        let decay = (-omega * seconds).exp();
        self.value = target + (displacement + c * seconds) * decay;
        self.velocity = (self.velocity - omega * c * seconds) * decay;
        if !self.active() {
            *self = Self::new(target);
        }
    }

    pub fn active(&self) -> bool {
        (self.value - self.target).abs() > 0.05 || self.velocity.abs() > 0.5
    }
}

/// Which composer a flight lands on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dest {
    /// The chat's docked composer.
    Dock,
    /// The new-thread screen's composer.
    Hero,
}

/// Where a composer was last painted: window origin and width.
#[derive(Clone, Copy)]
struct Spot {
    x: f32,
    y: f32,
    w: f32,
}

struct Flight {
    dest: Dest,
    x: Glide,
    y: Glide,
    from_w: f32,
    /// Set by the first destination frame: the route may take a moment to
    /// open, and that wait is not part of the motion.
    started: Option<Instant>,
    last: Option<Instant>,
}

impl Flight {
    fn progress(&self) -> f32 {
        self.started
            .map_or(0., |s| (s.elapsed().as_secs_f32() / DOCK_SECS).min(1.))
    }
}

thread_local! {
    static HERO: Cell<Option<Spot>> = const { Cell::new(None) };
    static DOCK: Cell<Option<Spot>> = const { Cell::new(None) };
    static FLIGHT: RefCell<Option<Flight>> = const { RefCell::new(None) };
}

fn slot(dest: Dest) -> &'static std::thread::LocalKey<Cell<Option<Spot>>> {
    match dest {
        Dest::Dock => &DOCK,
        Dest::Hero => &HERO,
    }
}

fn launch(from: Dest, to: Dest, reduced: bool) {
    let flight = slot(from)
        .with(Cell::get)
        .filter(|_| !reduced)
        .map(|s| Flight {
            dest: to,
            x: Glide::new(s.x),
            y: Glide::new(s.y),
            from_w: s.w,
            started: None,
            last: None,
        });
    FLIGHT.set(flight);
}

/// The first message left the new-thread screen: the next docked composer
/// starts from the hero's spot.
pub fn take_off(reduced: bool) {
    launch(Dest::Hero, Dest::Dock, reduced);
}

/// Back to a new thread from a chat: the hero rises from the dock.
pub fn land(reduced: bool) {
    launch(Dest::Dock, Dest::Hero, reduced);
}

/// The width `dest`'s card should draw at this frame while a flight lands
/// on it, easing from the departing card's width to its own resting width
/// (`fallback` before it ever rested).
pub fn width(dest: Dest, fallback: f32) -> Option<f32> {
    let natural = slot(dest).with(Cell::get).map_or(fallback, |s| s.w);
    FLIGHT.with_borrow(|f| {
        let f = f.as_ref().filter(|f| f.dest == dest)?;
        let t = motion::EASE_OUT_QUINT.eval(f.progress());
        (t < 1. && (f.from_w - natural).abs() > 0.5).then(|| motion::lerp(f.from_w, natural, t))
    })
}

/// Wrap the chat composer. Settled, it costs one borrow per frame.
pub fn docked(child: impl IntoElement) -> Docked {
    Docked {
        dest: Dest::Dock,
        child: child.into_any_element(),
    }
}

/// Wrap the new-thread composer.
pub fn hero(child: impl IntoElement) -> Docked {
    Docked {
        dest: Dest::Hero,
        child: child.into_any_element(),
    }
}

pub struct Docked {
    dest: Dest,
    child: AnyElement,
}

impl IntoElement for Docked {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Docked {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let (x, y) = (f32::from(bounds.left()), f32::from(bounds.top()));
        let now = Instant::now();
        let offset = FLIGHT.with_borrow_mut(|slot| {
            let flight = slot.as_mut().filter(|f| f.dest == self.dest)?;
            let dt = flight
                .last
                .map_or(0., |last| now.saturating_duration_since(last).as_secs_f32());
            flight.started.get_or_insert(now);
            flight.last = Some(now);
            flight.x.advance(x, dt, DOCK_SECS);
            flight.y.advance(y, dt, DOCK_SECS);
            let moving = flight.x.active() || flight.y.active() || flight.progress() < 1.;
            if !moving {
                *slot = None;
                return None;
            }
            Some((flight.x.value - x, flight.y.value - y))
        });
        match offset {
            Some((dx, dy)) => {
                window.request_animation_frame();
                window.with_element_offset(point(px(dx), px(dy)), |window| {
                    self.child.prepaint(window, cx);
                });
            }
            None => {
                // Only a resting card is a launch point.
                slot(self.dest).set(Some(Spot {
                    x,
                    y,
                    w: f32::from(bounds.size.width),
                }));
                self.child.prepaint(window, cx);
            }
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glide_settles_without_overshoot() {
        let mut g = Glide::new(0.);
        let mut t = 0.;
        while t < DOCK_SECS {
            g.advance(400., 1. / 120., DOCK_SECS);
            assert!(g.value <= 400.0001, "overshot at {t}: {}", g.value);
            t += 1. / 120.;
        }
        assert!((g.value - 400.).abs() < 0.5);
    }
}
