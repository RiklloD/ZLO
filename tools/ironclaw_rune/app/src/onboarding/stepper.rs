//! The wizard header's moving parts: the claw mark that draws itself in
//! once, and the steps (t3code `WizardSteps`) with one highlight that glides
//! to the current step.

use std::time::Duration;

use rune::{Beat, Easing, El, Sequence, Transition, el, row, text};

use crate::design::motion;
use crate::pages::common::ts::{sm_medium, xs_medium};
use crate::state::State;

use super::STEPS;

/// How long after setup first shows the claw mark may still draw itself
/// in; a render after that (a step change, a page visited and left) shows
/// it whole, so the entrance never replays.
pub const MARK_ENTRANCE: Duration = Duration::from_millis(900);

/// One stroke rising into place: from a faint, smaller pose to whole,
/// easing out (Apple's "ease out" for things arriving).
static STROKE_IN: [Beat; 2] = [
    Beat::new(0.).opacity(0.).scale(0.55),
    Beat::new(0.34).easing(Easing::EASE_OUT),
];

/// The beat between one stroke and the next.
const STROKE_STAGGER: f64 = 0.07;
/// The first stroke waits for the dialog to be on screen.
const STROKE_LEAD: f64 = 0.08;

/// Whether the claw mark still owes its first entrance.
fn mark_entering(s: &State) -> bool {
    s.onboarding
        .shown_at
        .is_some_and(|at| at.elapsed() < MARK_ENTRANCE)
}

/// The claw mark's three strokes at wordmark size (t3code's identity).
/// The first time setup shows, the strokes rise in one after another.
pub fn mark(s: &State) -> El {
    let p = &s.palette;
    let entering = mark_entering(s);
    let stroke = |ix: usize, h: f32| {
        let mut stroke = el()
            .key(format!("onboarding-mark-{ix}"))
            .w(4.)
            .h(h)
            .radius(2.)
            // One flat ink, as the rest of the chrome (no gradient).
            .fill(p.text);
        if entering {
            // A finite track on the render clock: it plays once from the
            // mount, holds its final (resting) pose, and holds whole under
            // Reduce Motion.
            stroke = stroke.sequence(
                Sequence::new(&STROKE_IN).delay(STROKE_LEAD + STROKE_STAGGER * ix as f64),
            );
        }
        stroke
    };
    row()
        .key("onboarding-mark")
        .h(22.)
        .gap(3.)
        .center_y()
        .a11y_hidden()
        .child(stroke(0, 14.))
        .child(stroke(1, 20.))
        .child(stroke(2, 14.))
}

/// The key of the highlight behind the current step.
pub const PILL_KEY: &str = "onboarding-step-pill";

/// t3code `WizardSteps`: done steps take a check and can be reopened. One
/// highlight sits behind the current step and glides to the next one. Drawn
/// as the app's segmented control: the faint wash, a raised knob, neutral
/// ink (the current step's badge is the solid one, as primary buttons are).
pub fn steps(s: &State) -> El {
    let p = &s.palette;
    let current = s.onboarding.step.index();
    let busy = s.onboarding.projects.importing;
    // The highlight lays out as one of three equal slots in a row shaped
    // like the steps' own (same padding and gap), so it covers the current
    // step exactly at any width; it stays one keyed element in one parent
    // and springs between slots on the selection pill's spring.
    let mut track = row()
        .key("onboarding-step-track")
        .pin(Some(0.), Some(0.), Some(0.), Some(0.))
        .padding(4.)
        .gap(4.)
        .inert()
        .a11y_hidden();
    for ix in 0..current {
        track = track.child(el().key(format!("before-{ix}")).grow(1.).min_w(0.));
    }
    let mut knob = el()
        .key(PILL_KEY)
        .grow(1.)
        .min_w(0.)
        .h_full()
        .radius(crate::design::ROW_RADIUS)
        .continuous_corners()
        .fill(if p.light {
            p.surface_raised
        } else {
            rune::Color::WHITE.alpha(0.12)
        })
        .motion(motion::PILL);
    if p.light {
        knob = knob.shadow(1., 2., rune::Color::BLACK.alpha(0.05));
    }
    track = track.child(knob);
    for ix in current + 1..STEPS.len() {
        track = track.child(el().key(format!("after-{ix}")).grow(1.).min_w(0.));
    }
    let mut list = row()
        .key("onboarding-steps")
        .role("list")
        .label("Setup progress")
        .w_full()
        .gap(4.)
        .padding(4.)
        .radius(crate::design::PANEL_RADIUS)
        .continuous_corners()
        .fill(crate::design::kit::hover_wash(p))
        .child(track);
    for (step, name) in STEPS {
        let ix = step.index();
        let here = ix == current;
        let done = ix < current;
        let mark = if done {
            rune::icon(&crate::icons::CHECK, 12.)
                .key("check")
                .color(p.text)
                .transition(motion::glyph_pop())
        } else {
            text((ix + 1).to_string())
                .key("number")
                .font(xs_medium())
                .color(if here { p.bg } else { p.text_muted })
                .transition(Transition::opacity())
        };
        let badge = el()
            .key("badge")
            .w(18.)
            .h(18.)
            .radius(9.)
            .center()
            .fill(if here { p.text } else { p.text.alpha(0.) })
            .border(1., if here { p.text } else { p.border_strong })
            .motion(motion::SLIDE)
            .child(mark);
        let mut item = row()
            .key(format!("onboarding-step-{ix}"))
            .grow(1.)
            .min_w(0.)
            .gap(8.)
            .px(10.)
            .py(8.)
            .radius(8.)
            .center_y()
            .label(format!("{name}, step {}", ix + 1))
            .child(badge)
            .child(
                text(name)
                    .font(sm_medium())
                    .color(if here { p.text } else { p.text_muted })
                    .truncate()
                    .min_w(0.),
            );
        if done && !busy {
            item = item
                .role("button")
                .focusable(true)
                .cursor(rune::Cursor::Pointer)
                .hover(crate::design::kit::hover_wash(p))
                .hover_motion(motion::HOVER, motion::HOVER)
                .press_scale(motion::PRESS)
                .on_press(move |s: &mut State| super::go_to(s, step));
        }
        list = list.child(item);
    }
    list
}
