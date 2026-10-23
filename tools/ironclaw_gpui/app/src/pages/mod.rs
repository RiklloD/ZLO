//! Full-width pages beside the chat: notifications, automations, files,
//! projects. Each page owns its data and reloads it when shown again.

pub mod automations;
pub mod extensions;
pub mod files;
pub mod notifications;
pub mod operator;
pub mod projects;
pub mod settings;
pub mod skills;

use crate::ui::*;

const PAGE_MAX_WIDTH: f32 = 860.;

/// What a page asks the window to do.
pub enum PageEvent {
    OpenThread(String),
    Unread(usize),
}

/// Title row with page-level actions on the right.
pub fn header(title: &'static str, actions: impl IntoElement) -> gpui_kit::Div {
    h_flex()
        .justify_between()
        .items_center()
        .pb_4()
        .child(div().text_xl().font_semibold().child(title))
        .child(h_flex().gap_2().child(actions))
}

/// Centered, width-capped page body that scrolls.
pub fn page_body(id: &'static str, content: impl IntoElement) -> impl IntoElement {
    div().id(id).size_full().overflow_y_scroll().child(
        h_flex()
            .w_full()
            .justify_center()
            .px_6()
            .py_6()
            .child(v_flex().w_full().max_w(px(PAGE_MAX_WIDTH)).child(content)),
    )
}

pub fn empty_state(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .py_12()
        .w_full()
        .text_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
}

pub fn error_line(message: &SharedString, cx: &App) -> impl IntoElement {
    div()
        .pb_3()
        .text_sm()
        .text_color(cx.theme().danger)
        .whitespace_normal()
        .child(message.clone())
}

pub fn refresh_button(id: &'static str) -> Button {
    Button::new(id)
        .icon(IconName::RefreshCw)
        .ghost()
        .small()
        .tooltip("Refresh")
        .accessibility_label("Refresh")
}
