//! The app's own controls, painted from the active [`Style`] and animated
//! through the tween stores in `motion`: hover washes fade, selections
//! glide, presses sink a pixel, popovers arrive on a spring.
//!
//! gpui-kit components still render everywhere else; they follow the same
//! palette and radii through the theme `install` writes, they only lack
//! the motion and the materials.
//!
//! The glass recipes (gradient lit from above, hairline rim, inset top
//! highlight, short drop) are adapted from Zeron's `glass.rs` (MIT).

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable, h_flex, v_flex};
use gpui_kit::{
    App, Background, BoxShadow, ClickEvent, Div, ElementId, Hsla, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, SharedString, Stateful, StatefulInteractiveElement, Styled, Window,
    div, hsla, linear_color_stop, linear_gradient, point, prelude::FluentBuilder, px,
};
use std::time::Duration;

use super::motion::{self, mix};
use super::pulse;
use super::styles::Style;
use super::{Palette, lift, on_accent};

fn shadow(color: Hsla, y: f32, blur: f32, spread: f32, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset,
    }
}

fn white(a: f32) -> Hsla {
    hsla(0., 0., 1., a)
}

fn black(a: f32) -> Hsla {
    hsla(0., 0., 0., a)
}

fn vertical(top: Hsla, bottom: Hsla) -> Background {
    linear_gradient(
        180.,
        linear_color_stop(top, 0.),
        linear_color_stop(bottom, 1.),
    )
}

/// A painted surface: fill, rim and shadows, ready to apply to a div.
pub struct Plate {
    pub fill: Background,
    pub rim: Hsla,
    pub shadows: Vec<BoxShadow>,
}

impl Plate {
    pub fn apply<E: Styled>(self, el: E) -> E {
        el.bg(self.fill)
            .border_1()
            .border_color(self.rim)
            .shadow(self.shadows)
    }
}

/// The neutral raised plate (secondary buttons, chips, cards). `h` is
/// hover progress, `lit` how raised it reads at rest (0..1). One quiet
/// recipe: a faint fill lit from above, a hairline rim and a soft drop.
pub fn neutral(s: &Style, p: &Palette, h: f32, lit: f32) -> Plate {
    let k = lit + 0.5 * h;
    if p.light {
        return Plate {
            fill: Background::from(mix(p.surface_card, p.surface, 0.35 * h)),
            rim: black(0.07 + 0.04 * h),
            shadows: vec![
                shadow(black(0.04 * k * s.elevation), 1., 2., 0., false),
                shadow(black(0.04 * k * s.elevation), 4., 12., -4., false),
            ],
        };
    }
    Plate {
        fill: vertical(white(0.055 + 0.035 * k), white(0.035 + 0.03 * k)),
        rim: white(0.07 + 0.05 * k),
        shadows: vec![
            shadow(white(0.05 * k), 1., 0., 0., true),
            shadow(black(0.25 * k * s.elevation), 1., 3., 0., false),
        ],
    }
}

/// The accent plate (primary buttons, the send button): a flat accent
/// fill, a lit top edge and a colored halo that spreads on hover.
pub fn accent(_s: &Style, p: &Palette, h: f32) -> Plate {
    let base = p.accent;
    Plate {
        fill: vertical(lift(base, 0.04 + 0.03 * h), base),
        rim: lift(base, 0.12).opacity(0.6),
        shadows: vec![
            shadow(white(0.22), 1., 0., 0., true),
            shadow(base.opacity(0.16 + 0.24 * h), 2., 8. + 10. * h, 0., false),
        ],
    }
}

/// Floating surfaces: popovers, menus, the composer.
pub fn floating(s: &Style, p: &Palette) -> Plate {
    let e = s.elevation;
    let fill = if s.vibrancy {
        p.surface_overlay.opacity(0.9)
    } else {
        p.surface_overlay
    };
    let mut shadows = vec![
        shadow(
            black((if p.light { 0.1 } else { 0.4 }) * e),
            12.,
            32.,
            -8.,
            false,
        ),
        shadow(
            black((if p.light { 0.05 } else { 0.2 }) * e),
            2.,
            6.,
            0.,
            false,
        ),
    ];
    if !p.light {
        shadows.push(shadow(white(0.06), 1., 0., 0., true));
    }
    Plate {
        fill: Background::from(fill),
        rim: if p.light { black(0.08) } else { white(0.09) },
        shadows,
    }
}

/// Elevation for the content panel: it floats a step above the sidebar on
/// a soft shadow, no outline.
pub fn panel_shadows(s: &Style, p: &Palette) -> Vec<BoxShadow> {
    let e = s.elevation;
    let mut v = vec![
        shadow(
            black((if p.light { 0.06 } else { 0.35 }) * e),
            6.,
            24.,
            -8.,
            false,
        ),
        shadow(
            black((if p.light { 0.04 } else { 0.2 }) * e),
            1.,
            2.,
            0.,
            false,
        ),
    ];
    if !p.light {
        // A lit top edge instead of a hairline all round.
        v.push(shadow(white(0.045), 1., 0., 0., true));
    }
    v
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Outline,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
    XSmall,
    Small,
    Medium,
    Large,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// The app's button, a drop-in for gpui-kit's (same builder names), so
/// every screen gets the motion: hover fades in the wash, a press sinks it
/// by a pixel, `selected` morphs the wash in, Primary carries the accent
/// plate and spreads a halo on hover, `loading` swaps the icon for
/// breathing dots.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<Icon>,
    icon_px: Option<f32>,
    text_px: Option<f32>,
    variant: Variant,
    size: Size,
    selected: bool,
    disabled: bool,
    loading: bool,
    full: bool,
    tooltip: Option<SharedString>,
    aria: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

pub fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id)
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            label: None,
            icon: None,
            icon_px: None,
            text_px: None,
            variant: Variant::Secondary,
            size: Size::Medium,
            selected: false,
            disabled: false,
            loading: false,
            full: false,
            tooltip: None,
            aria: None,
            on_click: None,
        }
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    /// Glyph size over the size preset's, for icons that draw edge to edge.
    pub fn icon_size(mut self, size: f32) -> Self {
        self.icon_px = Some(size);
        self
    }
    /// Label size over the size preset's.
    pub fn text_size(mut self, size: f32) -> Self {
        self.text_px = Some(size);
        self
    }
    pub fn primary(mut self) -> Self {
        self.variant = Variant::Primary;
        self
    }
    pub fn ghost(mut self) -> Self {
        self.variant = Variant::Ghost;
        self
    }
    pub fn outline(mut self) -> Self {
        self.variant = Variant::Outline;
        self
    }
    pub fn danger(mut self) -> Self {
        self.variant = Variant::Danger;
        self
    }
    pub fn small(mut self) -> Self {
        self.size = Size::Small;
        self
    }
    pub fn xsmall(mut self) -> Self {
        self.size = Size::XSmall;
        self
    }
    /// Icon-first header buttons: a 38 px target with a 20 px glyph.
    pub fn large(mut self) -> Self {
        self.size = Size::Large;
        self
    }
    pub fn selected(mut self, on: bool) -> Self {
        self.selected = on;
        self
    }
    pub fn disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        self
    }
    pub fn loading(mut self, on: bool) -> Self {
        self.loading = on;
        self
    }
    /// Stretch to the parent's width, content left-aligned.
    pub fn full(mut self) -> Self {
        self.full = true;
        self
    }
    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }
    pub fn accessibility_label(mut self, text: impl Into<SharedString>) -> Self {
        self.aria = Some(text.into());
        self
    }
    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let s = *Style::of(cx);
        let p = *Palette::of(cx);
        let key: SharedString = format!("btn:{:?}", self.id).into();
        let inert = self.disabled || self.loading;
        let h = if inert { 0. } else { motion::hover_t(&key) };
        let sel = motion::state_t(
            &format!("{key}:sel"),
            self.selected,
            s.slide,
            cx.reduce_motion(),
        );
        let icon_only = self.label.is_none();
        let height = match self.size {
            Size::XSmall => 24.,
            Size::Small => 30.,
            Size::Medium => 34.,
            Size::Large => 38.,
        };
        let pad = match self.size {
            Size::XSmall => 7.,
            Size::Small => 9.,
            Size::Medium | Size::Large => 12.,
        };
        let text = match self.variant {
            Variant::Primary => on_accent(p),
            Variant::Danger => p.danger,
            _ => mix(p.text_muted, p.text, h.max(sel)),
        };

        let mut el = h_flex()
            .id(self.id.clone())
            .role(gpui_kit::Role::Button)
            .relative()
            .flex_none()
            .h(px(height))
            .gap_1p5()
            .items_center()
            .when(!self.full, |d| d.justify_center())
            .when(self.full, |d| d.w_full())
            .when(icon_only, |d| d.w(px(height)))
            .when(!icon_only, |d| d.px(px(pad)))
            .rounded(px(s.control_radius))
            .text_color(text)
            .when(matches!(self.size, Size::XSmall | Size::Small), |d| {
                d.text_xs()
            })
            .when(matches!(self.size, Size::Medium | Size::Large), |d| {
                d.text_sm()
            })
            .when_some(self.text_px, |d, size| d.text_size(px(size)))
            .font_weight(gpui_kit::FontWeight::MEDIUM)
            .when_some(self.aria.or_else(|| self.label.clone()), |d, a| {
                d.aria_label(a)
            });

        el = match self.variant {
            Variant::Primary => accent(&s, &p, h).apply(el),
            Variant::Secondary => {
                let plate = neutral(&s, &p, h, 0.6);
                let rim = mix(plate.rim, p.accent.opacity(0.6), sel);
                plate.apply(el).border_color(rim)
            }
            Variant::Outline => el
                .border_1()
                .border_color(mix(mix(p.border_strong, p.text_faint, h), p.accent, sel))
                .bg(mix(mix(p.hover.opacity(0.), p.hover, h), p.active, sel)),
            Variant::Ghost => el.bg(mix(mix(p.hover.opacity(0.), p.hover, h), p.active, sel)),
            Variant::Danger => el
                .border_1()
                .border_color(p.danger.opacity(0.3 + 0.3 * h))
                .bg(p.danger.opacity(0.1 + 0.08 * h)),
        };

        let dots = self
            .loading
            .then(|| pulse_dots(ElementId::Name(format!("{key}:dots").into()), text));
        el.when(self.disabled, |d| d.opacity(0.45))
            .when(!inert, |d| {
                d.cursor_pointer()
                    .on_hover(motion::hover_listener(key.clone(), s.hover))
                    // The press: sink one pixel while the pointer is down.
                    .active(|st| st.top(px(1.)))
            })
            .when_some(dots, |d, dots| d.child(dots))
            .when_some(self.icon.filter(|_| !self.loading), |d, icon| {
                d.child(match (self.icon_px, self.size) {
                    (Some(size), _) => icon.size(px(size)),
                    (None, Size::Medium) => icon,
                    (None, Size::Large) => icon.size(px(20.)),
                    (None, Size::Small) => icon.small(),
                    (None, Size::XSmall) => icon.xsmall(),
                })
            })
            .when_some(self.label, |d, label| d.child(label))
            .when_some(self.tooltip, |d, tip| {
                d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when_some(self.on_click.filter(|_| !inert), |d, f| d.on_click(f))
    }
}

/// A sidebar or list row with a fading hover wash and a selection that
/// morphs in: the wash brightens and an accent tick grows at the left edge.
pub fn row(id: impl Into<SharedString>, active: bool, cx: &App) -> Stateful<Div> {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let id: SharedString = id.into();
    let key: SharedString = format!("row:{id}").into();
    let h = motion::hover_t(&key);
    let a = motion::state_t(&format!("{key}:on"), active, s.slide, cx.reduce_motion());
    let rest = hover_wash(&p).opacity(0.);
    // Zeron: the selected row never drifts toward the hover wash; the wash
    // alone marks it, no ring or tick.
    let wash = mix(mix(rest, hover_wash(&p), h), selected_wash(&s, &p), a);
    h_flex()
        .id(ElementId::Name(id))
        .relative()
        .w_full()
        .min_h(px(s.row_height))
        .gap_2()
        .px_2()
        .rounded(px(8.))
        .text_size(gpui_kit::rems(13. / 16.))
        .cursor_pointer()
        .bg(wash)
        .text_color(mix(p.text.opacity(0.8), p.text, h.max(a)))
        .on_hover(motion::hover_listener(key, s.hover))
}

/// The selected row: a light tint of the theme's ink, 7 % on dark and
/// 5.5 % on light, so the selection reads without turning into a slab.
pub fn selected_wash(_s: &Style, p: &Palette) -> Hsla {
    if p.light { black(0.055) } else { white(0.07) }
}

/// Row hover: one step under the selection.
pub fn hover_wash(p: &Palette) -> Hsla {
    if p.light { black(0.035) } else { white(0.04) }
}

/// A sliding pill behind a vertical list of rows. Put it as the first child
/// of a `relative` container; `top` is the selected row's offset in px
/// (`None` hides it). It glides between rows, across group gaps, instead of
/// jumping.
pub fn slide_pill(key: &str, top: Option<f32>, cx: &App) -> impl IntoElement {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let reduced = cx.reduce_motion();
    let shown = motion::state_t(&format!("{key}:shown"), top.is_some(), s.slide, reduced);
    // Hidden: hold the last place so the pill fades out where it was.
    let target = top.unwrap_or_else(|| last_slot(key));
    remember_slot(key, target);
    let y = motion::glide(key, target, s.slide, reduced);
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(y))
        .h(px(s.row_height))
        .rounded(px(8.))
        .bg(selected_wash(&s, &p).opacity(selected_wash(&s, &p).a * shown))
}

thread_local! {
    static SLOTS: std::cell::RefCell<std::collections::HashMap<String, f32>> =
        Default::default();
}

fn last_slot(key: &str) -> f32 {
    SLOTS.with(|s| s.borrow().get(key).copied().unwrap_or(0.))
}

fn remember_slot(key: &str, v: f32) {
    SLOTS.with(|s| {
        s.borrow_mut().insert(key.to_string(), v);
    });
}

/// A segmented control whose highlight slides between equal-width
/// segments (Zeron's 150 ms tab slide) instead of jumping.
pub fn segmented(
    key: &str,
    labels: &[&'static str],
    selected: usize,
    disabled: bool,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui_kit::AnyElement {
    const SEG_W: f32 = 54.;
    const SEG_H: f32 = 24.;
    const INSET: f32 = 2.;
    let p = *Palette::of(cx);
    let x = motion::glide(
        &format!("{key}:seg"),
        selected as f32 * SEG_W,
        motion::TAB_SLIDE,
        cx.reduce_motion(),
    );
    let on_pick = std::rc::Rc::new(on_pick);
    let knob = if p.light {
        p.surface_raised
    } else {
        white(0.12)
    };
    h_flex()
        .relative()
        .flex_none()
        .p(px(INSET))
        .rounded(px(8.))
        .bg(hover_wash(&p))
        .when(disabled, |d| d.opacity(0.5))
        .child(
            div()
                .absolute()
                .top(px(INSET))
                .left(px(INSET + x))
                .w(px(SEG_W))
                .h(px(SEG_H))
                .rounded(px(6.))
                .bg(knob)
                .when(p.light, |d| d.shadow_xs()),
        )
        .children(labels.iter().enumerate().map(|(ix, label)| {
            let on_pick = on_pick.clone();
            let on = ix == selected;
            div()
                .id(SharedString::from(format!("{key}:seg:{ix}")))
                .relative()
                .w(px(SEG_W))
                .h(px(SEG_H))
                .flex()
                .items_center()
                .justify_center()
                .text_size(gpui_kit::rems(12. / 16.))
                .text_color(if on { p.text } else { p.text_muted })
                .when(!disabled && !on, |d| {
                    d.cursor_pointer()
                        .hover(|d| d.text_color(p.text))
                        .on_click(move |_: &ClickEvent, window, cx| on_pick(ix, window, cx))
                })
                .child(*label)
        }))
        .into_any_element()
}

/// A row placed over a [`slide_pill`]: hover wash only, the pill carries
/// the selection.
pub fn pill_row(id: impl Into<SharedString>, active: bool, cx: &App) -> Stateful<Div> {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let id: SharedString = id.into();
    let key: SharedString = format!("prow:{id}").into();
    let h = if active { 0. } else { motion::hover_t(&key) };
    let a = motion::state_t(&format!("{key}:on"), active, s.slide, cx.reduce_motion());
    h_flex()
        .id(ElementId::Name(id))
        .relative()
        .w_full()
        .h(px(s.row_height))
        .gap_2()
        .px_2()
        .rounded(px(8.))
        .text_size(gpui_kit::rems(13. / 16.))
        .cursor_pointer()
        .bg(mix(hover_wash(&p).opacity(0.), hover_wash(&p), h))
        .text_color(mix(p.text.opacity(0.8), p.text, h.max(a)))
        .on_hover(motion::hover_listener(key, s.hover))
}

/// A toggle chip (filters, tags) with animated on/off.
pub fn chip(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on: bool,
    cx: &App,
) -> Stateful<Div> {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let id: SharedString = id.into();
    let key: SharedString = format!("chip:{id}").into();
    let h = motion::hover_t(&key);
    let a = motion::state_t(&format!("{key}:on"), on, s.slide, cx.reduce_motion());
    let label: SharedString = label.into();
    h_flex()
        .id(ElementId::Name(id))
        .role(gpui_kit::Role::Button)
        .aria_label(label.clone())
        .px_2()
        .py_0p5()
        .rounded_full()
        .text_xs()
        .cursor_pointer()
        .border_1()
        .border_color(mix(
            mix(p.border, p.border_strong, h),
            p.accent.opacity(0.8),
            a,
        ))
        .bg(mix(mix(p.hover.opacity(0.), p.hover, h), p.accent_wash, a))
        .text_color(mix(mix(p.text_muted, p.text, h), p.text, a))
        .on_hover(motion::hover_listener(key, s.hover))
        .active(|st| st.top(px(1.)))
        .relative()
        .child(label)
}

/// t3code's `⌘N` keycap, shown on rows while ⌘ is held.
pub fn shortcut_badge(index: usize, cx: &App) -> Div {
    let p = *Palette::of(cx);
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(p.border_strong)
        .bg(p.hover)
        .text_xs()
        .text_color(p.text_muted)
        .child(format!("⌘{}", index + 1))
}

/// The digit of a bare `⌘1`..`⌘9` keystroke as a 0-based index.
pub fn cmd_digit(k: &gpui_kit::Keystroke) -> Option<usize> {
    let m = &k.modifiers;
    if !m.platform || m.control || m.alt || m.shift {
        return None;
    }
    match k.key.parse::<usize>() {
        Ok(n @ 1..=9) => Some(n - 1),
        _ => None,
    }
}

/// A floating popover card that arrives on the style's spring.
pub fn popover(id: impl Into<ElementId>, cx: &App) -> impl FnOnce(Div) -> gpui_kit::AnyElement {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let id = id.into();
    move |content: Div| {
        let card = floating(&s, &p).apply(content.rounded(px(s.panel_radius.clamp(6., 12.))));
        motion::pop_in(card, id, s.pop, -6.).into_any_element()
    }
}

/// Three dots breathing in sequence: the "working" mark. A repeating
/// animation, so it costs frames only while shown, and the battery saver
/// freezes it.
pub fn pulse_dots(id: impl Into<ElementId>, color: Hsla) -> impl IntoElement {
    pulse::looping(
        id,
        Duration::from_millis(1200),
        pulse::Rate::Smooth,
        move |t| {
            h_flex()
                .gap(px(3.))
                .items_center()
                .children((0..3).map(move |i| {
                    // Each dot peaks a beat after the one before it. The lift is a
                    // relative inset, so the row never relayouts.
                    let phase = (t - i as f32 * 0.18).rem_euclid(1.);
                    let wave = (phase * std::f32::consts::TAU).sin().max(0.);
                    div()
                        .relative()
                        .size(px(5.))
                        .rounded_full()
                        .bg(color)
                        .opacity(0.25 + 0.75 * wave)
                        .top(px(-2. * wave))
                }))
        },
    )
}

/// Zeron's working glyph: a 3×3 grid of dots tinted blue, amber and pink
/// by row, with a brightness wave travelling up from the bottom edge every
/// 750 ms. One repeating animation; it costs frames only while shown, and
/// reduced motion freezes it at rest.
pub fn gradient_spinner(id: impl Into<ElementId>, cell: f32) -> impl IntoElement {
    const TINTS: [u32; 3] = [0xB6D3EF, 0xEDB185, 0xF888A0];
    let gap = cell / 2.;
    let period = Duration::from_millis(motion::GRADIENT_SPIN_MS);
    pulse::looping(id, period, pulse::Rate::Smooth, move |t| {
        v_flex()
            .gap(px(gap))
            .flex_none()
            .children((0..3).map(move |row| {
                h_flex().gap(px(gap)).children((0..3).map(move |col| {
                    // Distance from the wave origin (bottom row, centre column).
                    let d = 2. - row as f32 + (col as f32 - 1.).abs();
                    div()
                        .size(px(cell))
                        .rounded(px(cell / 2.))
                        .bg(gpui_kit::rgb(TINTS[row]))
                        .opacity(motion::gspin_opacity(t + d / 4., 0.1))
                }))
            }))
    })
}

/// A shimmer sweeping across a label while something streams in.
pub fn shimmer_text(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    let p = *Palette::of(cx);
    let text: SharedString = text.into();
    pulse::looping(
        id,
        Duration::from_millis(1600),
        pulse::Rate::Smooth,
        move |t| {
            let w = (t * std::f32::consts::TAU).sin() * 0.5 + 0.5;
            div().text_color(mix(p.text_faint, p.text, w)).child(text)
        },
    )
}

/// A small status dot that can pulse a soft halo.
pub fn status_dot(id: impl Into<ElementId>, color: Hsla, live: bool) -> impl IntoElement {
    let dot = div().size(px(6.)).rounded_full().bg(color);
    div().relative().size(px(6.)).child(dot).when(live, |d| {
        d.child(pulse::looping(
            id,
            Duration::from_millis(1800),
            pulse::Rate::Half,
            move |t| {
                let grow = 6. + 8. * t;
                div()
                    .absolute()
                    .top(px(-(grow - 6.) / 2.))
                    .left(px(-(grow - 6.) / 2.))
                    .size(px(grow))
                    .rounded_full()
                    .bg(color)
                    .opacity(0.45 * (1. - t))
            },
        ))
    })
}

/// The IronClaw mark: three claw strokes on a raised tile, lit in the
/// accent. Plain quads, so it scales with the style at no cost.
pub fn claw_mark(cx: &App) -> impl IntoElement {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let tile = 52.;
    let stroke = |h: f32, dy: f32| {
        div()
            .w(px(6.))
            .h(px(h))
            .mt(px(dy))
            .rounded_full()
            .bg(vertical(lift(p.accent, 0.12), p.accent_strong))
    };
    neutral(&s, &p, 0., 1.).apply(
        h_flex()
            .size(px(tile))
            .rounded(px(tile * 0.3))
            .items_start()
            .justify_center()
            .pt(px(12.))
            .gap(px(5.))
            .child(stroke(22., 4.))
            .child(stroke(28., 0.))
            .child(stroke(22., 4.)),
    )
}

/// Swap a snapping `.hover(|s| s.bg(..))` for a fading wash. `key` must be
/// unique among elements on screen; pass the element's own id text.
pub fn smooth_bg<E>(el: E, key: impl Into<SharedString>, cx: &App) -> E
where
    E: StatefulInteractiveElement + Styled,
{
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let key: SharedString = format!("wash:{}", key.into()).into();
    let h = motion::hover_t(&key);
    el.bg(mix(p.hover.opacity(0.), p.hover, h))
        .on_hover(motion::hover_listener(key, s.hover))
}
