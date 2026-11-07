//! Settings → Voice: push-to-talk, spoken replies, talk mode and the wake
//! word. Everything that costs battery says so, and all of it is off until
//! turned on here (the composer's mic button works without any of it).
//!
//! Rune port of the GPUI `pages/settings/voice.rs`. The page helpers
//! (`page_body`, `header`, `group_header`, `rows_card`, `setting_row`) are
//! local copies of the GPUI `pages/mod.rs` ones until area D lands
//! `pages/common.rs`; the integrator swaps them.

use rune::{Color, Cx, El, ScrollSpec, TextStyle, col, el, row, text};
use rune_kit::{Button, Kit, SegmentedControl, Switch, TextInput};

use super::{HOTKEYS, VoicePrefs};
use crate::state::State;

const PACES: [(f32, &str); 3] = [(0.42, "Slow"), (0.5, "Normal"), (0.58, "Fast")];
const PAUSES: [(u32, &str); 3] = [(1500, "1.5 s"), (2000, "2 s"), (3000, "3 s")];
const PAGE_MAX_WIDTH: f32 = 760.;

pub(super) fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let prefs = &s.voice.prefs;
    let switch = |on: bool, label: &'static str, set: fn(&mut VoicePrefs, bool)| {
        Switch::new(on)
            .label(label)
            .hide_label()
            .on_toggle(move |s: &mut State| {
                let on = !on;
                super::update_prefs(s, |p| set(p, on));
            })
            .build(k)
    };
    let key_labels: Vec<&'static str> = HOTKEYS.iter().map(|(_, l)| *l).collect();
    let key_ix = HOTKEYS
        .iter()
        .position(|(k, _)| *k == prefs.hotkey)
        .unwrap_or(0);
    let keys = segmented(s, k, "voice-key", &key_labels, key_ix, !prefs.hotkey_enabled, |s, ix| {
        super::update_prefs(s, |p| p.hotkey = HOTKEYS[ix].0.into())
    });
    let pace_ix = closest(PACES.iter().map(|(v, _)| *v), prefs.speech_rate);
    let pace = segmented(s, k, "voice-pace", &PACES.map(|(_, l)| l), pace_ix, false, |s, ix| {
        super::update_prefs(s, |p| p.speech_rate = PACES[ix].0)
    });
    let pause_ix = closest(
        PAUSES.iter().map(|(v, _)| *v as f32),
        prefs.silence_ms as f32,
    );
    let pause = segmented(s, k, "voice-pause", &PAUSES.map(|(_, l)| l), pause_ix, false, |s, ix| {
        super::update_prefs(s, |p| p.silence_ms = PAUSES[ix].0)
    });
    let access = if s.voice.authorized {
        text("Allowed")
            .key("voice-allowed")
            .font(TextStyle::new(14., 400.))
            .color(p.success)
    } else {
        Button::new("Allow")
            .on_press(super::request_access)
            .build(k)
            .key("voice-allow")
    };
    let phrase = el().key("voice-phrase").w(180.).child(
        TextInput::new(&s.voice.wake_phrase)
            .label("Wake phrase")
            .on_edit(|s: &mut State| &mut s.voice.wake_phrase)
            .on_submit(|s: &mut State, _| {
                let phrase = s.voice.wake_phrase.text.trim().to_string();
                if !phrase.is_empty() && phrase != s.voice.prefs.wake_phrase {
                    super::update_prefs(s, move |p| p.wake_phrase = phrase);
                }
            })
            .build(k),
    );
    let body = col()
        .w_full()
        .child(fixture_preview(s, k, _layer))
        .child(header(s, "Voice"))
        .child(group_header(
            s,
            "Dictation",
            Some("Speak into any composer. Speech is recognized on this Mac by Apple Speech."),
        ))
        .child(rows_card(
            s,
            "voice-dictation",
            vec![
                setting_row(
                    s,
                    "Microphone and speech recognition",
                    Some(
                        "macOS asks once. If you said no, turn ZLO Rune on in System Settings → \
                         Privacy & Security → Microphone and Speech Recognition.",
                    ),
                    access,
                ),
                setting_row(
                    s,
                    "Push-to-talk key",
                    Some(
                        "Hold the key in any app to dictate into ZLO; a tap or a shortcut \
                         with it does nothing. While on, ZLO checks the key 10 times a second.",
                    ),
                    switch(prefs.hotkey_enabled, "Push-to-talk key", |p, on| {
                        p.hotkey_enabled = on
                    }),
                ),
                setting_row(s, "Key", None, keys),
                setting_row(
                    s,
                    "Send on release",
                    Some("Releasing the key sends the message instead of leaving it to edit."),
                    switch(prefs.auto_send, "Send on release", |p, on| p.auto_send = on),
                ),
                setting_row(
                    s,
                    "On this Mac only",
                    Some(
                        "Audio never leaves your Mac. Turn off to let Apple's servers \
                         recognize speech when your language has no on-device model. Cloud \
                         engines (Whisper, OpenAI and ElevenLabs voices) need a gateway voice \
                         endpoint that does not exist yet.",
                    ),
                    switch(prefs.on_device_only, "On this Mac only", |p, on| {
                        p.on_device_only = on
                    }),
                ),
            ],
        ))
        .child(group_header(
            s,
            "Spoken replies",
            Some("Right-click the mic in a thread to choose for that thread alone."),
        ))
        .child(rows_card(
            s,
            "voice-replies",
            vec![
                setting_row(
                    s,
                    "Read replies aloud",
                    Some(
                        "In every chat, with the Mac's own voice. Code and tables are not \
                         read out; ZLO says to see the screen. Talking or the pill's Stop \
                         ends it.",
                    ),
                    switch(prefs.read_aloud, "Read replies aloud", |p, on| {
                        p.read_aloud = on
                    }),
                ),
                setting_row(s, "Pace", None, pace),
            ],
        ))
        .child(group_header(
            s,
            "Talk mode",
            Some(
                "The waveform button next to the mic: speak, ZLO sends after a pause, reads \
                 the reply, and listens again. Speak to interrupt. It ends after two quiet \
                 minutes.",
            ),
        ))
        .child(rows_card(
            s,
            "voice-talk",
            vec![setting_row(
                s,
                "Pause before sending",
                Some("How long you can stop talking before the message goes."),
                pause,
            )],
        ))
        .child(group_header(s, "Wake word", None))
        .child(rows_card(
            s,
            "voice-wake",
            vec![
                setting_row(
                    s,
                    "Listen for the wake phrase",
                    Some(
                        "Battery cost: keeps the microphone and speech recognition running \
                         the whole time ZLO is open, which drains a laptop noticeably. After \
                         the phrase and a short pause, ZLO captures what you say (20 seconds \
                         at most) and sends it.",
                    ),
                    switch(prefs.wake_enabled, "Wake word", |p, on| p.wake_enabled = on),
                ),
                setting_row(
                    s,
                    "Phrase",
                    Some("Pick words the recognizer spells reliably. Enter saves."),
                    phrase,
                ),
            ],
        ));
    page_body("voice-page", body)
}

/// Headless scenes: the composer's voice controls and SnapShot chips, as
/// a composer would place them (nothing outside fixtures).
fn fixture_preview(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let Some(preview) = s.voice.fixture_preview.as_ref().filter(|_| s.fixture) else {
        return el();
    };
    let p = &s.palette;
    let thread = match &preview.composer {
        super::Composer::Chat(id) => Some(id.as_str()),
        _ => None,
    };
    let mut chips = row().gap(6.).items_center().grow(1.);
    for file in &preview.staged {
        if crate::snapshot::is_companion(file, &preview.staged) {
            continue;
        }
        let (title, label) = crate::snapshot::names(s, file)
            .unwrap_or_else(|| (file.filename.clone(), file.filename.clone()));
        let mut chip = row()
            .key(format!("chip-{}", file.filename))
            .h(26.)
            .px(8.)
            .gap(6.)
            .items_center()
            .radius(8.)
            .fill(p.hover)
            .label(label);
        if let Some(icon) = crate::snapshot::icon(s, file) {
            chip = chip.child(icon);
        }
        chips = chips.child(chip.child(text(title).font(TextStyle::new(12., 400.)).color(p.text)));
    }
    row()
        .key("fixture-composer")
        .w_full()
        .min_h(56.)
        .px(12.)
        .py(10.)
        .mb(16.)
        .gap(8.)
        .items_center()
        .radius(16.)
        .fill(p.surface_card)
        .child(chips)
        .child(super::ui::composer_controls(s, k, layer, &preview.composer, thread))
}

/// The index of the option nearest `value`.
fn closest(options: impl Iterator<Item = f32>, value: f32) -> usize {
    options
        .enumerate()
        .min_by(|a, b| (a.1 - value).abs().total_cmp(&(b.1 - value).abs()))
        .map_or(0, |(ix, _)| ix)
}

/// A segmented choice (GPUI `design::kit::segmented`); dimmed and inert
/// while `disabled`.
fn segmented(
    _s: &State,
    k: &Kit,
    key: &'static str,
    labels: &[&'static str],
    selected: usize,
    disabled: bool,
    pick: fn(&mut State, usize),
) -> El {
    let control = SegmentedControl::new(labels, Some(selected)).label(key);
    let control = if disabled {
        control.build(k).opacity(0.5)
    } else {
        control
            .on_pick(move |s: &mut State, ix: usize, _| pick(s, ix))
            .build(k)
    };
    control.key(key)
}

// Page helpers (GPUI `pages/mod.rs`), local until `pages/common.rs`.

/// Centered, width-capped page body that scrolls.
fn page_body(key: &'static str, content: El) -> El {
    col()
        .key(key)
        .w_full()
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(
            row()
                .w_full()
                .justify_center()
                .px(40.)
                .pt(28.)
                .pb(48.)
                .child(col().w_full().max_w(PAGE_MAX_WIDTH).child(content)),
        )
}

fn header(s: &State, title: &'static str) -> El {
    row()
        .justify_between()
        .items_center()
        .px(8.)
        .pb(8.)
        .child(text(title).font(TextStyle::new(20., 500.)).color(s.palette.text))
}

/// A settings group's label: 32 px above the group, an optional line of
/// explanation.
fn group_header(s: &State, title: &'static str, note: Option<&'static str>) -> El {
    let p = &s.palette;
    let mut out = col().pt(32.).pb(8.).px(8.).gap(2.).child(
        text(title)
            .font(TextStyle {
                line_height: 22.,
                ..TextStyle::new(16., 600.)
            })
            .color(p.text),
    );
    if let Some(note) = note {
        out = out.child(text(note).font(TextStyle::new(12., 400.)).color(p.text_muted).wrap());
    }
    out
}

/// Zeron's block fill: a faint wash of the theme's ink, no outline.
fn block_fill(s: &State) -> Color {
    if s.palette.light {
        Color::BLACK.alpha(0.045)
    } else {
        Color::WHITE.alpha(0.045)
    }
}

/// Related settings in one block: 12 px radius, the wash fill, hairlines
/// between rows inset 16 px.
fn rows_card(s: &State, key: &'static str, rows: Vec<El>) -> El {
    let mut card = col().key(key).w_full().radius(12.).fill(block_fill(s));
    for (ix, r) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(el().mx(16.).h(1.).fill(s.palette.border.alpha(0.6 * s.palette.border.a)));
        }
        card = card.child(r);
    }
    card
}

/// One setting: a 14 px medium title and a 12 px muted description on the
/// left, the control on the right, at least 60 px tall.
fn setting_row(s: &State, label: &'static str, description: Option<&'static str>, control: El) -> El {
    let p = &s.palette;
    let mut left = col().grow(1.).min_w(160.).gap(1.).child(
        text(label)
            .font(TextStyle::new(14., 500.))
            .color(p.text),
    );
    if let Some(d) = description {
        left = left.child(
            text(d)
                .font(TextStyle {
                    line_height: 18.,
                    ..TextStyle::new(12., 400.)
                })
                .color(p.text_muted)
                .wrap(),
        );
    }
    row()
        .key(format!("row-{label}"))
        .w_full()
        .min_h(60.)
        .px(16.)
        .py(12.)
        .gap(16.)
        .items_center()
        .child(left)
        .child(el().child(control))
}
