//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("shell", shell),
    ("snapshot-chip", snapshot_chip),
    ("voice-dictating", voice_dictating),
    ("voice-hud", voice_hud),
    ("voice-settings", voice_settings),
    ("voice-talk", voice_talk),
];

/// The window with a few threads and nothing open.
fn shell(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    s.workspace.threads = vec![
        ("t1".into(), "Port the frontend to Rune".into()),
        ("t2".into(), "Fix the flaky integration test".into()),
    ];
}

/// Settings → Voice with the saved defaults.
fn voice_settings(s: &mut State) {
    shell(s);
    s.page = Page::Voice;
}

/// A reply read aloud: the Speaking pill, and a notice with its Settings link.
fn voice_hud(s: &mut State) {
    voice_settings(s);
    crate::voice::fixtures::with_status(s, &["read"], None);
    let (text, pane) = crate::voice::fixtures::dictation_off();
    crate::voice::fixtures::notice(s, text, pane);
    crate::voice::fixtures::preview(s, crate::voice::Composer::Chat("t1".into()), Vec::new());
}

/// Dictating into the open chat's composer: the Listening pill, mic on.
fn voice_dictating(s: &mut State) {
    voice_settings(s);
    let composer = crate::voice::Composer::Chat("t1".into());
    crate::voice::fixtures::with_status(s, &["click", "words"], Some(composer.clone()));
    crate::voice::fixtures::preview(s, composer, Vec::new());
}

/// Talk mode waiting for the reply: the Thinking pill, talk button on.
fn voice_talk(s: &mut State) {
    voice_settings(s);
    let composer = crate::voice::Composer::Chat("t1".into());
    crate::voice::fixtures::with_status(s, &["talk", "words", "tick"], Some(composer.clone()));
    crate::voice::fixtures::preview(s, composer, Vec::new());
}

/// A staged SnapShot: one chip for the picture and its text part.
fn snapshot_chip(s: &mut State) {
    voice_settings(s);
    let staged = crate::snapshot::fixtures::staged_pair(s);
    crate::voice::fixtures::preview(s, crate::voice::Composer::Draft, staged);
}
