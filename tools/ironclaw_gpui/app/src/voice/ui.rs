//! The composer's voice controls: the mic, the talk-mode button, and a pill
//! that says what voice is doing (listening, thinking, speaking) with a
//! one-click exit. Right-clicking the mic flips read-aloud for the thread.
//! Every composer (thread, new thread, quick) draws the same controls.

use crate::assets::ExtraIcon;
use crate::chat::round_button;
use crate::design::Palette;
use crate::ui::*;

use super::{Status, VoicePrefs};

/// Mic and talk buttons for `input`, with the state pill when voice is
/// busy with this composer. `thread` enables the read-aloud menu.
pub(crate) fn composer_controls(
    input: &Entity<TextareaState>,
    thread: Option<&str>,
    cx: &App,
) -> AnyElement {
    let status = super::status(cx);
    let ours = super::targets(input, cx);
    let talk = super::talk_mode(cx);
    let dictating = ours && status == Status::Dictating;
    let prefs = VoicePrefs::of(cx);
    let mic_label: &'static str = if dictating {
        "Stop dictating"
    } else {
        "Dictate"
    };
    let tip = if dictating {
        "Stop dictating".to_string()
    } else if prefs.hotkey_enabled {
        let key = super::HOTKEYS
            .iter()
            .find(|(k, _)| *k == prefs.hotkey)
            .map_or("the key", |(_, label)| *label);
        format!("Dictate (or hold {key})")
    } else {
        "Dictate".to_string()
    };
    let target = input.clone();
    let mut mic = round_button("voice-mic", IconName::Mic, dictating, mic_label, cx)
        .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
        .on_click(move |_, window, cx| {
            super::toggle_dictation(target.clone(), window.window_handle(), cx)
        })
        .into_any_element();
    if let Some(thread) = thread {
        let thread = thread.to_string();
        let reads = prefs.reads(&thread);
        mic = div()
            .id("voice-mic-menu")
            .child(mic)
            .context_menu(move |menu: PopupMenu, _, _| {
                let thread = thread.clone();
                menu.item(
                    PopupMenuItem::new(if reads {
                        "Stop reading replies in this thread"
                    } else {
                        "Read replies aloud in this thread"
                    })
                    .icon(Icon::new(if reads {
                        ExtraIcon::VolumeOff
                    } else {
                        ExtraIcon::Volume2
                    }))
                    .on_click(move |_, _, cx| super::toggle_read_aloud(&thread, cx)),
                )
            })
            .into_any_element();
    }
    let talk_target = input.clone();
    let talk_button = round_button(
        "voice-talk",
        ExtraIcon::AudioLines,
        talk && ours,
        "Talk mode",
        cx,
    )
    .tooltip(move |w, cx| {
        gpui_kit::component::tooltip::Tooltip::new(if talk {
            "Leave talk mode"
        } else {
            "Talk mode: speak, hear the reply, speak again"
        })
        .build(w, cx)
    })
    .on_click(move |_, window, cx| {
        super::toggle_talk(Some(talk_target.clone()), Some(window.window_handle()), cx)
    });
    // Speaking shows everywhere (a reply read aloud has no composer); the
    // rest only on the composer voice is typing into.
    let pill = match &status {
        Status::Speaking => Some(pill(&status, cx)),
        Status::Off | Status::Waiting => None,
        _ if ours => Some(pill(&status, cx)),
        _ => None,
    };
    let notice = super::notice(cx).map(|n| notice_pill(n, cx));
    h_flex()
        .gap(px(6.))
        .items_center()
        .children(notice)
        .children(pill)
        .child(talk_button)
        .child(mic)
        .into_any_element()
}

fn pill(status: &Status, cx: &App) -> AnyElement {
    let p = *Palette::of(cx);
    let (label, dot) = match status {
        Status::Dictating => ("Listening", p.danger),
        Status::Listening => ("Listening", p.danger),
        Status::Thinking => ("Thinking…", p.warning),
        Status::Speaking => ("Speaking", p.accent),
        Status::Waiting | Status::Off => ("", p.text_faint),
    };
    let speaking = *status == Status::Speaking;
    let close = div()
        .id("voice-exit")
        .role(Role::Button)
        .aria_label(if speaking {
            "Stop speaking"
        } else {
            "Stop voice"
        })
        .cursor_pointer()
        .px(px(2.))
        .text_color(p.text_muted)
        .child(
            Icon::new(if speaking {
                IconName::Square
            } else {
                IconName::Close
            })
            .size(px(11.)),
        )
        .on_click(move |_, _, cx| {
            if speaking && !super::talk_mode(cx) {
                super::stop_speaking(cx)
            } else {
                super::exit(cx)
            }
        });
    h_flex()
        .h(px(26.))
        .px(px(10.))
        .gap(px(6.))
        .items_center()
        .rounded_full()
        .bg(p.hover)
        .text_xs()
        .text_color(p.text)
        .child(div().size(px(7.)).rounded_full().bg(dot))
        .child(label)
        .child(close)
        .into_any_element()
}

fn notice_pill(notice: super::Notice, cx: &App) -> AnyElement {
    let p = *Palette::of(cx);
    let text = notice.text.clone();
    h_flex()
        .id("voice-notice")
        .max_w(px(280.))
        .h(px(26.))
        .px(px(10.))
        .gap(px(6.))
        .items_center()
        .rounded_full()
        .bg(p.hover)
        .text_xs()
        .text_color(p.warning)
        .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(w, cx))
        .child(div().min_w_0().truncate().child(notice.text.clone()))
        .when_some(notice.pane, |d, pane| {
            d.child(
                div()
                    .id("voice-notice-open")
                    .role(Role::Button)
                    .cursor_pointer()
                    .flex_none()
                    .text_color(p.accent)
                    .child("Open Settings")
                    .on_click(move |_, _, _| super::open_settings(pane)),
            )
        })
        .child(
            div()
                .id("voice-notice-close")
                .role(Role::Button)
                .aria_label("Dismiss")
                .cursor_pointer()
                .flex_none()
                .text_color(p.text_muted)
                .child(Icon::new(IconName::Close).size(px(11.)))
                .on_click(|_, _, cx| super::dismiss_notice(cx)),
        )
        .into_any_element()
}
