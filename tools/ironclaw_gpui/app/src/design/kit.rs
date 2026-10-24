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
    Animation, AnimationExt, App, Background, BoxShadow, ClickEvent, Div, ElementId, Hsla,
    InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, hsla, linear_color_stop, linear_gradient,
    point, prelude::FluentBuilder, px,
};
use std::time::Duration;

use super::motion::{self, mix};
use super::styles::{Material, Style};
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

/// The neutral raised plate (secondary buttons, chips, the composer).
/// `h` is hover progress, `lit` how raised it reads at rest (0..1).
pub fn neutral(s: &Style, p: &Palette, h: f32, lit: f32) -> Plate {
    let k = lit + 0.5 * h;
    match s.material {
        Material::Metal => Plate {
            fill: vertical(
                lift(p.surface_raised, 0.03 + 0.03 * h).opacity(0.55 + 0.35 * k),
                p.surface_raised.opacity(0.4 + 0.3 * k),
            ),
            rim: white(0.06 + 0.05 * k),
            shadows: vec![
                shadow(white(0.06 + 0.05 * k), 1., 0., 0., true),
                shadow(black(0.35 * k), 1., 2., 0., false),
            ],
        },
        Material::Frost => Plate {
            fill: vertical(white(0.07 + 0.06 * k), white(0.035 + 0.05 * k)),
            rim: white(0.1 + 0.08 * k),
            shadows: vec![
                shadow(white(0.1 * k), 1., 0., 0., true),
                shadow(black(0.18 * k), 2., 6., 0., false),
            ],
        },
        Material::Ink => Plate {
            fill: if p.light {
                Background::from(mix(p.surface_card, p.surface, 0.3 * h))
            } else {
                Background::from(p.surface_raised.opacity(0.5 + 0.3 * k))
            },
            rim: p.border.opacity(0.6 + 0.4 * h),
            shadows: vec![
                shadow(black(0.05 * k), 1., 2., 0., false),
                shadow(black(0.04 * k), 4., 12., -2., false),
            ],
        },
        Material::Line => Plate {
            fill: Background::from(p.hover.opacity(0.4 * k + 0.6 * h)),
            rim: mix(p.border, p.border_strong, h),
            shadows: Vec::new(),
        },
    }
}

/// The accent plate (primary buttons, the send button). `glow` spreads a
/// colored halo, used on hover.
pub fn accent(s: &Style, p: &Palette, h: f32) -> Plate {
    let base = p.accent;
    match s.material {
        Material::Metal | Material::Frost => Plate {
            fill: vertical(lift(base, 0.07 + 0.04 * h), base),
            rim: lift(base, 0.25).opacity(0.5),
            shadows: vec![
                shadow(white(0.28), 1., 0., 0., true),
                shadow(black(0.12), -1., 0., 0., true),
                shadow(base.opacity(0.18 + 0.3 * h), 2., 6. + 12. * h, 0., false),
            ],
        },
        Material::Ink => Plate {
            fill: Background::from(mix(base, p.accent_strong, h)),
            rim: p.accent_strong,
            shadows: vec![
                shadow(base.opacity(0.22 + 0.1 * h), 2., 6. + 6. * h, 0., false),
                shadow(white(0.18), 1., 0., 0., true),
            ],
        },
        Material::Line => Plate {
            fill: Background::from(base.opacity(0.14 + 0.1 * h)),
            rim: base.opacity(0.7 + 0.3 * h),
            shadows: Vec::new(),
        },
    }
}

/// Floating surfaces: popovers, menus, the composer dock.
pub fn floating(s: &Style, p: &Palette) -> Plate {
    let e = s.elevation;
    let fill = match s.material {
        Material::Frost => p.surface_overlay.opacity(0.86),
        _ => p.surface_overlay,
    };
    let mut shadows = vec![
        shadow(
            black((if p.light { 0.08 } else { 0.3 }) * e),
            8.,
            24.,
            -4.,
            false,
        ),
        shadow(
            black((if p.light { 0.06 } else { 0.2 }) * e),
            2.,
            6.,
            0.,
            false,
        ),
    ];
    if matches!(s.material, Material::Metal | Material::Frost) {
        shadows.push(shadow(white(0.06), 1., 0., 0., true));
    }
    Plate {
        fill: Background::from(fill),
        rim: if s.material == Material::Frost {
            white(0.12)
        } else {
            p.border_strong
        },
        shadows,
    }
}

/// Elevation for the content panel (the big rounded plane).
pub fn panel_shadows(s: &Style, p: &Palette) -> Vec<BoxShadow> {
    let e = s.elevation;
    if e <= 0. {
        return Vec::new();
    }
    let mut v = vec![shadow(
        black((if p.light { 0.06 } else { 0.25 }) * e),
        4.,
        16.,
        -6.,
        false,
    )];
    if matches!(s.material, Material::Metal | Material::Frost) {
        v.push(shadow(white(0.04), 1., 0., 0., true));
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
        };
        let pad = match self.size {
            Size::XSmall => 7.,
            Size::Small => 9.,
            Size::Medium => 12.,
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
            .when(self.size != Size::Medium, |d| d.text_xs())
            .when(self.size == Size::Medium, |d| d.text_sm())
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
                d.child(match self.size {
                    Size::Medium => icon,
                    Size::Small => icon.small(),
                    Size::XSmall => icon.xsmall(),
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
    let rest = p.hover.opacity(0.);
    let wash = mix(mix(rest, p.hover, h), selected_wash(&s, &p), a);
    h_flex()
        .id(ElementId::Name(id))
        .relative()
        .w_full()
        .min_h(px(s.row_height))
        .gap_2()
        .px_2()
        .rounded(px(s.control_radius))
        .text_sm()
        .cursor_pointer()
        .bg(wash)
        .text_color(mix(p.text_muted, p.text, h.max(a)))
        .on_hover(motion::hover_listener(key, s.hover))
        .when(a > 0.01, |d| d.child(tick(&s, &p, a)))
}

/// What a selected row looks like in each style.
fn selected_wash(s: &Style, p: &Palette) -> Hsla {
    match s.material {
        Material::Line => p.accent.opacity(0.1),
        Material::Ink if p.light => p.surface_card,
        _ => p.active,
    }
}

/// The selection tick: a short accent bar that grows from the middle.
fn tick(s: &Style, p: &Palette, a: f32) -> impl IntoElement {
    let full = (s.row_height - 14.).max(8.);
    let h = full * a;
    div()
        .absolute()
        .left(px(if s.panel_inset == 0. { 0. } else { -1. }))
        .top(px((s.row_height - h) / 2.))
        .w(px(if s.material == Material::Line { 2. } else { 3. }))
        .h(px(h))
        .rounded_full()
        .bg(p.accent.opacity(a))
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
        .rounded(px(s.control_radius))
        .bg(selected_wash(&s, &p).opacity(selected_wash(&s, &p).a * shown))
        .when(s.material == Material::Ink && p.light, |d| {
            d.shadow(vec![
                shadow(black(0.06 * shown), 1., 2., 0., false),
                shadow(black(0.04 * shown), 0., 0., 1., false),
            ])
        })
        .when(
            matches!(s.material, Material::Metal | Material::Frost),
            |d| d.shadow(vec![shadow(white(0.05 * shown), 1., 0., 0., true)]),
        )
        .child(
            div()
                .absolute()
                .left(px(if s.panel_inset == 0. { 0. } else { -1. }))
                .top(px(7.))
                .bottom(px(7.))
                .w(px(if s.material == Material::Line { 2. } else { 3. }))
                .rounded_full()
                .bg(p.accent.opacity(shown)),
        )
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
        .rounded(px(s.control_radius))
        .text_sm()
        .cursor_pointer()
        .bg(mix(p.hover.opacity(0.), p.hover, h * 0.8))
        .text_color(mix(p.text_muted, p.text, h.max(a)))
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
        .rounded(px(if s.material == Material::Line {
            s.control_radius
        } else {
            999.
        }))
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

/// A floating popover card that arrives on the style's spring.
pub fn popover(id: impl Into<ElementId>, cx: &App) -> impl FnOnce(Div) -> gpui_kit::AnyElement {
    let s = *Style::of(cx);
    let p = *Palette::of(cx);
    let id = id.into();
    move |content: Div| {
        let card = floating(&s, &p).apply(content.rounded(px(s.panel_radius.clamp(6., 14.))));
        motion::pop_in(card, id, s.pop, -6.).into_any_element()
    }
}

/// Three dots breathing in sequence: the "working" mark. A repeating
/// animation, so it costs frames only while shown, and the battery saver
/// freezes it.
pub fn pulse_dots(id: impl Into<ElementId>, color: Hsla) -> impl IntoElement {
    let id = id.into();
    h_flex()
        .gap(px(3.))
        .items_center()
        .children((0..3).map(move |i| {
            div()
                .relative()
                .size(px(5.))
                .rounded_full()
                .bg(color)
                .with_animation(
                    ElementId::NamedInteger(format!("{id:?}-dot").into(), i as u64),
                    Animation::new(Duration::from_millis(1200)).repeat(),
                    move |el, t| {
                        // Each dot peaks 0.2 s after the one before it. The lift is
                        // a relative inset, so the row never relayouts.
                        let phase = (t - i as f32 * 0.18).rem_euclid(1.);
                        let wave = (phase * std::f32::consts::TAU).sin().max(0.);
                        el.opacity(0.25 + 0.75 * wave).top(px(-2. * wave))
                    },
                )
        }))
}

/// Zeron's working glyph: a 3×3 grid of dots tinted blue, amber and pink
/// by row, with a brightness wave travelling up from the bottom edge every
/// 750 ms. One repeating animation; it costs frames only while shown, and
/// reduced motion freezes it at rest.
pub fn gradient_spinner(id: impl Into<ElementId>, cell: f32) -> impl IntoElement {
    const TINTS: [u32; 3] = [0xB6D3EF, 0xEDB185, 0xF888A0];
    let id = id.into();
    let gap = cell / 2.;
    v_flex()
        .gap(px(gap))
        .flex_none()
        .children((0..3).map(move |row| {
            let id = id.clone();
            h_flex().gap(px(gap)).children((0..3).map(move |col| {
                // Distance from the wave origin (bottom row, centre column).
                let d = 2. - row as f32 + (col as f32 - 1.).abs();
                let phase = d / 4.;
                div()
                    .size(px(cell))
                    .rounded(px(cell / 2.))
                    .bg(gpui_kit::rgb(TINTS[row]))
                    .with_animation(
                        ElementId::NamedInteger(format!("{id:?}-g").into(), (row * 3 + col) as u64),
                        Animation::new(Duration::from_millis(motion::GRADIENT_SPIN_MS)).repeat(),
                        move |el, t| el.opacity(motion::gspin_opacity(t + phase, 0.1)),
                    )
            }))
        }))
}

/// A shimmer sweeping across a label while something streams in.
pub fn shimmer_text(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    let p = *Palette::of(cx);
    let text: SharedString = text.into();
    div().text_color(p.text_muted).child(text).with_animation(
        id,
        Animation::new(Duration::from_millis(1600)).repeat(),
        move |el, t| {
            let w = (t * std::f32::consts::TAU).sin() * 0.5 + 0.5;
            el.text_color(mix(p.text_faint, p.text, w))
        },
    )
}

/// A small status dot that can pulse a soft halo.
pub fn status_dot(id: impl Into<ElementId>, color: Hsla, live: bool) -> impl IntoElement {
    let dot = div().size(px(6.)).rounded_full().bg(color);
    div().relative().size(px(6.)).child(dot).when(live, |d| {
        d.child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(px(6.))
                .rounded_full()
                .bg(color)
                .with_animation(
                    id,
                    Animation::new(Duration::from_millis(1800)).repeat(),
                    move |el, t| {
                        let grow = 6. + 8. * t;
                        el.size(px(grow))
                            .top(px(-(grow - 6.) / 2.))
                            .left(px(-(grow - 6.) / 2.))
                            .opacity(0.45 * (1. - t))
                    },
                ),
        )
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
    let plate = match s.material {
        Material::Line => Plate {
            fill: Background::from(p.bg),
            rim: p.accent.opacity(0.6),
            shadows: Vec::new(),
        },
        _ => neutral(&s, &p, 0., 1.),
    };
    plate.apply(
        h_flex()
            .size(px(tile))
            .rounded(px(if s.material == Material::Line {
                s.control_radius
            } else {
                tile * 0.3
            }))
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
