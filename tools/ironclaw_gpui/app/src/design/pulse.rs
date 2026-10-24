//! One shared clock for looping animations (spinners, pulse dots, shimmer),
//! after MonoCode's `pulse.rs` (MIT, see `THIRD_PARTY_NOTICES.md`).
//!
//! A view that paints a loop leases the clock. Each tick notifies every
//! leased view together, at 30 Hz instead of the display's 120 Hz, so all
//! loops land in one frame. A lease ends when its view was notified but did
//! not paint a loop again: a closed, scrolled-away or hidden spinner stops
//! costing anything, and the clock parks once no lease is left. While no
//! window is active, loops tick eight times slower.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, EntityId, Global, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Window,
};

const TICK: Duration = Duration::from_millis(33);
const BACKGROUND_STRIDE: u64 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rate {
    /// About 30 fps: fades and waves that must look continuous.
    Smooth,
    /// About 15 fps: small loaders.
    Half,
}

impl Rate {
    fn stride(self) -> u64 {
        match self {
            Rate::Smooth => 1,
            Rate::Half => 2,
        }
    }
}

type FrameCallback = Box<dyn FnOnce(&mut App)>;

struct Lease {
    rate: Rate,
    /// Painted a loop since the clock last notified it.
    painted: bool,
    on_tick: Option<FrameCallback>,
}

#[derive(Default)]
struct Clock {
    epoch: Option<Instant>,
    leases: HashMap<EntityId, Lease>,
    tick: u64,
    running: bool,
}

impl Global for Clock {}

/// Keeps the view being drawn repainting at `rate`. Call it every time the
/// loop is drawn. Returns false, leasing nothing, under reduced motion or
/// the battery saver: draw the resting frame.
pub fn request(rate: Rate, window: &Window, cx: &mut App) -> bool {
    request_inner(rate, window, cx, None)
}


fn request_inner(
    rate: Rate,
    window: &Window,
    cx: &mut App,
    mut on_tick: Option<FrameCallback>,
) -> bool {
    if cx.reduce_motion() {
        return false;
    }
    let view = window.current_view();
    let clock = cx.default_global::<Clock>();
    clock
        .leases
        .entry(view)
        .and_modify(|lease| {
            lease.painted = true;
            if on_tick.is_some() {
                lease.on_tick = on_tick.take();
            }
            if lease.rate.stride() > rate.stride() {
                lease.rate = rate;
            }
        })
        .or_insert_with(|| Lease {
            rate,
            painted: true,
            on_tick,
        });
    if !clock.running {
        clock.running = true;
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if !cx.update(tick) {
                    break;
                }
            }
        })
        .detach();
    }
    true
}

/// The fraction of `period` the shared clock is into, in `0..1`. Loops of
/// one period stay in step across the app.
pub fn phase(period: Duration, cx: &mut App) -> f32 {
    if period.is_zero() {
        return 0.;
    }
    let now = Instant::now();
    let clock = cx.default_global::<Clock>();
    let into = (now - *clock.epoch.get_or_insert(now)).as_nanos() % period.as_nanos();
    into as f32 / period.as_nanos() as f32
}

fn tick(cx: &mut App) -> bool {
    let background = !app_active(cx);
    let clock = cx.default_global::<Clock>();
    clock.tick = clock.tick.wrapping_add(1);
    let tick = clock.tick;
    let mut due = Vec::new();
    clock.leases.retain(|view, lease| {
        let stride = lease.rate.stride() * if background { BACKGROUND_STRIDE } else { 1 };
        if tick % stride != 0 {
            return true;
        }
        if !lease.painted {
            return false;
        }
        lease.painted = false;
        due.push((*view, lease.on_tick.take()));
        true
    });
    if clock.leases.is_empty() {
        clock.running = false;
        return false;
    }
    for (view, callback) in due {
        match callback {
            Some(callback) => callback(cx),
            None => cx.notify(view),
        }
    }
    true
}

fn app_active(cx: &mut App) -> bool {
    cx.windows().into_iter().any(|window| {
        window
            .update(cx, |_, window, _| window.is_window_active())
            .unwrap_or(false)
    })
}

/// A repeating animation on the shared clock: `build` gets the fraction of
/// `period` elapsed and returns the frame. Reduced motion draws phase zero.
pub fn looping<E: IntoElement>(
    id: impl Into<ElementId>,
    period: Duration,
    rate: Rate,
    build: impl FnOnce(f32) -> E + 'static,
) -> Looping {
    Looping {
        id: id.into(),
        period,
        rate,
        build: Some(Box::new(move |phase| build(phase).into_any_element())),
    }
}

pub struct Looping {
    id: ElementId,
    period: Duration,
    rate: Rate,
    build: Option<Box<dyn FnOnce(f32) -> AnyElement>>,
}

impl IntoElement for Looping {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Looping {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
    ) -> (LayoutId, AnyElement) {
        let phase = if request(self.rate, window, cx) {
            phase(self.period, cx)
        } else {
            0.
        };
        let mut element = match self.build.take() {
            Some(build) => build(phase),
            None => gpui_kit::div().into_any_element(),
        };
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}
