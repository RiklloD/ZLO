//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::design::gallery;
use crate::state::{Conn, Page, State};
use crate::workspace::fixtures as ws;

/// One scene: its name and the fixture that fills the state.
pub type Scene = (&'static str, fn(&mut State));

pub const SCENES: &[Scene] = &[
    ("appearance", |s| page_with(s, Page::Appearance, crate::pages::settings::fixture)),
    ("automation-form", |s| page_with(s, Page::Automations, crate::pages::automations::fixture_form)),
    ("automations", |s| page_with(s, Page::Automations, crate::pages::automations::fixture)),
    ("chat", crate::chat::fixtures::chat),
    ("chat-gate", crate::chat::fixtures::gate),
    ("chat-menu", crate::chat::fixtures::menu),
    ("chat-question", crate::chat::fixtures::question),
    ("chat-running", crate::chat::fixtures::running),
    ("command-menu", ws::fixture_command_menu),
    ("computer", |s| crate::chat::computer::scene(s, false)),
    ("computer-big", |s| crate::chat::computer::scene(s, true)),
    ("context-meter", ws::fixture_context_meter),
    ("disconnected", ws::fixture_disconnected),
    ("draft", ws::fixture_draft),
    ("extensions", crate::pages::extensions::fixtures::extensions),
    ("files", |s| page_with(s, Page::Files, crate::pages::files::fixture)),
    ("goals", crate::chat::goals::scene),
    ("hooks", crate::pages::hooks::fixture_hooks),
    ("inbox", ws::fixture_inbox),
    ("inbox-subagents", ws::fixture_inbox_subagents),
    ("kit-gallery", gallery::fixture_kit_gallery),
    ("kit-gallery-light", gallery::fixture_kit_gallery_light),
    ("look-wallpaper-ascii", gallery::fixture_wallpaper_ascii),
    ("look-wallpaper-dither", gallery::fixture_wallpaper_dither),
    ("look-wallpaper-dither-full", gallery::fixture_wallpaper_dither_full),
    ("look-wallpaper-halftone", gallery::fixture_wallpaper_halftone),
    ("look-wallpaper-scanlines", gallery::fixture_wallpaper_scanlines),
    ("look-wallpaper-scene", gallery::fixture_wallpaper_scene),
    ("memory", memory),
    ("memory-draft", memory_draft),
    ("memory-supermemory", memory_supermemory),
    ("model-picker", |s| crate::chat::model_picker::scene(s, false)),
    ("model-picker-effort", |s| crate::chat::model_picker::scene(s, true)),
    ("notifications", |s| page_with(s, Page::Notifications, crate::pages::notifications::fixture)),
    ("operator", crate::pages::operator::fixtures::status),
    ("operator-config", crate::pages::operator::fixtures::config),
    ("operator-extensions", crate::pages::operator::fixtures::extension_config),
    ("operator-inspector", crate::pages::operator::fixtures::inspector),
    ("operator-logs", crate::pages::operator::fixtures::logs),
    ("operator-setup", crate::pages::operator::fixtures::setup),
    ("orchestration", crate::chat::orchestration::scene),
    ("permissions", |s| page_with(s, Page::Permissions, crate::pages::settings::fixture)),
    ("profile-editor", profile_editor),
    ("profile-pickers", profile_pickers),
    ("profiles", profiles),
    ("projects", |s| page_with(s, Page::Projects, crate::pages::projects::fixture)),
    ("providers", crate::pages::providers::fixtures::providers),
    ("providers-catalog", crate::pages::providers::fixtures::catalog),
    ("providers-cli", crate::pages::providers::fixtures::cli),
    ("quick-composer", ws::fixture_quick),
    ("settings", |s| page_with(s, Page::Settings, crate::pages::settings::fixture)),
    ("settings-nav", ws::fixture_settings_nav),
    ("shell", ws::fixture_shell),
    ("side-activity", crate::chat::side::fixtures::activity),
    ("side-agents", crate::chat::side::fixtures::agents),
    ("side-changes", crate::chat::side::fixtures::changes),
    ("side-console", crate::chat::side::fixtures::console),
    ("side-files", crate::chat::side::fixtures::files),
    ("side-history", crate::chat::side::fixtures::history),
    ("side-no-folder", crate::chat::side::fixtures::no_folder),
    ("side-outputs", crate::chat::side::fixtures::outputs),
    ("side-routines", crate::chat::side::fixtures::routines),
    ("side-session", crate::chat::side::fixtures::session),
    ("skills", crate::pages::skills::fixtures::skills),
    ("snapshot-chip", snapshot_chip),
    ("status-bar", ws::fixture_status_bar),
    ("traces", |s| page_with(s, Page::Traces, crate::pages::settings::fixture)),
    ("usage", usage),
    ("usage-compactions", usage_compactions),
    ("usage-history", usage_history),
    ("voice-dictating", voice_dictating),
    ("voice-hud", voice_hud),
    ("voice-settings", voice_settings),
    ("voice-talk", voice_talk),
];

/// A page scene: the shell, then `fill` puts the page's fixture data in.
fn page_with(s: &mut State, page: Page, fill: fn(&mut State)) {
    shell(s);
    // Default preferences, not the user's file: scenes look the same anywhere.
    s.prefs = Default::default();
    s.palette = crate::design::resolve(&s.prefs, true);
    s.page = page;
    fill(s);
}

/// The window with a few threads and nothing open.
fn shell(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    s.workspace.titles.remember("t1", "Port the frontend to Rune");
    s.workspace.titles.remember("t2", "Fix the flaky integration test");
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
