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

use gpui_kit::component::bubble::{Bubble, BubbleContent, BubbleVariant};
use gpui_kit::component::message::MessageAlignment;
use gpui_kit::component::text::TextView;

/// A right-aligned bubble for the person's own message.
pub fn user_bubble(text: impl Into<SharedString>, pending: bool) -> impl IntoElement {
    Bubble::new()
        .alignment(MessageAlignment::End)
        .with_variant(BubbleVariant::Secondary)
        .content(BubbleContent::new().child(text.into()))
        .when(pending, |this| this.opacity(0.6))
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

/// A bordered card used for approval prompts and run notices.
pub fn card(cx: &App) -> gpui_kit::Div {
    v_flex()
        .gap_2()
        .p_3()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
}
