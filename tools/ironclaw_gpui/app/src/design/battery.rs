//! Battery saver (t3code behaviour): decorative animation stops while the
//! window is in the background or nobody has touched it for 30 s.
//!
//! It flips gpui's `reduce_motion`, so every `with_animation` element (our
//! motion helpers, gpui-kit spinners) renders its static frame and schedules
//! nothing. That goes one step past t3code's 1 fps: zero frames, and live
//! content (streamed text, new rows) still repaints because it re-renders
//! on data, not on a clock.

use std::time::{Duration, Instant};

use gpui_kit::{App, Context, Task, Window};

const IDLE_AFTER: Duration = Duration::from_secs(30);

pub struct BatterySaver {
    last_input: Instant,
    idle: bool,
    active: bool,
    _timer: Option<Task<()>>,
}

impl BatterySaver {
    pub fn new() -> Self {
        Self {
            last_input: Instant::now(),
            idle: false,
            active: true,
            _timer: None,
        }
    }

    /// Start watching. `get` reaches the saver inside its owning view.
    pub fn start<V: 'static>(&mut self, get: fn(&mut V) -> &mut Self, cx: &mut Context<V>) {
        self.arm(get, cx);
    }

    /// Any key press or pointer movement. Cheap: one `Instant` store unless
    /// the saver was engaged.
    pub fn input<V: 'static>(&mut self, get: fn(&mut V) -> &mut Self, cx: &mut Context<V>) {
        self.last_input = Instant::now();
        if self.idle {
            self.idle = false;
            self.apply(cx);
            self.arm(get, cx);
        }
    }

    /// Engaged: the window is in the background or idle. Pollers skip
    /// their tick while this holds.
    pub fn saving(&self) -> bool {
        self.idle || !self.active
    }

    pub fn set_active(&mut self, window: &Window, cx: &mut App) {
        self.active = window.is_window_active();
        if self.active {
            self.last_input = Instant::now();
            self.idle = false;
        }
        self.apply(cx);
    }

    fn apply(&self, cx: &mut App) {
        // "Reduce motion: On" in Appearance holds it regardless; "Off" lets
        // only the battery saver decide, as "System" does.
        let forced = cx
            .try_global::<super::prefs::Prefs>()
            .is_some_and(|p| p.reduce_motion == "on");
        cx.set_reduce_motion(forced || self.idle || !self.active);
    }

    /// One timer that wakes at the idle deadline and re-sleeps for whatever
    /// input pushed it back, so motion costs at most one wake per 30 s.
    fn arm<V: 'static>(&mut self, get: fn(&mut V) -> &mut Self, cx: &mut Context<V>) {
        self._timer = Some(cx.spawn(async move |view, cx| {
            let mut wait = IDLE_AFTER;
            loop {
                cx.background_executor().timer(wait).await;
                let next = view.update(cx, |view, cx| {
                    let saver = get(view);
                    let quiet = saver.last_input.elapsed();
                    if quiet >= IDLE_AFTER {
                        saver.idle = true;
                        saver.apply(cx);
                        None
                    } else {
                        Some(IDLE_AFTER - quiet)
                    }
                });
                match next {
                    Ok(Some(rest)) => wait = rest,
                    _ => break,
                }
            }
        }));
    }
}
