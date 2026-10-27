//! FLIP motion: an element that moves between frames (a thread row resorted
//! to the top, a selection pill under another tab) glides from where it was
//! drawn to where it now sits, like Zeron's tab slide and row resort.
//!
//! Positions are measured against a group [`anchor`], the first child of the
//! moving rows' container, so scrolling the container moves the anchor too
//! and never reads as motion. The glide is a critically damped spring on the
//! prepaint origin (`dock::Glide`): hitboxes travel with the pixels, a second
//! move mid-flight keeps the velocity, and nothing relayouts per frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, SharedString, Styled as _, Window, canvas, point, px,
};

use super::dock::Glide;

/// A resorted row settles in about this long.
pub const ROW_SECS: f32 = 0.32;
/// A selection pill moving between tabs.
pub const PILL_SECS: f32 = 0.30;
/// The first frame after a quiet spell steps at most this far, so a move
/// that lands after a long idle starts from rest instead of jumping.
const MAX_STEP: f32 = 1. / 60.;

struct Slot {
    x: Glide,
    y: Glide,
    last: Instant,
    /// The group's draw count when this slot was last drawn.
    seen: u64,
}

/// A group's origin this frame and how many frames it has drawn.
#[derive(Clone, Copy, Default)]
struct Origin {
    x: f32,
    y: f32,
    frame: u64,
}

thread_local! {
    static ORIGINS: RefCell<HashMap<&'static str, Origin>> = RefCell::new(HashMap::new());
    static SLOTS: RefCell<HashMap<SharedString, Slot>> = RefCell::new(HashMap::new());
}

/// The origin rows of `group` are measured from. Put it first among them.
pub fn anchor(group: &'static str) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, _, _| {
            ORIGINS.with_borrow_mut(|o| {
                let at = o.entry(group).or_default();
                at.x = f32::from(bounds.left());
                at.y = f32::from(bounds.top());
                at.frame += 1;
            });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_0()
}

/// Wrap `child` so it glides when its spot in `group` changes. `key` names
/// it across frames; `secs` is the spring's settle time.
pub fn flip(
    group: &'static str,
    key: impl Into<SharedString>,
    secs: f32,
    child: impl IntoElement,
) -> Flip {
    Flip {
        group,
        key: key.into(),
        secs,
        child: child.into_any_element(),
    }
}

pub struct Flip {
    group: &'static str,
    key: SharedString,
    secs: f32,
    child: AnyElement,
}

impl IntoElement for Flip {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Flip {
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
        let o = ORIGINS
            .with_borrow(|o| o.get(self.group).copied())
            .unwrap_or_default();
        let (x, y) = (
            f32::from(bounds.left()) - o.x,
            f32::from(bounds.top()) - o.y,
        );
        let now = Instant::now();
        let reduced = cx.reduce_motion();
        let offset = SLOTS.with_borrow_mut(|slots| {
            if slots.len() > 4_096 {
                slots.clear();
            }
            let fresh = Slot {
                x: Glide::new(x),
                y: Glide::new(y),
                last: now,
                seen: o.frame,
            };
            let slot = slots.entry(self.key.clone()).or_insert(fresh);
            // A row back from being hidden (a filter, a folded shelf) skipped
            // a frame of its group: it appears in place rather than flying
            // in from where it used to be.
            let stale = slot.seen + 1 < o.frame;
            slot.seen = o.frame;
            if reduced || stale {
                *slot = Slot {
                    x: Glide::new(x),
                    y: Glide::new(y),
                    last: now,
                    seen: o.frame,
                };
                return None;
            }
            let dt = now
                .saturating_duration_since(slot.last)
                .as_secs_f32()
                .min(MAX_STEP);
            slot.last = now;
            slot.x.advance(x, dt, self.secs);
            slot.y.advance(y, dt, self.secs);
            let (dx, dy) = (slot.x.value - x, slot.y.value - y);
            (slot.x.active() || slot.y.active()).then_some((dx, dy))
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
