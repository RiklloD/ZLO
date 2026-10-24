//! The new-thread hand-off, after Zeron's `composer_dock.rs` (MIT, see
//! `THIRD_PARTY_NOTICES.md`). The new-thread screen records where its
//! composer sits; when the first message opens the thread, the chat's
//! composer starts at that spot and glides down to its dock on a critically
//! damped spring. Only the prepaint origin moves, so hitboxes, caret and
//! selection travel with the pixels and nothing relayouts per frame.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window, canvas, div, point, prelude::*, px,
};

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

struct Flight {
    x: Glide,
    y: Glide,
    started: Instant,
    last: Option<Instant>,
}

thread_local! {
    /// Where the new-thread composer was last painted (window coordinates).
    static HERO: Cell<Option<(f32, f32)>> = const { Cell::new(None) };
    static FLIGHT: RefCell<Option<Flight>> = const { RefCell::new(None) };
}

/// Put inside the new-thread composer: records its origin every paint.
pub fn hero_probe() -> impl IntoElement {
    div().absolute().top_0().left_0().size_full().child(
        canvas(
            |bounds: Bounds<Pixels>, _, _| {
                HERO.set(Some((f32::from(bounds.left()), f32::from(bounds.top()))));
            },
            |_, _, _, _| {},
        )
        .size_full(),
    )
}

/// The first message left the new-thread screen: the next docked composer
/// starts from the hero's spot.
pub fn take_off(reduced: bool) {
    let flight = HERO.take().filter(|_| !reduced).map(|(x, y)| Flight {
        x: Glide::new(x),
        y: Glide::new(y),
        started: Instant::now(),
        last: None,
    });
    FLIGHT.set(flight);
}

/// Wrap the chat composer. Settled, it costs one borrow per frame.
pub fn docked(child: impl IntoElement) -> Docked {
    Docked {
        child: child.into_any_element(),
    }
}

pub struct Docked {
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
            let flight = slot.as_mut()?;
            // The first docked frame is the start of the motion, not
            // elapsed time: the thread may have taken a moment to open.
            let dt = flight
                .last
                .map_or(0., |last| now.saturating_duration_since(last).as_secs_f32());
            if flight.last.is_none() {
                flight.started = now;
            }
            flight.last = Some(now);
            flight.x.advance(x, dt, DOCK_SECS);
            flight.y.advance(y, dt, DOCK_SECS);
            let moving = flight.x.active() || flight.y.active() || route_progress_of(flight) < 1.;
            let offset = (flight.x.value - x, flight.y.value - y);
            if !moving {
                *slot = None;
                return None;
            }
            Some(offset)
        });
        match offset {
            Some((dx, dy)) => {
                window.request_animation_frame();
                window.with_element_offset(point(px(dx), px(dy)), |window| {
                    self.child.prepaint(window, cx);
                });
            }
            None => {
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

fn route_progress_of(f: &Flight) -> f32 {
    (f.started.elapsed().as_secs_f32() / DOCK_SECS).min(1.)
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
