//! Settings → Voice: push-to-talk, spoken replies, talk mode and the wake
//! word. Everything that costs battery says so, and all of it is off until
//! turned on here (the composer's mic button works without any of it).

use super::SettingsView;
use crate::design::kit;
use crate::pages::{group_header, rows_card, setting_row};
use crate::ui::*;
use crate::voice::{self, HOTKEYS, VoicePrefs};

const PACES: [(f32, &str); 3] = [(0.42, "Slow"), (0.5, "Normal"), (0.58, "Fast")];
const PAUSES: [(u32, &str); 3] = [(1500, "1.5 s"), (2000, "2 s"), (3000, "3 s")];

pub(super) fn render(view: &SettingsView, cx: &Context<SettingsView>) -> impl IntoElement {
    let prefs = VoicePrefs::of(cx);
    let switch =
        |id: &'static str, on: bool, label: &'static str, set: fn(&mut VoicePrefs, bool)| {
            Switch::new(id)
                .checked(on)
                .accessibility_label(label)
                .on_click(move |checked: &bool, _, cx| {
                    let on = *checked;
                    VoicePrefs::update(cx, |p| set(p, on));
                })
        };
    let key_labels: Vec<&'static str> = HOTKEYS.iter().map(|(_, l)| *l).collect();
    let key_ix = HOTKEYS
        .iter()
        .position(|(k, _)| *k == prefs.hotkey)
        .unwrap_or(0);
    let keys = kit::segmented(
        "voice-key",
        &key_labels,
        key_ix,
        !prefs.hotkey_enabled,
        |ix, _, cx| VoicePrefs::update(cx, |p| p.hotkey = HOTKEYS[ix].0.into()),
        cx,
    );
    let pace_ix = closest(PACES.iter().map(|(v, _)| *v), prefs.speech_rate);
    let pace = kit::segmented(
        "voice-pace",
        &PACES.map(|(_, l)| l),
        pace_ix,
        false,
        |ix, _, cx| VoicePrefs::update(cx, |p| p.speech_rate = PACES[ix].0),
        cx,
    );
    let pause_ix = closest(
        PAUSES.iter().map(|(v, _)| *v as f32),
        prefs.silence_ms as f32,
    );
    let pause = kit::segmented(
        "voice-pause",
        &PAUSES.map(|(_, l)| l),
        pause_ix,
        false,
        |ix, _, cx| VoicePrefs::update(cx, |p| p.silence_ms = PAUSES[ix].0),
        cx,
    );
    let access = if voice::authorized() {
        div()
            .text_sm()
            .text_color(cx.theme().success)
            .child("Allowed")
            .into_any_element()
    } else {
        Button::new("voice-allow")
            .label("Allow")
            .small()
            .on_click(|_, _, cx| voice::request_access(cx))
            .into_any_element()
    };
    v_flex()
        .child(group_header(
            "Dictation",
            Some("Speak into any composer. Speech is recognized on this Mac by Apple Speech."),
            cx,
        ))
        .child(rows_card(
            [
                setting_row(
                    "Microphone and speech recognition",
                    Some(
                        "macOS asks once. If you said no, turn ZLO on in System Settings → \
                         Privacy & Security → Microphone and Speech Recognition."
                            .into(),
                    ),
                    access,
                    cx,
                ),
                setting_row(
                    "Push-to-talk key",
                    Some(
                        "Hold the key in any app to dictate into ZLO; a tap or a shortcut \
                         with it does nothing. While on, ZLO checks the key 10 times a second."
                            .into(),
                    ),
                    switch(
                        "voice-hotkey",
                        prefs.hotkey_enabled,
                        "Push-to-talk key",
                        |p, on| p.hotkey_enabled = on,
                    ),
                    cx,
                ),
                setting_row("Key", None, keys, cx),
                setting_row(
                    "Send on release",
                    Some(
                        "Releasing the key sends the message instead of leaving it to edit.".into(),
                    ),
                    switch(
                        "voice-autosend",
                        prefs.auto_send,
                        "Send on release",
                        |p, on| p.auto_send = on,
                    ),
                    cx,
                ),
                setting_row(
                    "On this Mac only",
                    Some(
                        "Audio never leaves your Mac. Turn off to let Apple's servers \
                         recognize speech when your language has no on-device model. Cloud \
                         engines (Whisper, OpenAI and ElevenLabs voices) need a gateway voice \
                         endpoint that does not exist yet."
                            .into(),
                    ),
                    switch(
                        "voice-ondevice",
                        prefs.on_device_only,
                        "On this Mac only",
                        |p, on| p.on_device_only = on,
                    ),
                    cx,
                ),
            ],
            cx,
        ))
        .child(group_header(
            "Spoken replies",
            Some("Right-click the mic in a thread to choose for that thread alone."),
            cx,
        ))
        .child(rows_card(
            [
                setting_row(
                    "Read replies aloud",
                    Some(
                        "In every chat, with the Mac's own voice. Code and tables are not \
                         read out; ZLO says to see the screen. Talking or the pill's Stop \
                         ends it."
                            .into(),
                    ),
                    switch(
                        "voice-read",
                        prefs.read_aloud,
                        "Read replies aloud",
                        |p, on| p.read_aloud = on,
                    ),
                    cx,
                ),
                setting_row("Pace", None, pace, cx),
            ],
            cx,
        ))
        .child(group_header(
            "Talk mode",
            Some(
                "The waveform button next to the mic: speak, ZLO sends after a pause, reads \
                 the reply, and listens again. Speak to interrupt. It ends after two quiet \
                 minutes.",
            ),
            cx,
        ))
        .child(rows_card(
            [setting_row(
                "Pause before sending",
                Some("How long you can stop talking before the message goes.".into()),
                pause,
                cx,
            )],
            cx,
        ))
        .child(group_header("Wake word", None, cx))
        .child(rows_card(
            [
                setting_row(
                    "Listen for the wake phrase",
                    Some(
                        "Battery cost: keeps the microphone and speech recognition running \
                         the whole time ZLO is open, which drains a laptop noticeably. After \
                         the phrase and a short pause, ZLO captures what you say (20 seconds \
                         at most) and sends it."
                            .into(),
                    ),
                    switch("voice-wake", prefs.wake_enabled, "Wake word", |p, on| {
                        p.wake_enabled = on
                    }),
                    cx,
                ),
                setting_row(
                    "Phrase",
                    Some("Pick words the recognizer spells reliably. Enter saves.".into()),
                    div().w(px(180.)).child(Input::new(&view.wake_phrase)),
                    cx,
                ),
            ],
            cx,
        ))
}

/// The index of the option nearest `value`.
fn closest(options: impl Iterator<Item = f32>, value: f32) -> usize {
    options
        .enumerate()
        .min_by(|a, b| (a.1 - value).abs().total_cmp(&(b.1 - value).abs()))
        .map_or(0, |(ix, _)| ix)
}
