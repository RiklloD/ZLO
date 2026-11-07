//! The main column under the breadcrumb (GPUI `Workspace::render`'s
//! `main`): one keyed structure whatever shows. The open chat stays mounted
//! under the settings pages (hidden) and Appearance (live), so a visit
//! never loses its place; pages and threads enter with a rise and the one
//! they replace cuts out at once.

use rune::{Cx, El, col, el};
use rune_kit::Kit;

use super::super::ui::{SEMIBOLD, SM, t, tw};
use super::super::{active_id, context, draft};
use crate::design::Palette;
use crate::icons;
use crate::state::{Page, State};

/// Whether the open chat is mounted in the main area: on its own page, under
/// Appearance and, hidden, under the settings pages, so coming back finds
/// it where it was (GPUI kept the chat entity alive). The new-thread screen
/// unmounts it: its composer flies to the chat's on the first send.
pub(super) fn chat_shown(s: &State) -> bool {
    s.chat.is_some() && s.page != Page::Draft
}

/// The page or thread entrance: a fade while rising, as GPUI's
/// `Style::enter` + `enter_rise`; the page it replaces cuts out at once
/// (GPUI animated only the entrance).
fn entrance(s: &State) -> rune::Transition {
    let st = crate::design::styles::Style::of(&s.prefs);
    rune::Transition::opacity()
        .combined(rune::Transition::offset(0., st.enter_rise))
        .springs(rune::SpringPair::new(st.enter, rune::Spring::NONE))
}

/// The main column under the breadcrumb. One keyed structure whatever
/// shows: the chat host (context banner slot over the chat) and, above it,
/// the page, so the banner coming or going and a visit to Settings never
/// remount the transcript.
pub(super) fn main_area(
    s: &State,
    p: &Palette,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let st = crate::design::styles::Style::of(&s.prefs);
    let mut main = el().key("main").w_full().grow(1.0).min_h(0.0);
    if chat_shown(s) {
        let id = active_id(s).unwrap_or_default();
        let banner = (s.page == Page::Chat)
            .then(|| context::banner(s, k))
            .flatten();
        let mut slot = el().key("context-banner-slot").w_full();
        if let Some(banner) = banner {
            slot = slot.child(banner);
        }
        // Replays the entrance on every thread switch.
        let mut host = col()
            .key(format!("main-chat-{id}"))
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .transition(entrance(s))
            .child(slot)
            .child(
                el().key("chat-host")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .child(crate::chat::view(s, cx, k, layer)),
            );
        host = match s.page {
            // Back from a settings page the chat rises in again.
            Page::Chat => host
                .opacity(1.)
                .offset(0., 0.)
                .opacity_motion(st.enter)
                .offset_motion(st.enter),
            // Appearance previews on the live chat underneath.
            Page::Appearance => host.inert(),
            // Under a settings page: out at once, kept as it was.
            _ => host
                .opacity(0.)
                .offset(0., st.enter_rise)
                .opacity_motion(rune::Anim::INSTANT)
                .offset_motion(rune::Anim::INSTANT)
                .inert()
                .a11y_hidden(),
        };
        main = main.child(host);
    }
    let page = match s.page {
        Page::Chat if s.chat.is_some() => None,
        Page::Chat => Some(empty(s, p, k)),
        Page::Draft => Some(draft::view(s, cx, k, layer)),
        page => Some(crate::pages::view(s, page, cx, k, layer)),
    };
    if let Some(page) = page {
        let mut over = el()
            .key(format!("main-{:?}", s.page))
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .transition(entrance(s))
            .child(page);
        // Appearance floats over the chat at 85 % so a pick previews live
        // on the real thread or new-chat screen.
        if s.page == Page::Appearance {
            over = over.fill(p.bg.alpha(0.85));
        }
        main = main.child(over);
    }
    main
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
