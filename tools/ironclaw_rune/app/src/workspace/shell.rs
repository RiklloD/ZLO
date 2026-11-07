//! The window's drawing (GPUI `Workspace::render` and its helpers): the
//! loading and connection-error screens; then Zeron's shell of full-height
//! columns — the sidebar (inbox or settings nav, titlebar strip, footer), its
//! resize edge, and the main panel under the breadcrumb — with the status
//! bar below, the wallpaper layers over each area (area G draws them), and
//! the ⌘K menu and dialogs above everything.

use rune::{Color, Cursor, Cx, El, Phase, col, el, row};
use rune_kit::{ControlSize, Dialog, Kit};

use super::inbox::view as inbox_view;
use super::ui::{self, MEDIUM, SEMIBOLD, SM, XS, t, tw};
use super::{active_id, command, context, draft, settings_groups, settings_pages, status};
use crate::design::{self, Palette};
use crate::icons;
use crate::state::{Conn, Page, State};

/// Room the titlebar cluster takes: the traffic lights, then the sidebar
/// button (GPUI `panels::CLUSTER_END`).
const TOGGLE_SIZE: f32 = 28.;
const CLUSTER_END: f32 = design::TRAFFIC_LIGHTS_WIDTH + TOGGLE_SIZE;

pub(super) fn view(s: &State, cx: &mut Cx<State>) -> El {
    // Follow the system appearance. Only the view can read it (`Cx`), so a
    // change is queued back into the state: `s.system_dark` and
    // `s.palette` stay current for every reader (⇧⌘Y, pages, wallpaper).
    // This frame already paints with the new palette.
    let dark = cx.system_dark();
    if dark != s.system_dark {
        s.rt.handle.update(move |s, _| apply_system_dark(s, dark));
    }
    let p = if dark == s.system_dark {
        s.palette
    } else {
        design::resolve(&s.prefs, dark)
    };
    let k = design::kit(&p);
    let mut layer: Vec<El> = Vec::new();
    let body = match &s.conn {
        Conn::Connecting => loading(s, &p),
        Conn::Failed { message, retry_in } => failed(&p, &k, message, *retry_in),
        Conn::Ready => ready(s, &p, cx, &k, &mut layer),
    };
    let mut root = col()
        .key("window")
        .w_full()
        .h_full()
        .fill(p.surface)
        .on_pointer_move(|s: &mut State, _| crate::design::look::input(s))
        .child(el().w_full().grow(1.0).min_h(0.0).child(body));
    if !s.prefs.status_bar_hidden {
        root = root.child(status::bar(s, &k, &mut layer));
    }
    if let Some(over) = crate::design::look::wallpaper_layer(s, "window") {
        root = root.child(over);
    }
    command::view(s, &k, &mut layer);
    if s.workspace.quick_fixture {
        layer.push(
            row()
                .pin(Some(0.), Some(120.), Some(0.), None)
                .justify_center()
                .child(super::quick::fixture_card(s)),
        );
    }
    confirm_delete(s, &k, &mut layer);
    rune_kit::root(root, layer)
}

/// Launch screen while the gateway boots: the claw mark breathing on the
/// shell, its caption fading in late so a fast start shows no text at all.
fn loading(s: &State, p: &Palette) -> El {
    col()
        .key("loading")
        .w_full()
        .h_full()
        .center()
        .gap(16.)
        .child(
            crate::design::kit::claw_mark(s).cycle(rune::Cycle {
                period: 1.4,
                phase: 0.,
                stops: &[(0.0, 0.55), (0.5, 1.0), (1.0, 0.55)],
                still: 1.0,
                fps: 30.0,
            }),
        )
        .child(
            t("Starting IronClaw", SM, p.text_muted).key("loading-caption"),
        )
}

fn failed(p: &Palette, k: &Kit, message: &str, retry_in: Option<u64>) -> El {
    let mut c = col()
        .key("failed")
        .w_full()
        .h_full()
        .center()
        .gap(12.)
        .padding(32.)
        .child(tw("Can't connect to IronClaw", 16., SEMIBOLD, p.text))
        .child(el().w(520.).child(t(message.to_string(), SM, p.text_muted).wrap()));
    if let Some(secs) = retry_in {
        c = c.child(t(format!("Trying again in {secs} s"), XS, p.text_faint));
    }
    c.child(
        row()
            .gap(8.)
            .child(
                rune_kit::Button::new("Try again")
                    .variant(rune_kit::ButtonVariant::Contrast)
                    .on_press(super::retry_connect)
                    .build(k),
            )
            .child(
                // The gateway is down, so its in-app log ring is out of
                // reach: open the files instead.
                rune_kit::Button::new("Logs")
                    .icon(&icons::FILE)
                    .help("Open the app and gateway logs in Console")
                    .on_press(|_: &mut State| crate::logs::open_files())
                    .build(k),
            ),
    )
}

/// The sidebar's width: dragged, saved, or the default, within bounds.
fn sidebar_width(s: &State) -> f32 {
    let saved = s.prefs.sidebar_width;
    let width = s.workspace.sidebar_drag.unwrap_or(if saved > 0. {
        saved
    } else {
        design::SIDEBAR_WIDTH
    });
    width.clamp(design::SIDEBAR_MIN_WIDTH, design::SIDEBAR_MAX_WIDTH)
}

fn ready(s: &State, p: &Palette, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let shown = !s.prefs.sidebar_hidden || s.workspace.sidebar_drag.is_some();
    let width = sidebar_width(s);
    let drawn = if shown { width } else { 0. };
    let main = match s.page {
        Page::Chat => match context::banner(s, k) {
            Some(banner) if s.chat.is_some() => col()
                .w_full()
                .h_full()
                .child(banner)
                .child(el().w_full().grow(1.0).min_h(0.0).child(crate::chat::view(s, cx, k, layer))),
            _ if s.chat.is_some() => crate::chat::view(s, cx, k, layer),
            _ => empty(s, p, k),
        },
        Page::Draft => draft::view(s, cx, k, layer),
        // Appearance floats over the chat at 85 % so a pick previews live on
        // the real thread or new-chat screen.
        Page::Appearance => {
            let under = if s.chat.is_some() {
                crate::chat::view(s, cx, k, layer)
            } else {
                empty(s, p, k)
            };
            el().w_full()
                .h_full()
                .child(el().pin(Some(0.), Some(0.), Some(0.), Some(0.)).inert().child(under))
                .child(
                    el().pin(Some(0.), Some(0.), Some(0.), Some(0.))
                        .fill(p.bg.alpha(0.85))
                        .child(crate::pages::view(s, s.page, cx, k, layer)),
                )
        }
        page => crate::pages::view(s, page, cx, k, layer),
    };
    // Replays the entrance on every page or thread switch.
    let main_key = match (s.page, active_id(s)) {
        (Page::Chat, Some(id)) => format!("main-chat-{id}"),
        (page, _) => format!("main-{page:?}"),
    };
    let mut panel = col()
        .key("panel")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .fill(p.bg.alpha(crate::design::styles::Style::of(&s.prefs).panel_alpha))
        .clip_rounded(0.)
        .child(breadcrumb(s, p, drawn, cx, k, layer))
        .child(
            el().key(main_key)
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .transition(rune::Transition::opacity())
                .child(main),
        );
    for area in ["conversation", "pane"] {
        if let Some(over) = crate::design::look::wallpaper_layer(s, area) {
            panel = panel.child(over);
        }
    }
    let mut shell = row().key("shell").w_full().h_full().min_h(0.0);
    if shown {
        let mut sidebar = el()
            .key("sidebar-clip")
            .w(width)
            .h_full()
            .child(sidebar(s, p, cx, k, layer));
        if let Some(over) = crate::design::look::wallpaper_layer(s, "sidebar") {
            sidebar = sidebar.child(over);
        }
        shell = shell
            .child(sidebar)
            .child(sidebar_edge(s, p))
            .child(
                // The sidebar/panel hairline; accent while resizing.
                el().w(1.5).h_full().fill(if s.workspace.sidebar_drag.is_some() {
                    p.accent.alpha(0.6)
                } else {
                    p.border_strong
                }),
            );
    }
    // The window's sidebar button sits above both columns, so it never
    // moves when the sidebar folds.
    el().key("ready")
        .w_full()
        .h_full()
        .child(shell.child(panel))
        .child(
            row()
                .pin(Some(design::TRAFFIC_LIGHTS_WIDTH), Some(0.), None, None)
                .h(design::TITLEBAR_HEIGHT)
                .center_y()
                .child(sidebar_toggle(s, p)),
        )
}

/// Zeron's sidebar button: folds the left sidebar away or brings it back;
/// the choice survives a relaunch.
fn sidebar_toggle(s: &State, p: &Palette) -> El {
    let open = !s.prefs.sidebar_hidden;
    let label = if open { "Hide sidebar" } else { "Show sidebar" };
    // The glyph: a window outline with its sidebar column filled when open.
    let glyph = row()
        .size(16., 13.)
        .radius(3.)
        .border(1.4, p.text_muted)
        .child(
            el().w(5.)
                .h_full()
                .fill(if open { p.text_muted.alpha(0.6) } else { Color::TRANSPARENT }),
        );
    row()
        .key("sidebar-toggle")
        .role("button")
        .label(label)
        .help(label)
        .size(TOGGLE_SIZE, TOGGLE_SIZE)
        .radius(design::CONTROL_RADIUS)
        .center()
        .cursor(Cursor::Pointer)
        .hover(p.hover)
        .child(glyph)
        .on_press(super::toggle_sidebar)
}

/// A thin strip on the sidebar's right edge: drag it to resize the sidebar,
/// double-click it to restore the default width.
fn sidebar_edge(_s: &State, _p: &Palette) -> El {
    el().key("sidebar-edge")
        .w(6.)
        .ml(-3.)
        .mr(-3.)
        .h_full()
        .cursor(Cursor::ResizeColumn)
        .on_drag(|s: &mut State, drag, _tx| match drag.phase {
            Phase::Down => {
                let start = sidebar_width(s);
                drag.data = start;
                s.workspace.sidebar_drag = Some(start);
            }
            Phase::Move => {
                let width = (drag.data + drag.dx)
                    .clamp(design::SIDEBAR_MIN_WIDTH, design::SIDEBAR_MAX_WIDTH);
                s.workspace.sidebar_drag = Some(width);
            }
            Phase::Up => {
                if let Some(width) = s.workspace.sidebar_drag.take() {
                    s.update_prefs(|p| p.sidebar_width = width);
                }
            }
        })
        .on_double_press(|s: &mut State| {
            s.workspace.sidebar_drag = None;
            s.update_prefs(|p| p.sidebar_width = 0.);
        })
}

fn sidebar(s: &State, p: &Palette, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let settings = super::in_settings(s);
    // Titlebar strip: the traffic lights and the window's sidebar button
    // float over its left end, the name follows them; the empty space drags
    // the window.
    let titlebar = row()
        .key("sidebar-titlebar")
        .w_full()
        .h(design::TITLEBAR_HEIGHT)
        .pl(CLUSTER_END + 10.)
        .pr(8.)
        .center_y()
        .window_drag()
        .child(tw(if settings { "Settings" } else { "ZLO" }, SM, SEMIBOLD, p.text));
    col()
        .key("sidebar")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(titlebar)
        .child(if settings {
            settings_nav(s, p)
        } else {
            inbox_view::view(s, cx, k, layer)
        })
        .child(if settings {
            row()
                .padding(8.)
                .child(
                    rune_kit::Button::new("Back")
                        .variant(rune_kit::ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .icon(&icons::ARROW_LEFT)
                        .on_press(super::back_to_threads)
                        .build(k),
                )
        } else {
            footer(s, p, k)
        })
}

/// Zeron's footer: the account chip on the left ("Local": this Mac's
/// gateway), icon buttons on the right at 32 px.
fn footer(s: &State, p: &Palette, _k: &Kit) -> El {
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
            .on_press(|s: &mut State| super::show(s, Page::Notifications)),
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
                .child(t("Local", design::SIDEBAR_TITLE_SIZE, p.text.alpha(0.8))),
        )
        .child(el().grow(1.0))
        .child(bell)
        .child(
            foot("open-usage", &icons::CHART_PIE, "Usage")
                .on_press(|s: &mut State| super::show(s, Page::Usage)),
        )
        .child(
            foot("open-browser", &icons::EXTERNAL_LINK, "Open in browser")
                .on_press_tx(|s: &mut State, tx: &mut rune::Tx| super::open_in_browser(s, tx)),
        )
        .child(
            foot("open-settings", &icons::SETTINGS, "Settings")
                .on_press(|s: &mut State| super::show(s, Page::Settings)),
        )
}

fn settings_nav(s: &State, p: &Palette) -> El {
    const GROUP_GAP: f32 = 10.;
    const HEADING: f32 = 26.;
    let unread = s.workspace.unread;
    let mut nav = col().key("settings-nav-list").w_full().px(8.).pb(8.);
    for (gx, (heading, group)) in settings_groups(s).into_iter().enumerate() {
        let mut rows = col()
            .key(format!("nav-group-{gx}"))
            .w_full()
            .child(
                row()
                    .h(HEADING)
                    .items_end()
                    .pb(6.)
                    .px(10.)
                    .child(tw(heading, XS, MEDIUM, p.text_faint)),
            );
        if gx > 0 {
            rows = rows.mt(GROUP_GAP);
        }
        for (page, icon, label) in group {
            let on = s.page == page;
            let mut r = row()
                .key(format!("nav-{label}"))
                .role("button")
                .label(label)
                .w_full()
                .h(design::ROW_HEIGHT)
                .px(8.)
                .gap(8.)
                .center_y()
                .radius(8.)
                .cursor(Cursor::Pointer)
                .fill(if on { ui::selected_wash(p) } else { Color::TRANSPARENT })
                .hover(if on { Color::TRANSPARENT } else { ui::hover_wash(p) })
                .child(ui::glyph(icon, 16., if on { p.accent } else { p.text_muted }))
                .child(el().grow(1.0).child(t(label, SM, if on { p.text } else { p.text_muted })))
                .on_press(move |s: &mut State| super::show(s, page));
            if page == Page::Notifications && unread > 0 {
                r = r.child(
                    row()
                        .h(18.)
                        .px(6.)
                        .radius(9.)
                        .center()
                        .fill(p.accent)
                        .child(tw(unread.to_string(), 11., SEMIBOLD, p.on_accent())),
                );
            }
            rows = rows.child(r);
        }
        nav = nav.child(rows);
    }
    col()
        .key("settings-nav")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(rune::ScrollSpec { stick_bottom: false })
        .child(nav)
}

/// The panel's top strip: where you are, t3code's "project / thread", and
/// the open thread's actions at its right end. `sidebar` is the drawn
/// sidebar width: the crumbs keep clear of the window's titlebar buttons
/// while the sidebar is folded.
fn breadcrumb(
    s: &State,
    p: &Palette,
    sidebar: f32,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let (parent, leaf): (String, String) = match s.page {
        Page::Chat => {
            let id = active_id(s).unwrap_or("");
            let row_data = s.workspace.inbox.row(id);
            let profile = row_data
                .and_then(|r| s.workspace.inbox.profile(&r.profile_id))
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| "Default".into());
            let title = row_data
                .map(|r| super::row_title(r).to_string())
                .or_else(|| crate::titles::get(s, id))
                .unwrap_or_else(|| "New conversation".into());
            (profile, title)
        }
        Page::Draft => (draft::profile_name(s), "New thread".into()),
        page => {
            let label = settings_pages(s)
                .into_iter()
                .find(|(p, _, _)| *p == page)
                .map_or("Settings", |(_, _, l)| l);
            ("Settings".into(), label.into())
        }
    };
    let actions = match s.page {
        Page::Chat if s.chat.is_some() => crate::chat::titlebar_actions(s, cx, k, layer),
        _ => None,
    };
    // Zeron's titlebar identity: the thread title in the 13 px UI face, its
    // home quieter beside it; no rule, the content fades under it.
    let left = (CLUSTER_END + 10. - sidebar).max(16.);
    let right = if actions.is_some() { 0. } else { 16. };
    let mut strip = row()
        .key("breadcrumb")
        .w_full()
        .h(design::TITLEBAR_HEIGHT)
        .pl(left)
        .pr(right)
        .center_y()
        .window_drag()
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .gap(6.)
                .center_y()
                .child(t(parent, 13., p.text_muted))
                .child(t("/", 13., p.text_faint))
                .child(el().min_w(0.0).child(tw(leaf, 13., MEDIUM, p.text).truncate())),
        );
    if let Some(actions) = actions {
        strip = strip.child(actions);
    }
    strip
}

/// No thread open: the mark, a line and one button.
fn empty(s: &State, p: &Palette, k: &Kit) -> El {
    col()
        .key("empty-state")
        .w_full()
        .h_full()
        .center()
        .gap(16.)
        .child(crate::design::kit::claw_mark(s))
        .child(
            col()
                .items_center()
                .gap(6.)
                .child(tw("What are we building?", 26., SEMIBOLD, p.text))
                .child(t(
                    "Start a conversation, or pick one from the sidebar.",
                    SM,
                    p.text_muted,
                )),
        )
        .child(
            rune_kit::Button::new("New thread")
                .variant(rune_kit::ButtonVariant::Contrast)
                .icon(&icons::PLUS)
                .on_press(|s: &mut State| draft::open(s))
                .build(k),
        )
}

/// Deleting from the menu asks first: a right-click misfires more than the
/// settled row's trash button.
fn confirm_delete(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(id) = s.workspace.confirm_delete.clone() else {
        return;
    };
    let title = s
        .workspace
        .inbox
        .row(&id)
        .map_or("this conversation", super::row_title)
        .to_string();
    let delete_id = id.clone();
    Dialog::new("confirm-delete", format!("Delete \"{title}\"?"))
        .open(true)
        .fit()
        .width(400.)
        .description("The conversation leaves the inbox and can't be opened again.")
        .child(
            row()
                .gap(8.)
                .justify_end()
                .child(
                    rune_kit::Button::new("Cancel")
                        .on_press(|s: &mut State| s.workspace.confirm_delete = None)
                        .build(k),
                )
                .child(
                    rune_kit::Button::new("Delete")
                        .variant(rune_kit::ButtonVariant::Danger)
                        .on_press(move |s: &mut State| {
                            s.workspace.confirm_delete = None;
                            super::delete_thread(s, delete_id.clone());
                        })
                        .build(k),
                ),
        )
        .on_close(|s: &mut State| s.workspace.confirm_delete = None)
        .build(k, layer);
}

/// The system appearance flipped: re-resolve the palette (it matters only
/// for the "system" scheme, but `system_dark` is always kept true to macOS)
/// and repaint the wallpaper for the new side.
fn apply_system_dark(s: &mut State, dark: bool) {
    if s.system_dark == dark {
        return;
    }
    s.system_dark = dark;
    let palette = design::resolve(&s.prefs, dark);
    let flipped = palette.light != s.palette.light;
    s.palette = palette;
    if flipped {
        crate::design::look::reload(s);
    }
}
