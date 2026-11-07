//! The composer's voice controls: the mic, the talk-mode button, and a pill
//! that says what voice is doing (listening, thinking, speaking) with a
//! one-click exit. Right-clicking the mic flips read-aloud for the thread.
//! Every composer (thread, new thread, quick) draws the same controls.
//!
//! Rune port of the GPUI `voice/ui.rs`. Area A (chat composer) and area B
//! (new-thread and quick composers) place [`composer_controls`] in their
//! composer's bottom row.

use rune::{Color, Cursor, El, PointerPress, TextStyle, Tx, el, icon, row, text};
use rune_kit::{Kit, MenuItem};

use super::{Composer, Notice, Status};
use crate::icons;
use crate::state::State;

const PILL_TEXT: TextStyle = TextStyle::new(12., 400.);

/// Mic and talk buttons for `composer`, with the state pill when voice is
/// busy with this composer. `thread` enables the read-aloud menu.
pub fn composer_controls(
    s: &State,
    k: &Kit,
    layer: &mut Vec<El>,
    composer: &Composer,
    thread: Option<&str>,
) -> El {
    let status = super::status(s);
    let ours = super::targets(s, composer);
    let talk = super::talk_mode(s);
    let dictating = ours && status == Status::Dictating;
    let prefs = &s.voice.prefs;
    let mic_label = if dictating { "Stop dictating" } else { "Dictate" };
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
    let target = composer.clone();
    let mic_thread = thread.map(str::to_string);
    let mut mic = round_button(s, "voice-mic", &icons::MIC, dictating, mic_label)
        .help(tip)
        .on_press(move |s: &mut State| {
            super::toggle_dictation(s, target.clone(), mic_thread.clone())
        });
    if let Some(thread) = thread {
        let reads = prefs.reads(thread);
        let menu_thread = thread.to_string();
        mic = mic.on_secondary_press(|s: &mut State, at: PointerPress, _: &mut Tx| {
            s.voice.mic_menu = Some(at);
        });
        mic = rune_kit::context_menu("voice-mic-menu", mic, s.voice.mic_menu)
            .item(
                MenuItem::new(if reads {
                    "Stop reading replies in this thread"
                } else {
                    "Read replies aloud in this thread"
                })
                .icon(if reads {
                    &icons::VOLUME_OFF
                } else {
                    &icons::VOLUME_2
                })
                .on_select(move |s: &mut State| {
                    s.voice.mic_menu = None;
                    super::toggle_read_aloud(s, &menu_thread);
                }),
            )
            .on_close(|s: &mut State| s.voice.mic_menu = None)
            .build(k, layer);
    }
    let talk_target = composer.clone();
    let talk_thread = thread.map(str::to_string);
    let talk_button = round_button(
        s,
        "voice-talk",
        &icons::AUDIO_LINES,
        talk && ours,
        "Talk mode",
    )
    .help(if talk {
        "Leave talk mode"
    } else {
        "Talk mode: speak, hear the reply, speak again"
    })
    .on_press(move |s: &mut State| {
        super::toggle_talk(s, Some(talk_target.clone()), talk_thread.clone())
    });
    // Speaking shows everywhere (a reply read aloud has no composer); the
    // rest only on the composer voice is typing into.
    let pill = match status {
        Status::Speaking => Some(status_pill(s, status)),
        Status::Off | Status::Waiting => None,
        _ if ours => Some(status_pill(s, status)),
        _ => None,
    };
    let notice = s.voice.notice.as_ref().map(|n| notice_pill(s, n));
    let mut out = row().key("voice-controls").gap(6.).items_center();
    if let Some(notice) = notice {
        out = out.child(notice);
    }
    if let Some(pill) = pill {
        out = out.child(pill);
    }
    out.child(talk_button).child(mic)
}

/// The composer's round icon button (GPUI `chat::round_button`, its
/// non-send variant): a 28 px circle, filled with the ink while on.
fn round_button(
    s: &State,
    key: &'static str,
    glyph: &'static rune::Icon,
    filled: bool,
    label: &'static str,
) -> El {
    let p = &s.palette;
    let wash = if p.light {
        Color::BLACK.alpha(0.08)
    } else {
        Color::WHITE.alpha(0.10)
    };
    let (fill, ink, hover) = if filled {
        (p.text, p.bg, p.bg.alpha(0.15))
    } else {
        (Color::TRANSPARENT, p.text_muted, wash)
    };
    el().key(key)
        .role("button")
        .label(label)
        .size(28., 28.)
        .radius(14.)
        .center()
        .cursor(Cursor::Pointer)
        .fill(fill)
        .hover(hover)
        .child(icon(glyph, 14.).color(ink))
}

fn status_pill(s: &State, status: Status) -> El {
    let p = &s.palette;
    let (label, dot) = match status {
        Status::Dictating | Status::Listening => ("Listening", p.danger),
        Status::Thinking => ("Thinking…", p.warning),
        Status::Speaking => ("Speaking", p.accent),
        Status::Waiting | Status::Off => ("", p.text_faint),
    };
    let speaking = status == Status::Speaking;
    let close = el()
        .key("voice-exit")
        .role("button")
        .label(if speaking { "Stop speaking" } else { "Stop voice" })
        .cursor(Cursor::Pointer)
        .px(2.)
        .child(
            icon(
                if speaking {
                    &icons::SQUARE
                } else {
                    &icons::CLOSE
                },
                11.,
            )
            .color(p.text_muted),
        )
        .on_press(move |s: &mut State| {
            if speaking && !super::talk_mode(s) {
                super::stop_speaking(s)
            } else {
                super::exit(s)
            }
        });
    row()
        .key("voice-pill")
        .h(26.)
        .px(10.)
        .gap(6.)
        .items_center()
        .radius(13.)
        .fill(p.hover)
        .child(el().size(7., 7.).radius(3.5).fill(dot))
        .child(text(label).font(PILL_TEXT).color(p.text))
        .child(close)
}

fn notice_pill(s: &State, notice: &Notice) -> El {
    let p = &s.palette;
    let mut pill = row()
        .key("voice-notice")
        .max_w(280.)
        .h(26.)
        .px(10.)
        .gap(6.)
        .items_center()
        .radius(13.)
        .fill(p.hover)
        .help(notice.text.clone())
        .child(
            text(notice.text.clone())
                .font(PILL_TEXT)
                .color(p.warning)
                .truncate()
                .shrink()
                .min_w(0.),
        );
    if let Some(pane) = notice.pane {
        pill = pill.child(
            text("Open Settings")
                .key("voice-notice-open")
                .role("button")
                .font(PILL_TEXT)
                .color(p.accent)
                .cursor(Cursor::Pointer)
                .on_press(move |_: &mut State| super::open_settings(pane)),
        );
    }
    pill.child(
        el().key("voice-notice-close")
            .role("button")
            .label("Dismiss")
            .cursor(Cursor::Pointer)
            .child(icon(&icons::CLOSE, 11.).color(p.text_muted))
            .on_press(super::dismiss_notice),
    )
}
