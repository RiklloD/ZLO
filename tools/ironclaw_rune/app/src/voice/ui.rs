//! The composer's voice controls: one mic, and a pill that says what voice
//! is doing (listening, thinking, speaking) with a one-click exit. A click
//! dictates; right-clicking the mic offers talk mode and, in a thread,
//! read-aloud. (A second, talk-mode button beside the mic read as a second
//! dictation button: both showed "Listening".) Every composer (thread, new
//! thread, quick) draws the same controls; in the quick composer, which has
//! no menu, a right-click on the mic toggles talk mode.
//!
//! Area A (chat composer) and area B (new-thread and quick composers) place
//! [`composer_controls`] in their composer's bottom row.

use rune::{Color, Cursor, El, PointerPress, TextStyle, Tx, el, icon, row, text};
use rune_kit::{Kit, MenuItem, RollingText};

use super::level::{BARS, STEPS};
use super::{Composer, Notice, Status};
use crate::design::kit::loops::{BREATH_35, BREATH_45, breathe};
use crate::design::{Palette, motion};
use crate::icons;
use crate::state::State;

const PILL_TEXT: TextStyle = TextStyle::new(12., 400.);
/// The waveform's bars: 1.5 pt wide, 1 pt apart, 2–12 pt tall.
const BAR_W: f32 = 1.5;
const BAR_GAP: f32 = 1.;
const BAR_MIN: f32 = 2.;
const BAR_MAX: f32 = 12.;
/// Still poses, in height steps: what each state shows when nothing
/// moves (reduced motion, the battery saver, a fixture), and the shapes
/// the thinking and speaking loops brighten.
const LISTENING_STILL: [u8; BARS] = [2, 5, 8, 5, 2];
const THINKING: [u8; BARS] = [2, 3, 4, 3, 2];
const SPEAKING: [u8; BARS] = [3, 6, 9, 6, 3];

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
    /// The live waveform (`VoiceState::bars`); `None` draws the still pose.
    bars: Option<[u8; BARS]>,
}

impl Controls {
    pub fn of(s: &State, composer: &Composer) -> Self {
        let status = super::status(s);
        let ours = super::targets(s, composer);
        let prefs = &s.voice.prefs;
        let talk = super::talk_mode(s);
        let tip = if ours && status == Status::Dictating {
            "Stop dictating".to_string()
        } else if ours && talk {
            "Leave talk mode".to_string()
        } else if prefs.hotkey_enabled {
            let key = super::HOTKEYS
                .iter()
                .find(|(k, _)| *k == prefs.hotkey)
                .map_or("the key", |(_, label)| *label);
            format!("Dictate (or hold {key}) · right-click for talk mode")
        } else {
            "Dictate · right-click for talk mode".to_string()
        };
        Self {
            status,
            ours,
            talk,
            tip,
            notice: s.voice.notice.clone(),
            bars: s.voice.bars,
        }
    }

    fn dictating(&self) -> bool {
        self.ours && self.status == Status::Dictating
    }

    /// Talk mode is on and speaking into this composer.
    fn talking(&self) -> bool {
        self.ours && self.talk
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

/// The mic for `composer`, with the state pill when voice is busy with this
/// composer. Its right-click menu holds talk mode, and read-aloud when
/// `thread` is set.
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
    let talking = controls.talking();
    let talk_target = composer.clone();
    let talk_thread = thread.map(str::to_string);
    let menu = |mic: El| -> El {
        let mic = mic.on_secondary_press(|s: &mut State, at: PointerPress, _: &mut Tx| {
            s.voice.mic_menu = Some(at);
        });
        let menu = rune_kit::context_menu("voice-mic-menu", mic, s.voice.mic_menu).item(
            MenuItem::new(if talking {
                "Leave talk mode"
            } else {
                "Talk mode: speak, hear the reply, speak again"
            })
            .icon(&icons::AUDIO_LINES)
            .on_select(move |s: &mut State| {
                s.voice.mic_menu = None;
                press(s, Press::Talk, talk_target.clone(), talk_thread.clone());
            }),
        );
        let Some(thread) = thread else {
            return menu
                .on_close(|s: &mut State| s.voice.mic_menu = None)
                .build(k, layer);
        };
        let reads = s.voice.prefs.reads(thread);
        let menu_thread = thread.to_string();
        menu.item(
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
    let (dictating, talking) = (c.dictating(), c.talking());
    let mic_label = if dictating {
        "Stop dictating"
    } else if talking {
        "Leave talk mode"
    } else {
        "Dictate"
    };
    // In talk mode the one mic is talk mode's: a click leaves it.
    let mic_press = on(if talking { Press::Talk } else { Press::Mic });
    let mic = round_button(
        p,
        "voice-mic",
        if talking {
            &icons::AUDIO_LINES
        } else {
            &icons::MIC
        },
        dictating || talking,
        mic_label,
    )
    .help(c.tip.clone())
    .on_press(move |s: &mut S| mic_press(s));
    let mic = wrap_mic(mic);
    // Speaking shows everywhere (a reply read aloud has no composer); the
    // rest only on the composer voice is typing into.
    let pill = match c.status {
        Status::Speaking => Some(status_pill(p, c.status, c.bars, &on)),
        Status::Off | Status::Waiting => None,
        _ if c.ours => Some(status_pill(p, c.status, c.bars, &on)),
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
    out.child(mic)
}

/// The composer's round icon button: a 28 px tile with 7 px corners, filled
/// with the ink while on.
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
        .radius(7.)
        .center()
        .cursor(Cursor::Pointer)
        .fill(fill)
        .hover(hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .press_scale(motion::PRESS)
        .child(icon(glyph, 14.).color(ink))
}

fn status_pill<S: 'static>(
    p: &Palette,
    status: Status,
    bars: Option<[u8; BARS]>,
    on: &impl Fn(Press) -> OnPress<S>,
) -> El {
    let (label, ink) = match status {
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
        .press_scale(motion::PRESS)
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
    // State changes are events: the label rolls to its new words and the
    // pill's width follows on the label's own spring.
    let label = RollingText::new("voice-pill-label", label)
        .font(PILL_TEXT)
        .color(p.text)
        .spring(motion::ROLL)
        .build(&crate::design::kit(p));
    row()
        .key("voice-pill")
        .h(26.)
        .px(10.)
        .gap(6.)
        .items_center()
        .radius(13.)
        .fill(p.hover)
        .transition(motion::pop(6.))
        .child(waveform(status, bars, ink))
        .child(label)
        .child(close)
}

/// The pill's mini waveform, in the colour the dot had. Listening: the
/// mic's level (`bars`, sampled at ~16 Hz), or a still pose. A sample's
/// commit would spring the bars' heights on the layout spring, drawing at
/// the display's rate between samples; `instant_changes` lands each one, so
/// the bars step at the sample rate instead. Thinking: a low form breathing. Speaking:
/// a brightness wave across the bars. Both loops are 15 fps `Cycle`s and
/// hold still under reduced motion.
fn waveform(status: Status, bars: Option<[u8; BARS]>, ink: Color) -> El {
    let heights = match status {
        Status::Thinking => THINKING,
        Status::Speaking => SPEAKING,
        _ => bars.unwrap_or(LISTENING_STILL),
    };
    let mut wave = row()
        .key("voice-wave")
        .h(BAR_MAX)
        .gap(BAR_GAP)
        .items_center()
        .instant_changes()
        .a11y_hidden();
    for (i, steps) in heights.into_iter().enumerate() {
        let h = BAR_MIN + (BAR_MAX - BAR_MIN) * f32::from(steps.min(STEPS)) / f32::from(STEPS);
        let mut bar = el()
            .key(format!("voice-bar-{i}"))
            .size(BAR_W, h)
            .radius(BAR_W / 2.)
            .fill(ink);
        bar = match status {
            // Half a period in: the loops start lit, as their still pose is.
            Status::Thinking => bar.cycle(breathe(1.6, 0.8, &BREATH_45)),
            Status::Speaking => bar.cycle(breathe(1.0, 0.5 + i as f32 * 0.12, &BREATH_35)),
            _ => bar,
        };
        wave = wave.child(bar);
    }
    wave
}

fn notice_pill<S: 'static>(p: &Palette, notice: &Notice, on: &impl Fn(Press) -> OnPress<S>) -> El {
    let mut pill = row()
        .key("voice-notice")
        .transition(motion::pop(6.))
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
            .press_scale(motion::PRESS)
            .child(icon(&icons::CLOSE, 11.).color(p.text_muted))
            .on_press({
                let dismiss = on(Press::DismissNotice);
                move |s: &mut S| dismiss(s)
            }),
    )
}
