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
pub mod usage_history;

use crate::ui::*;

/// Zeron's settings column (`widgets::page_column`).
const PAGE_MAX_WIDTH: f32 = 760.;

/// What a page asks the window to do.
pub enum PageEvent {
    OpenThread(String),
    Unread(usize),
}

/// Title row with page-level actions on the right: Zeron's 20 px medium
/// page title, inset 8 px like its section labels.
pub fn header(title: &'static str, actions: impl IntoElement) -> gpui_kit::Div {
    h_flex()
        .justify_between()
        .items_center()
        .px(px(8.))
        .pb(px(8.))
        .child(
            div()
                .text_size(gpui_kit::rems(20. / 16.))
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(title),
        )
        .child(h_flex().gap_2().child(actions))
}

/// A settings group's label, Zeron's `section_label`: 13 px muted text,
/// inset 8 px, 32 px above the group, with an optional line of explanation.
pub fn group_header(title: impl Into<SharedString>, note: Option<&str>, cx: &App) -> gpui_kit::Div {
    let p = crate::design::Palette::of(cx);
    v_flex()
        .pt(px(32.))
        .pb(px(8.))
        .px(px(8.))
        .gap(px(2.))
        .child(
            div()
                .text_size(gpui_kit::rems(13. / 16.))
                .line_height(gpui_kit::rems(17. / 16.))
                .text_color(p.text_muted)
                .child(title.into()),
        )
        .when_some(note, |d, note| {
            d.child(
                div()
                    .text_size(gpui_kit::rems(12. / 16.))
                    .text_color(p.text_faint)
                    .whitespace_normal()
                    .child(note.to_string()),
            )
        })
}

/// Zeron's block fill: a faint wash of the theme's ink, no outline.
pub fn block_fill(cx: &App) -> gpui_kit::Hsla {
    let p = crate::design::Palette::of(cx);
    if p.light {
        gpui_kit::hsla(0., 0., 0., 0.045)
    } else {
        gpui_kit::hsla(0., 0., 1., 0.045)
    }
}

/// Related settings in one block: 12 px radius, the wash fill, hairlines
/// between rows inset 16 px.
pub fn rows_card(rows: impl IntoIterator<Item = AnyElement>, cx: &App) -> gpui_kit::Div {
    let p = crate::design::Palette::of(cx);
    let mut card = v_flex()
        .w_full()
        .rounded(px(12.))
        .bg(block_fill(cx))
        .overflow_hidden();
    for (ix, row) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(div().mx(px(16.)).h(px(1.)).bg(p.border.opacity(0.6)));
        }
        card = card.child(row);
    }
    card
}

/// One setting, Zeron's `card_row`: a 13 px medium title and a 12 px muted
/// description on the left, the control on the right, at least 60 px tall.
pub fn setting_row(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let p = crate::design::Palette::of(cx);
    h_flex()
        .w_full()
        .min_h(px(60.))
        .px(px(16.))
        .py(px(12.))
        .gap(px(16.))
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w(px(160.))
                .gap(px(1.))
                .child(
                    div()
                        .text_size(gpui_kit::rems(13. / 16.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child(label.into()),
                )
                .when_some(description, |d, text| {
                    d.child(
                        div()
                            .text_size(gpui_kit::rems(12. / 16.))
                            .text_color(p.text_muted)
                            .line_clamp(2)
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
            .px(px(40.))
            .pt(px(28.))
            .pb(px(48.))
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
