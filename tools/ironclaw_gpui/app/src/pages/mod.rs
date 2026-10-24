//! Full-width pages beside the chat: notifications, automations, files,
//! projects. Each page owns its data and reloads it when shown again.

pub mod automations;
pub mod extensions;
pub mod files;
pub mod notifications;
pub mod operator;
pub mod profiles;
pub mod projects;
pub mod providers;
pub mod settings;
pub mod skills;
pub mod usage;

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
        .pb_5()
        .child(div().text_xl().font_semibold().child(title))
        .child(h_flex().gap_2().child(actions))
}

/// A settings group's header, Zeron-style: a muted label above its card,
/// with an optional line of explanation.
pub fn group_header(title: impl Into<SharedString>, note: Option<&str>, cx: &App) -> gpui_kit::Div {
    let p = crate::design::Palette::of(cx);
    v_flex()
        .pt_6()
        .pb_2()
        .px_1()
        .gap_0p5()
        .child(
            div()
                .text_sm()
                .font_medium()
                .text_color(p.text_muted)
                .child(title.into()),
        )
        .when_some(note, |d, note| {
            d.child(
                div()
                    .text_xs()
                    .text_color(p.text_faint)
                    .whitespace_normal()
                    .child(note.to_string()),
            )
        })
}

/// Related settings in one card, hairlines between rows (inset like
/// macOS System Settings).
pub fn rows_card(rows: impl IntoIterator<Item = AnyElement>, cx: &App) -> gpui_kit::Div {
    let s = crate::design::Style::of(cx);
    let p = crate::design::Palette::of(cx);
    let mut card = crate::design::kit::neutral(s, p, 0., 0.5).apply(
        v_flex()
            .w_full()
            .rounded(px(s.panel_radius.clamp(4., 12.)))
            .overflow_hidden(),
    );
    for (ix, row) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(div().mx_4().h(px(1.)).bg(p.border));
        }
        card = card.child(row);
    }
    card
}

/// One setting: label and description on the left, its control on the right.
pub fn setting_row(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let p = crate::design::Palette::of(cx);
    h_flex()
        .w_full()
        .min_h(px(56.))
        .px_4()
        .py_3()
        .gap_4()
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_medium().text_color(p.text).child(label.into()))
                .when_some(description, |d, text| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(p.text_muted)
                            .whitespace_normal()
                            .child(text),
                    )
                }),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

/// Centered, width-capped page body that scrolls.
pub fn page_body(id: &'static str, content: impl IntoElement) -> impl IntoElement {
    div().id(id).size_full().overflow_y_scroll().child(
        h_flex()
            .w_full()
            .justify_center()
            .px_8()
            .pt_8()
            .pb_12()
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
