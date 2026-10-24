//! The app's only door into gpui-kit.
//!
//! GPUI and gpui-kit are pre-1.0 and change weekly. Screens import from here,
//! never from `gpui_kit` directly, so a version bump is fixed in this file.

pub use gpui_kit::component::button::{Button, ButtonVariants};
pub use gpui_kit::component::input::{
    Input, InputEvent, InputState, Position, Textarea, TextareaState,
};
pub use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
pub use gpui_kit::component::spinner::Spinner;
pub use gpui_kit::component::switch::Switch;
pub use gpui_kit::component::tag::Tag;
pub use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt, h_flex, v_flex,
};
pub use gpui_kit::prelude::FluentBuilder;
pub use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, EventEmitter, ExternalPaths, Global,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, PathPromptOptions, Render, Role,
    SharedString, StatefulInteractiveElement, Styled, Subscription, SystemNotification,
    SystemNotificationResponse, Task, WeakEntity, Window, div, px,
};

use gpui_kit::component::text::TextView;

/// A right-aligned bubble for the person's own message, painted from the
/// active design style. A pending message rises in and stays dimmed until
/// the gateway accepts it.
pub fn user_bubble(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    pending: bool,
    cx: &App,
) -> AnyElement {
    use crate::design::styles::{Material, Style};
    let s = *Style::of(cx);
    let p = *crate::design::Palette::of(cx);
    let (fill, rim, ink): (gpui_kit::Background, _, _) = match s.material {
        Material::Metal => (
            gpui_kit::linear_gradient(
                180.,
                gpui_kit::linear_color_stop(crate::design::lift(p.surface_raised, 0.04), 0.),
                gpui_kit::linear_color_stop(p.surface_raised, 1.),
            ),
            gpui_kit::hsla(0., 0., 1., 0.08),
            p.text,
        ),
        Material::Frost => (
            gpui_kit::Background::from(p.accent.opacity(0.22)),
            p.accent.opacity(0.35),
            p.text,
        ),
        Material::Ink => (
            gpui_kit::Background::from(p.accent),
            p.accent_strong,
            crate::design::on_accent(p),
        ),
        Material::Line => (
            gpui_kit::Background::from(p.hover),
            p.border_strong,
            p.text,
        ),
    };
    let bubble = div()
        .max_w(gpui_kit::relative(0.78))
        .px(px(14.))
        .py(px(9.))
        .rounded(px(s.bubble_radius))
        .when(s.material != Material::Line, |d| d.rounded_br(px(s.bubble_radius.min(6.))))
        .bg(fill)
        .border_1()
        .border_color(rim)
        .text_color(ink)
        .whitespace_normal()
        .when(s.material == Material::Line, |d| {
            d.child(div().text_color(p.accent).child("> "))
                .flex()
                .flex_row()
        })
        .child(text.into());
    let el = h_flex()
        .w_full()
        .justify_end()
        .child(bubble.when(pending, |d| d.opacity(0.6)));
    if pending {
        crate::design::motion::fade_in(el, id, s.enter, 8.).into_any_element()
    } else {
        el.into_any_element()
    }
}

/// Assistant Markdown. `id` must stay stable while text streams in, so the
/// view appends deltas instead of re-parsing (and fades in new words).
pub fn markdown(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    streaming: bool,
) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        .stream_fade(streaming)
}

/// One muted line of run activity (thinking, tool call, skill activation).
pub fn activity_line(
    icon: IconName,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    cx: &App,
) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_2()
        .items_start()
        .text_xs()
        .text_color(muted)
        .child(div().pt(px(1.)).child(Icon::new(icon).xsmall()))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().font_medium().child(title.into()))
                .when_some(detail, |this, detail| {
                    // line_clamp counts per paragraph; fold to one so the clamp holds.
                    let folded = detail.split_whitespace().collect::<Vec<_>>().join(" ");
                    this.child(div().line_clamp(3).child(folded))
                }),
        )
}

/// A raised card used for approval prompts and run notices, in the
/// active style's material.
pub fn card(cx: &App) -> gpui_kit::Div {
    use crate::design::styles::Style;
    let s = Style::of(cx);
    let p = crate::design::Palette::of(cx);
    crate::design::kit::neutral(s, p, 0., 0.5).apply(
        v_flex()
            .gap_2()
            .p_3()
            .rounded(px(s.panel_radius.clamp(4., 12.))),
    )
}
