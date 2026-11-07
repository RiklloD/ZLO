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
use crate::design::Palette;
use crate::icons;
use crate::state::State;

const PILL_TEXT: TextStyle = TextStyle::new(12., 400.);

/// What a composer's controls show, read from the voice state. The quick
/// composer draws in its own window with its own state type, so the main
/// window hands it a copy of this (`workspace::quick`) and its presses
/// come back as [`Press`]es.
#[derive(Clone, Debug, PartialEq)]
pub struct Controls {
    status: Status,
    /// Voice is typing into this composer.
    ours: bool,
    talk: bool,
    tip: String,
    notice: Option<Notice>,
}

impl Controls {
    pub fn of(s: &State, composer: &Composer) -> Self {
        let status = super::status(s);
        let ours = super::targets(s, composer);
        let prefs = &s.voice.prefs;
        let tip = if ours && status == Status::Dictating {
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
        Self {
            status,
            ours,
            talk: super::talk_mode(s),
            tip,
            notice: s.voice.notice.clone(),
        }
    }

    fn dictating(&self) -> bool {
        self.ours && self.status == Status::Dictating
    }
}

/// A press on the controls, carried out by [`press`] on the main state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Mic,
    Talk,
    /// The pill's ✕ (or ■ while a reply is spoken).
    Exit {
        speaking: bool,
    },
    DismissNotice,
}

/// Carry out `press` on `composer`'s controls.
pub fn press(s: &mut State, press: Press, composer: Composer, thread: Option<String>) {
    match press {
        Press::Mic => super::toggle_dictation(s, composer, thread),
        Press::Talk => super::toggle_talk(s, Some(composer), thread),
        Press::Exit { speaking } => {
            if speaking && !super::talk_mode(s) {
                super::stop_speaking(s)
            } else {
                super::exit(s)
            }
        }
        Press::DismissNotice => super::dismiss_notice(s),
    }
}

/// A handler for one [`Press`], in the drawing window's state type.
pub type OnPress<S> = Box<dyn Fn(&mut S)>;

/// Mic and talk buttons for `composer`, with the state pill when voice is
/// busy with this composer. `thread` enables the read-aloud menu.
pub fn composer_controls(
    s: &State,
    k: &Kit,
    layer: &mut Vec<El>,
    composer: &Composer,
    thread: Option<&str>,
) -> El {
    let controls = Controls::of(s, composer);
    let target = composer.clone();
    let owned_thread = thread.map(str::to_string);
    let on = move |p: Press| -> OnPress<State> {
        let target = target.clone();
        let thread = owned_thread.clone();
        Box::new(move |s: &mut State| press(s, p, target.clone(), thread.clone()))
    };
    let menu = |mic: El| -> El {
        let Some(thread) = thread else { return mic };
        let reads = s.voice.prefs.reads(thread);
        let menu_thread = thread.to_string();
        let mic = mic.on_secondary_press(|s: &mut State, at: PointerPress, _: &mut Tx| {
            s.voice.mic_menu = Some(at);
        });
        rune_kit::context_menu("voice-mic-menu", mic, s.voice.mic_menu)
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
            .build(k, layer)
    };
    draw(&controls, &s.palette, on, menu)
}

/// Draw `c` in any window: `on` makes each press's handler in that
/// window's state type, `wrap_mic` may add to the mic (its menu).
pub fn draw<S: 'static>(
    c: &Controls,
    p: &Palette,
    on: impl Fn(Press) -> OnPress<S>,
    wrap_mic: impl FnOnce(El) -> El,
) -> El {
    let dictating = c.dictating();
    let mic_label = if dictating {
        "Stop dictating"
    } else {
        "Dictate"
    };
    let mic_press = on(Press::Mic);
    let mic = round_button(p, "voice-mic", &icons::MIC, dictating, mic_label)
        .help(c.tip.clone())
        .on_press(move |s: &mut S| mic_press(s));
    let mic = wrap_mic(mic);
    let talk_press = on(Press::Talk);
    let talk_button = round_button(
        p,
        "voice-talk",
        &icons::AUDIO_LINES,
        c.talk && c.ours,
        "Talk mode",
    )
    .help(if c.talk {
        "Leave talk mode"
    } else {
        "Talk mode: speak, hear the reply, speak again"
    })
    .on_press(move |s: &mut S| talk_press(s));
    // Speaking shows everywhere (a reply read aloud has no composer); the
    // rest only on the composer voice is typing into.
    let pill = match c.status {
        Status::Speaking => Some(status_pill(p, c.status, &on)),
        Status::Off | Status::Waiting => None,
        _ if c.ours => Some(status_pill(p, c.status, &on)),
        _ => None,
    };
    let notice = c.notice.as_ref().map(|n| notice_pill(p, n, &on));
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
    p: &Palette,
    key: &'static str,
    glyph: &'static rune::Icon,
    filled: bool,
    label: &'static str,
) -> El {
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

fn status_pill<S: 'static>(p: &Palette, status: Status, on: &impl Fn(Press) -> OnPress<S>) -> El {
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
        .label(if speaking {
            "Stop speaking"
        } else {
            "Stop voice"
        })
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
        .on_press({
            let exit = on(Press::Exit { speaking });
            move |s: &mut S| exit(s)
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

fn notice_pill<S: 'static>(p: &Palette, notice: &Notice, on: &impl Fn(Press) -> OnPress<S>) -> El {
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
                .on_press(move |_: &mut S| super::open_settings(pane)),
        );
    }
    pill.child(
        el().key("voice-notice-close")
            .role("button")
            .label("Dismiss")
            .cursor(Cursor::Pointer)
            .child(icon(&icons::CLOSE, 11.).color(p.text_muted))
            .on_press({
                let dismiss = on(Press::DismissNotice);
                move |s: &mut S| dismiss(s)
            }),
    )
}
