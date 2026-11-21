//! The model picker's thinking row: the current model's levels as one
//! segmented control under the models, its knob gliding to the pick, and
//! Tab / Shift+Tab stepping it from the search field.

use rune::{El, el, row as hrow};

use super::{ModelPickerState, Slot, effort_label, effort_short, pick_effort};
use crate::design::kit::compact::{XS, txt, txt_medium};
use crate::design::{self, Palette};
use crate::state::State;

/// Move the thinking level one step along the row (Auto, then the model's
/// levels), wrapping; nothing when the model has no levels.
pub(super) fn step(s: &mut State, slot: &Slot, forward: bool) {
    let Some(m) = slot.get(s) else { return };
    let model = m.current_model().unwrap_or("default");
    let choices: Vec<Option<String>> = std::iter::once(None)
        .chain(
            m.efforts_of(m.current_provider(), model)
                .iter()
                .cloned()
                .map(Some),
        )
        .collect();
    if choices.len() < 2 {
        return;
    }
    let at = choices.iter().position(|c| *c == m.effort).unwrap_or(0);
    let n = choices.len();
    let next = if forward {
        (at + 1) % n
    } else {
        (at + n - 1) % n
    };
    pick_effort(s, slot.clone(), choices[next].clone());
}

/// The popover's thinking row: the current model's levels as one segmented
/// control, "Auto" first (the provider's default). Picking a level keeps
/// the popover open; picking a model closes it; Tab / Shift+Tab in the
/// search field step it from the keyboard. `None` when the model has no
/// levels.
pub(super) fn row(s: &State, m: &ModelPickerState, slot: &Slot) -> Option<El> {
    let model = m.current_model().unwrap_or("default");
    let efforts = m.efforts_of(m.current_provider(), model).to_vec();
    if efforts.is_empty() {
        return None;
    }
    let p = &s.palette;
    let choices: Vec<(Option<String>, String)> = std::iter::once((None, "Auto".to_string()))
        .chain(efforts.iter().map(|e| (Some(e.clone()), effort_short(e))))
        .collect();
    let picked = choices.iter().position(|(value, _)| *value == m.effort);
    let mut track = hrow()
        .key("thinking-track")
        .role("radiogroup")
        .label("Thinking")
        .grow(1.)
        .min_w(0.)
        .min_h(26.)
        .padding(2.)
        .gap(2.)
        .radius(design::CONTROL_RADIUS + 1.)
        .fill(p.bg)
        .border(1., p.border)
        .children(picked.map(|ix| thinking_knob(p, ix, choices.len())));
    for (value, title) in choices {
        let on = m.effort == value;
        let key = value.clone().unwrap_or_else(|| "auto".into());
        let full = value
            .as_deref()
            .map_or_else(|| "Auto".to_string(), effort_label);
        let pick = slot.clone();
        track = track.child(
            el().key(format!("thinking-{key}"))
                .role("radio")
                .label(full)
                .checked(on)
                .grow(1.)
                .w(0.)
                .min_w(0.)
                .py(3.)
                .center()
                .radius(design::CONTROL_RADIUS - 1.)
                .hover(p.hover)
                .cursor(rune::Cursor::Pointer)
                .child(txt(title, XS, if on { p.text } else { p.text_muted }).truncate())
                .on_press(move |s: &mut State| pick_effort(s, pick.clone(), value.clone())),
        );
    }
    Some(
        hrow()
            .key("model-thinking")
            .px(10.)
            .py(8.)
            .gap(10.)
            .center_y()
            .child(txt_medium("Thinking", XS, p.text_faint))
            .child(track),
    )
}

/// The thinking row's knob: one keyed plate in a layer laid out like the
/// segments (same inset, gap and equal widths), standing in the picked
/// segment's place. Picking another level moves it to that place, so it
/// glides there on Zeron's tab slide instead of the fill jumping.
fn thinking_knob(p: &Palette, picked: usize, n: usize) -> El {
    let mut layer = hrow()
        .key("thinking-knobs")
        .pin(Some(2.), Some(2.), Some(2.), Some(2.))
        .gap(2.)
        .a11y_hidden()
        .inert();
    for ix in 0..n {
        layer = layer.child(if ix == picked {
            el().key("thinking-knob")
                .grow(1.)
                .w(0.)
                .h_full()
                .radius(design::CONTROL_RADIUS - 1.)
                .fill(p.surface_raised)
                .motion(design::motion::TAB_SLIDE)
        } else {
            el().key(format!("thinking-slot-{ix}"))
                .grow(1.)
                .w(0.)
                .h_full()
        });
    }
    layer
}

#[cfg(test)]
mod tests {
    use super::step;
    use crate::chat::model_picker::{Slot, scene};

    /// Tab and Shift+Tab in the search field step the thinking level along
    /// the row, wrapping past Auto.
    #[test]
    fn tab_steps_the_thinking_level() {
        let mut s = crate::chat::fixtures::test_state();
        scene(&mut s, true);
        let slot = Slot::chat("t1");
        let effort = |s: &crate::state::State| slot.get(s).and_then(|m| m.effort.clone());
        assert_eq!(effort(&s).as_deref(), Some("high"));
        step(&mut s, &slot, true);
        assert_eq!(effort(&s).as_deref(), Some("xhigh"));
        step(&mut s, &slot, true);
        assert_eq!(effort(&s), None, "wraps to Auto");
        step(&mut s, &slot, false);
        assert_eq!(effort(&s).as_deref(), Some("xhigh"));
    }
}
