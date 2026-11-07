//! Text styles of the GPUI app's rem scale (16 px base, 14 px body):
//! `text_xs` 12, `text_sm` 14, `text_lg` 18, the page title 20. Each is a
//! function, scaled to Appearance → UI font size (`styles::rem`) as GPUI's
//! rem was; code ([`MONO_XS`]) keeps its fixed size.

use rune::TextStyle;

use crate::design::styles::rem;

fn at(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(rem(size), weight)
}

pub fn xs() -> TextStyle {
    at(12., 400.)
}
pub fn xs_medium() -> TextStyle {
    at(12., 500.)
}
pub fn xs_semibold() -> TextStyle {
    at(12., 600.)
}
pub fn sm() -> TextStyle {
    at(14., 400.)
}
pub fn sm_medium() -> TextStyle {
    at(14., 500.)
}
pub fn body_semibold() -> TextStyle {
    at(14., 600.)
}
pub fn lg_semibold() -> TextStyle {
    at(18., 600.)
}
pub fn title() -> TextStyle {
    at(20., 500.)
}
pub const MONO_XS: TextStyle = TextStyle::new(12., 400.).mono();
