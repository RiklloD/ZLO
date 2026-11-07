//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("computer", |s| crate::chat::computer::scene(s, false)),
    ("computer-big", |s| crate::chat::computer::scene(s, true)),
    ("goals", crate::chat::goals::scene),
    ("memory", memory),
    ("memory-draft", memory_draft),
    ("memory-supermemory", memory_supermemory),
    ("model-picker", |s| crate::chat::model_picker::scene(s, false)),
    ("model-picker-effort", |s| crate::chat::model_picker::scene(s, true)),
    ("orchestration", crate::chat::orchestration::scene),
    ("profile-editor", profile_editor),
    ("profile-pickers", profile_pickers),
    ("profiles", profiles),
    ("shell", shell),
    ("snapshot-chip", snapshot_chip),
    ("usage", usage),
    ("usage-compactions", usage_compactions),
    ("usage-history", usage_history),
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

fn page(s: &mut State, page: Page) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = page;
}

/// Usage, Limits: live providers with pace ticks, idle ones collapsed.
fn usage(s: &mut State) {
    page(s, Page::Usage);
    crate::pages::usage::fixture_limits(s);
}

/// Usage, Cost over 30 days: overview, chart, totals, breakdown.
fn usage_history(s: &mut State) {
    page(s, Page::Usage);
    crate::pages::usage::fixture_history(s, crate::pages::usage::Metric::Cost);
}

/// Usage, Tokens, scrolled to the idle compactions.
fn usage_compactions(s: &mut State) {
    page(s, Page::Usage);
    crate::pages::usage::fixture_history(s, crate::pages::usage::Metric::Tokens);
}

/// Memory, Local: documents, a search with a hit, MEMORY.md open.
fn memory(s: &mut State) {
    page(s, Page::Memory);
    crate::pages::memory::fixture_local(s);
}

/// Memory, Local: MEMORY.md in the editor after a conflicting save.
fn memory_draft(s: &mut State) {
    page(s, Page::Memory);
    crate::pages::memory::fixture_draft(s);
}

/// Memory, Supermemory: a profile's container, hits and documents.
fn memory_supermemory(s: &mut State) {
    page(s, Page::Memory);
    crate::pages::memory::fixture_supermemory(s);
}

/// Profiles: the list, recently used first, with an import notice.
fn profiles(s: &mut State) {
    page(s, Page::Profiles);
    crate::pages::profiles::fixture_list(s);
}

/// Profiles: the editor on one profile.
fn profile_editor(s: &mut State) {
    page(s, Page::Profiles);
    crate::pages::profiles::fixture_editor(s);
}

/// Profiles: the editor with the tools checklist open.
fn profile_pickers(s: &mut State) {
    page(s, Page::Profiles);
    crate::pages::profiles::fixture_pickers(s);
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

/// The shell with thread `t1` open as an empty chat: the base of the chat
/// extras' scenes (area H).
pub fn chat(s: &mut State) {
    shell(s);
    s.chat = Some(crate::chat::ChatState::new("t1".into()));
}
