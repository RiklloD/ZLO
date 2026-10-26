//! The app's only door into gpui-kit.
//!
//! GPUI and gpui-kit are pre-1.0 and change weekly. Screens import from here,
//! never from `gpui_kit` directly, so a version bump is fixed in this file.

// Our animated button replaces gpui-kit's under the same name and builder
// methods, so every screen picks up the design kit's motion.
pub use crate::design::kit::Button;
pub use gpui_kit::component::input::{
    InlineToken, InlineTokenContext, Input, InputContent, InputEvent, InputState, Position,
    Textarea, TextareaState,
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
    AnyElement, App, AppContext, ClipboardEntry, ClipboardItem, Context, ElementId, Entity,
    EventEmitter, ExternalPaths, Global, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, PathPromptOptions, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Subscription, SystemNotification,
    SystemNotificationResponse, Task, WeakEntity, Window, div, px,
};
/// Drawing primitives for the computer-use viewer (frames, cursor, ripples).
pub use gpui_kit::{Bounds, PathBuilder, Pixels, RenderImage, canvas, img, point, rgb, rgba};

use gpui_kit::component::text::TextView;

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

/// One line of run activity (thinking, tool call, skill activation): the
/// icon sits in a small tile, the detail folds to three lines.
pub fn activity_line(
    icon: IconName,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    cx: &App,
) -> impl IntoElement {
    let p = crate::design::Palette::of(cx);
    let s = crate::design::Style::of(cx);
    h_flex()
        .gap_2()
        .items_start()
        .text_xs()
        .text_color(p.text_muted)
        .child(
            div()
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(s.control_radius.min(6.)))
                .bg(p.hover)
                .border_1()
                .border_color(p.border)
                .child(Icon::new(icon).xsmall()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .pt(px(3.))
                .gap_0p5()
                .child(div().font_medium().text_color(p.text).child(title.into()))
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
