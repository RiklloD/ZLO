//! Text styles of the GPUI app's rem scale (16 px base, 14 px body):
//! `text_xs` 12, `text_sm` 14, `text_lg` 18, the page title 20.

use rune::TextStyle;

pub const XS: TextStyle = TextStyle::new(12., 400.);
pub const XS_MEDIUM: TextStyle = TextStyle::new(12., 500.);
pub const XS_SEMIBOLD: TextStyle = TextStyle::new(12., 600.);
pub const SM: TextStyle = TextStyle::new(14., 400.);
pub const SM_MEDIUM: TextStyle = TextStyle::new(14., 500.);
pub const BODY: TextStyle = TextStyle::new(14., 400.);
pub const BODY_SEMIBOLD: TextStyle = TextStyle::new(14., 600.);
pub const LG_SEMIBOLD: TextStyle = TextStyle::new(18., 600.);
pub const MONO_XS: TextStyle = TextStyle::new(12., 400.).mono();
pub const TITLE: TextStyle = TextStyle::new(20., 500.);
