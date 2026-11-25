//! The kit's segmented control: a knob that glides between segments sized
//! to their labels (Zeron's 150 ms tab slide).

use rune::tok::TextStyle;
use rune::{Cursor, El, el, text};

use super::{XS, black, hover_wash, white};
use crate::design::motion;
use crate::design::styles::Style;
use crate::state::State;

/// The narrowest segment, so short labels (On / Off) share one width.
const SEG_MIN: f32 = 54.;
const SEG_H: f32 = 24.;
const INSET: f32 = 2.;
/// Room either side of a label.
const SEG_PAD: f32 = 7.;
/// Geist's average advance as a share of the font size, a little generous
/// so a label never touches its segment's edge.
const ADVANCE: f32 = 0.55;

/// A segment's width for `label` at `px`: its estimated text width plus
/// padding, never under [`SEG_MIN`]. Every segment and the knob take their
/// widths from here, so the knob always lands exactly on its segment.
pub fn segment_width(label: &str, px: f32) -> f32 {
    let chars = label.chars().count() as f32;
    (chars * px * ADVANCE + 2. * SEG_PAD).max(SEG_MIN).ceil()
}

/// A segmented control whose knob glides, changing width as it goes, to the
/// picked segment instead of jumping. Each segment is as wide as its label
/// needs (at least 54 pt), so "System / Light / Dark" and "Cost / Tokens"
/// both fit; segments of short labels share the 54 pt width. The knob
/// springs on `motion::TAB_SLIDE`; Reduce Motion lands it.
///
/// ```ignore
/// kit::segmented(s, "usage-metric", &["Cost", "Tokens"], ix, false, |s, ix| ..)
/// ```
pub fn segmented(
    s: &State,
    key: &str,
    labels: &[&str],
    selected: usize,
    disabled: bool,
    on_pick: impl Fn(&mut State, usize) + Clone + 'static,
) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let px = st.text(XS);
    let widths: Vec<f32> = labels.iter().map(|l| segment_width(l, px)).collect();
    let mut bar = rune::row()
        .key(key.to_string())
        .role("radiogroup")
        .self_start()
        .padding(INSET)
        .radius(8.)
        .continuous_corners()
        .fill(hover_wash(p));
    if let Some(&w) = widths.get(selected) {
        let left = INSET + widths[..selected].iter().sum::<f32>();
        let mut knob = el()
            .key(format!("{key}-knob"))
            .absolute(left, INSET)
            .size(w, SEG_H)
            .radius(6.)
            .continuous_corners()
            .fill(if p.light {
                p.surface_raised
            } else {
                white(0.12)
            })
            .a11y_hidden()
            .motion(motion::TAB_SLIDE);
        if p.light {
            knob = knob.shadow(1., 2., black(0.05));
        }
        bar = bar.child(knob);
    }
    for (ix, (label, w)) in labels.iter().zip(&widths).enumerate() {
        let on = ix == selected;
        let mut seg = el()
            .key(format!("{key}-seg-{ix}"))
            .role("radio")
            .label(label.to_string())
            .size(*w, SEG_H)
            .center()
            .child(
                text(label.to_string())
                    .font(TextStyle::new(px, 400.))
                    .color(if on { p.text } else { p.text_muted })
                    .group_color(p.text)
                    .single_line(),
            );
        if !disabled && !on {
            let pick = on_pick.clone();
            seg = seg
                .cursor(Cursor::Pointer)
                .hover_group()
                .on_press(move |s: &mut State| pick(s, ix));
        }
        bar = bar.child(seg);
    }
    if disabled {
        bar = bar.dim(0.5);
    }
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_labels_keep_the_minimum_width_and_long_ones_grow() {
        assert_eq!(segment_width("On", 12.), SEG_MIN);
        assert_eq!(segment_width("Dark", 12.), SEG_MIN);
        assert!(segment_width("Accounts and keys", 12.) > SEG_MIN);
    }
}
