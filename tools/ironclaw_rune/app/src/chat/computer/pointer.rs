//! The agent's cursor on a card: the glide to each target, click ripples
//! and the drag trail, and the poll that animates them while any runs.

use super::*;

/// A point as fractions of the picture (0..1, origin top-left).
pub(super) type Frac = (f32, f32);

/// The cursor, as fractions of the picture.
#[derive(Debug, Clone, Copy)]
pub(super) struct Glide {
    pub(super) from: Frac,
    pub(super) to: Frac,
    pub(super) started: Instant,
}

impl Glide {
    pub(super) fn at(&self) -> (Frac, bool) {
        let elapsed = Instant::now().saturating_duration_since(self.started);
        let t = (elapsed.as_secs_f32() / GLIDE.as_secs_f32()).min(1.);
        let e = 1. - (1. - t).powi(3);
        let lerp = |a: f32, b: f32| a + (b - a) * e;
        (
            (lerp(self.from.0, self.to.0), lerp(self.from.1, self.to.1)),
            t < 1.,
        )
    }
}

pub(super) struct Ripple {
    pub(super) at: Frac,
    pub(super) starts: Instant,
    pub(super) right: bool,
}

/// Glide the card's cursor to the session's pointer when it targets this
/// window; ripple on fresh clicks, trail on fresh drags.
pub(super) fn follow_pointer(
    card: &mut Card,
    session: &ComputerSession,
    window: u32,
    skew_ms: i64,
) {
    let Some(p) = session.pointer.as_ref().filter(|p| p.window_id == window) else {
        card.glide = None;
        return;
    };
    let to = (p.x as f32, p.y as f32);
    if p.seq == card.pointer_seq {
        return;
    }
    let first = card.pointer_seq == 0;
    card.pointer_seq = p.seq;
    let fresh = now_ms() - skew_ms - p.at < FRESH_MS;
    let from = card.glide.map_or(to, |g| g.at().0);
    card.glide = Some(Glide {
        from: if first || !fresh { to } else { from },
        to,
        started: Instant::now(),
    });
    if first || !fresh {
        return;
    }
    if p.kind.contains("click") {
        let right = p.kind == "right_click";
        card.ripples.push(Ripple {
            at: to,
            starts: Instant::now() + GLIDE,
            right,
        });
        if p.kind == "double_click" {
            card.ripples.push(Ripple {
                at: to,
                starts: Instant::now() + GLIDE + Duration::from_millis(180),
                right: false,
            });
        }
    }
    if p.kind == "drag"
        && let (Some(x2), Some(y2)) = (p.to_x, p.to_y)
    {
        let end = (x2 as f32, y2 as f32);
        card.trail = Some((to, end, Instant::now()));
        // The cursor follows the drag once it reached the start.
        card.glide = Some(Glide {
            from: to,
            to: end,
            started: Instant::now() + GLIDE,
        });
    }
}

/// The drag trail's opacity: full, then fading to nothing over its last
/// [`TRAIL_FADE`].
pub(super) fn trail_alpha(age: Duration) -> f32 {
    let left = TRAIL.saturating_sub(age);
    (left.as_secs_f32() / TRAIL_FADE.as_secs_f32()).clamp(0., 1.)
}

/// Expire finished animations; keep drawing while any runs. A drag trail
/// that only stands still asks for one wake, when its fade begins, rather
/// than a frame every 16 ms.
pub fn poll(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    let Some(cu) = slot(s, id) else { return false };
    if cu.hidden || cu.session.is_none() {
        return false;
    }
    let now = Instant::now();
    let mut animating = false;
    let mut removed = false;
    let mut fade_in: Option<Duration> = None;
    for card in cu.cards.values_mut() {
        let ripples = card.ripples.len();
        card.ripples.retain(|r| now < r.starts + RIPPLE);
        removed |= card.ripples.len() != ripples;
        if card.trail.is_some_and(|(_, _, at)| at.elapsed() > TRAIL) {
            card.trail = None;
            removed = true;
        }
        animating |= card.glide.is_some_and(|g| g.at().1) || !card.ripples.is_empty();
        if let Some((_, _, at)) = card.trail {
            let until = (TRAIL - TRAIL_FADE).saturating_sub(now.saturating_duration_since(at));
            if until.is_zero() {
                animating = true;
            } else {
                fade_in = Some(fade_in.map_or(until, |f| f.min(until)));
            }
        }
    }
    if animating {
        tx.wake_after(FRAME);
    } else if let Some(fade) = fade_in {
        tx.wake_after(fade);
    }
    animating || removed
}
