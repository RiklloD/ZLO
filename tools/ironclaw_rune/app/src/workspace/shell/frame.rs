//! Zeron's window frame on rune-kit's app shell: a `SplitView` holds the
//! sidebar pane (its width and open state in `s.workspace.sidebar`, a
//! `PaneState` mirrored to prefs), the main panel under the breadcrumb
//! toolbar, the titlebar band and the sidebar's resize grip, and lets the
//! closed sidebar peek over the panel. ZLO's look rides on a kit made for
//! the frame: no frame inset, a square panel without a border, the panel's
//! and the shell's own fills.
//!
//! Kept ZLO's own: the sidebar button (Zeron's morphing glyph with the
//! titlebar toggles' active fill, fixed past the traffic lights) floats over
//! the frame as before, and the panes slide on [`panels::REVEAL`].

use std::time::Duration;

use rune::{Color, Cx, El, el, row};
use rune_kit::Kit;
use rune_kit::shell::{PaneMotion, SidebarPeek, SidebarToggle, SplitView, TOOLBAR_HEIGHT, Toolbar};

use super::super::ui::{MEDIUM, t, tw};
use super::super::{active_id, draft, settings_pages};
use super::main_area::{chat_shown, main_area};
use super::sidebar::sidebar;
use crate::design::{self, Palette, panels, styles::Style};
use crate::state::{Page, State};

/// Where the titlebar cluster ends: the traffic lights, then the sidebar
/// button (GPUI `panels::CLUSTER_END`).
pub(super) const CLUSTER_END: f32 = design::TRAFFIC_LIGHTS_WIDTH + panels::TOGGLE_SIZE;
/// Room between the cluster and the titlebar's text.
pub(super) const PAST_CLUSTER: f32 = 10.;
/// The titlebar band's height (rune-kit's toolbar; the traffic lights sit
/// in its middle).
pub(super) const BAND: f32 = TOOLBAR_HEIGHT;
/// The toolbar's own trailing padding, which ZLO's titlebar actions sit in.
const TOOLBAR_PAD: f32 = 12.;

pub(super) fn ready(
    s: &State,
    p: &Palette,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let mut main = el()
        .key("panel")
        .w_full()
        .h_full()
        .min_w(0.0)
        .child(main_area(s, p, cx, k, layer));
    // The open chat lays the conversation's and the right pane's wallpaper
    // over its own regions (`chat::view`); without it the conversation is
    // the whole panel, up over the breadcrumb.
    if !chat_shown(s)
        && let Some(over) = crate::design::look::wallpaper_layer(s, "conversation")
    {
        main = main.child(
            el().pin(Some(0.), Some(-BAND), Some(0.), Some(0.))
                .inert()
                .child(over),
        );
    }
    let content = sidebar(s, p, cx, k, layer);
    let frame = SplitView::new(main)
        .sidebar(
            &s.workspace.sidebar,
            |s: &mut State| &mut s.workspace.sidebar,
            content,
        )
        .toolbar(breadcrumb(s, p, cx, k, layer))
        // The crumbs of a folded sidebar start where its title did.
        .titlebar_inset(CLUSTER_END)
        .sidebar_toggle(
            SidebarToggle::new()
                .past_lights(PAST_CLUSTER)
                .edge(PAST_CLUSTER),
        )
        .hide_sidebar_toggle()
        .frame_inset(0.)
        .toolbar_in_panel()
        .pane_motion(PaneMotion::new().spring(panels::REVEAL).fade_tail(0.15))
        .sidebar_peek(
            SidebarPeek::new()
                .edge(16., Duration::from_millis(40))
                .toggle_delay(Duration::from_millis(80))
                .below(BAND),
        )
        .reduced_motion(cx.is_reduced_motion())
        .build(&frame_kit(s, p, k));
    // The window's sidebar button sits above both columns, so it never
    // moves when the sidebar folds.
    let mut button = row()
        .pin(Some(design::TRAFFIC_LIGHTS_WIDTH), Some(0.), None, None)
        .h(BAND)
        .center_y()
        .child(sidebar_toggle(s));
    // A peeking sidebar floats raised; the button stays above it.
    if !s.workspace.sidebar.is_open() {
        button = button.raise();
    }
    el().key("ready")
        .w_full()
        .h_full()
        .child(frame)
        .child(button)
}

/// The app's kit with the frame's look: the panel square, borderless and
/// filled as Zeron's. The window root already paints the shell's fill and
/// material behind the panes, so the frame adds none (a peeking sidebar
/// brings its own surface, `sidebar::sidebar`).
fn frame_kit(s: &State, p: &Palette, k: &Kit) -> Kit {
    let st = Style::of(&s.prefs);
    let mut f = k.clone();
    f.radius.lg = 0.;
    f.c.border_muted = Color::TRANSPARENT;
    f.c.bg_base = p.bg.alpha(st.panel_alpha);
    f.c.bg_deep = Color::TRANSPARENT;
    f
}

/// Zeron's sidebar button (GPUI `panels::toggle`): the morphing sidebar
/// glyph, and the titlebar toggles' active fill while the sidebar is open.
/// Folds the left sidebar away or brings it back (docks a peeking one); the
/// choice survives a relaunch.
fn sidebar_toggle(s: &State) -> El {
    let open = s.workspace.sidebar.is_open();
    let label = if open { "Hide sidebar" } else { "Show sidebar" };
    panels::toggle(s, "sidebar-toggle", open, false, label)
        .fill(if open {
            s.palette.active
        } else {
            Color::TRANSPARENT
        })
        .on_press_tx(|s: &mut State, tx: &mut rune::Tx| super::super::toggle_sidebar(s, tx))
}

/// The panel's toolbar: where you are, t3code's "project / thread", and the
/// open thread's actions at its right end. The frame keeps the crumbs clear
/// of the titlebar cluster while the sidebar is folded.
fn breadcrumb(s: &State, p: &Palette, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> Toolbar {
    let (parent, leaf): (String, String) = match s.page {
        Page::Chat => {
            let id = active_id(s).unwrap_or("");
            let row_data = s.workspace.inbox.row(id);
            let profile = row_data
                .and_then(|r| s.workspace.inbox.profile(&r.profile_id))
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| "Default".into());
            let title = row_data
                .map(|r| super::super::row_title(r).to_string())
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
    // Zeron's titlebar identity: the thread title in the 13 px UI face, its
    // home quieter beside it; no rule, the content fades under it.
    let crumbs = row()
        .key("breadcrumb")
        .min_w(0.0)
        .gap(6.)
        .center_y()
        .child(t(parent, 13., p.text_muted))
        .child(t("/", 13., p.text_faint))
        .child(
            el().min_w(0.0)
                .child(tw(leaf, 13., MEDIUM, p.text).truncate()),
        );
    let mut bar = Toolbar::new().leading(crumbs);
    let actions = match s.page {
        Page::Chat if s.chat.is_some() => crate::chat::titlebar_actions(s, cx, k, layer),
        _ => None,
    };
    if let Some(actions) = actions {
        // Flush with the panel's edge, as Zeron's.
        bar = bar.trailing(actions.mr(-TOOLBAR_PAD));
    }
    bar
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
