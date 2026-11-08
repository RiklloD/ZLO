//! What the frame's sidebar pane holds: the inbox (search, filters, its
//! virtual list) or the settings nav, the footer, and over the titlebar
//! band the window's name, with the sidebar/panel hairline at the pane's
//! edge.
//!
//! The inbox keeps its own list rather than rune-kit's `Sidebar`: that
//! container owns its scroll viewport, so the wheel glide
//! (`design::smooth_scroll`) can't sit on it, and its arrow keys press the
//! item they focus, which in a card would be a hover button (pin, snooze,
//! settle). The settings nav is a plain source list and uses `Sidebar`,
//! `SidebarSection` and `SidebarRow`.

use rune::{Color, Cursor, Cx, El, col, el, row};
use rune_kit::shell::{Sidebar, SidebarRow, SidebarSection};
use rune_kit::{ControlSize, Kit};

use super::super::inbox::view as inbox_view;
use super::super::ui::{self, SEMIBOLD, SM, tw};
use super::super::{in_settings, settings_groups};
use super::frame::{BAND, CLUSTER_END, PAST_CLUSTER};
use crate::design::{self, Palette};
use crate::icons;
use crate::state::{Page, State};

/// The sidebar/panel hairline's width.
const HAIRLINE: f32 = 1.5;

pub(super) fn sidebar(
    s: &State,
    p: &Palette,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let settings = in_settings(s);
    let column = col()
        .key("sidebar-column")
        .w_full()
        .h_full()
        .min_h(0.0)
        .pr(HAIRLINE)
        .child(if settings {
            settings_nav(s, k)
        } else {
            inbox_view::view(s, cx, k, layer)
        })
        .children((!settings).then(|| footer(s, p)));
    // Over the frame's titlebar band: the window's name past the cluster;
    // the band's empty space drags the window.
    let title = row()
        .key("sidebar-title")
        .pin(Some(0.), Some(-BAND), Some(0.), None)
        .h(BAND)
        .pl(CLUSTER_END + PAST_CLUSTER)
        .pr(8.)
        .center_y()
        .window_drag()
        .child(tw(
            if settings { "Settings" } else { "ZLO" },
            SM,
            SEMIBOLD,
            p.text,
        ));
    let mut pane = el().key("sidebar").w_full().h_full();
    let surface = el()
        .key("sidebar-surface")
        .pin(Some(0.), Some(-BAND), Some(0.), Some(0.))
        .inert();
    pane = pane.child(if s.workspace.sidebar.is_open() {
        // Docked over a translucent window, the shell's fill alone lets
        // through far more of the blurred desktop than the panel's stacked
        // fills, and the sidebar read lighter than the panel. GPUI's reads a
        // step darker: the shell's (darker) surface at the panel's opacity.
        // Opaque, this is the shell's own fill again.
        let st = design::styles::Style::of(&s.prefs);
        surface.fill(p.surface.alpha(st.panel_alpha))
    } else {
        // Closed, the pane's contents only show peeking over the panel (or
        // sliding back out of a peek): they float on the shell's own surface.
        surface
            .fill(p.surface)
            .shadow(0., 16., Color::BLACK.alpha(0.3))
    });
    let mut pane = pane.child(column).child(title);
    if let Some(over) = crate::design::look::wallpaper_layer(s, "sidebar") {
        pane = pane.child(
            el().pin(Some(0.), Some(-BAND), Some(0.), Some(0.))
                .inert()
                .child(over),
        );
    }
    // The sidebar/panel hairline, the window's height; accent while resizing.
    pane.child(
        el().key("sidebar-hairline")
            .pin(None, Some(-BAND), Some(0.), Some(0.))
            .w(HAIRLINE)
            .inert()
            .fill(if s.workspace.sidebar.is_resizing() {
                p.accent.alpha(0.6)
            } else {
                p.border_strong
            }),
    )
}

/// Zeron's footer: the account chip on the left ("Local": this Mac's
/// gateway), icon buttons on the right at 32 px.
fn footer(s: &State, p: &Palette) -> El {
    let unread = s.workspace.unread;
    let foot = |key: &'static str, i: &'static rune::Icon, label: &'static str| {
        // The composer's round-button wash, so both hovers read the same.
        row()
            .key(key)
            .role("button")
            .label(label)
            .help(label)
            .size(design::SIDEBAR_CONTROL, design::SIDEBAR_CONTROL)
            .radius(design::SIDEBAR_CONTROL / 2.)
            .center()
            .cursor(Cursor::Pointer)
            .hover(if p.light {
                Color::BLACK.alpha(0.08)
            } else {
                Color::WHITE.alpha(0.10)
            })
            .child(ui::glyph(i, 16., p.text_muted))
    };
    let mut bell = el().child(
        foot("open-notifications", &icons::BELL, "Notifications")
            .on_press(|s: &mut State| super::super::show(s, Page::Notifications)),
    );
    if unread > 0 {
        bell = bell.child(
            el().pin(None, Some(5.), Some(5.), None)
                .size(7., 7.)
                .radius(3.5)
                .fill(p.accent),
        );
    }
    row()
        .key("sidebar-footer")
        .w_full()
        .padding(8.)
        .gap(4.)
        .center_y()
        .child(
            row()
                .key("account-local")
                .gap(8.)
                .px(6.)
                .h(design::SIDEBAR_CONTROL)
                .center_y()
                .radius(8.)
                .child(
                    row()
                        .size(18., 18.)
                        .radius(9.)
                        .fill(p.accent)
                        .center()
                        .child(tw("L", 10., SEMIBOLD, p.on_accent())),
                )
                .child(ui::side_t(
                    "Local",
                    design::SIDEBAR_TITLE_SIZE,
                    p.text.alpha(0.8),
                )),
        )
        .child(el().grow(1.0))
        .child(bell)
        .child(
            foot("open-usage", &icons::CHART_PIE, "Usage")
                .on_press(|s: &mut State| super::super::show(s, Page::Usage)),
        )
        .child(
            foot("open-browser", &icons::EXTERNAL_LINK, "Open in browser").on_press_tx(
                |s: &mut State, tx: &mut rune::Tx| super::super::open_in_browser(s, tx),
            ),
        )
        .child(
            foot("open-settings", &icons::SETTINGS, "Settings")
                .on_press(|s: &mut State| super::super::show(s, Page::Settings)),
        )
}

/// The settings pages as a source list: a section per group, a row per
/// page (its glyph in the accent while open, Notifications with its unread
/// count), Back to the threads under it. Up and Down step through the pages.
fn settings_nav(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let unread = s.workspace.unread;
    let mut nav = Sidebar::new().label("Settings").inset(8.).row_gap(0.);
    for (gx, (heading, group)) in settings_groups(s).into_iter().enumerate() {
        let rows = group.into_iter().map(|(page, icon, label)| {
            let on = s.page == page;
            let mut r = SidebarRow::new(label)
                .avatar(ui::glyph(
                    icon,
                    16.,
                    if on { p.accent } else { p.text_muted },
                ))
                .height(design::ROW_HEIGHT)
                .title_style(rune::TextStyle::new(design::styles::rem(SM), ui::REGULAR))
                .selected(on)
                .on_select(move |s: &mut State, _| super::super::show(s, page));
            if page == Page::Notifications && unread > 0 {
                r = r.trailing(
                    row()
                        .h(18.)
                        .px(6.)
                        .radius(9.)
                        .center()
                        .fill(p.accent)
                        .child(tw(unread.to_string(), 11., SEMIBOLD, p.on_accent())),
                );
            }
            let built = r.build(k).key(format!("nav-{label}"));
            if on {
                built.fill(ui::selected_wash(p))
            } else {
                built
            }
        });
        nav = nav.section(
            SidebarSection::new(heading)
                .key(format!("nav-group-{gx}"))
                .rows(rows.collect::<Vec<_>>()),
        );
    }
    let back = rune_kit::Button::new("Back")
        .variant(rune_kit::ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .icon(&icons::ARROW_LEFT)
        .on_press(super::super::back_to_threads)
        .build(k);
    el().key("settings-nav")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(nav.footer(row().child(back)).build(k))
}
