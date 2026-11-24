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

/// The UI font size the `*-large-type` scenes ask for (default 16).
const LARGE_TYPE: f32 = 20.;

pub const SCENES: &[Scene] = &[
    // Frame-time bench scenes (`bench.rs`, `bench/run.py`).
    ("bench-dock", crate::bench::dock),
    ("bench-scroll", crate::bench::scroll),
    ("bench-shelf", crate::bench::shelf),
    ("bench-sidebar", crate::bench::sidebar),
    ("bench-stream", crate::bench::stream),
    ("bench-switch", crate::bench::switch),
    ("bench-tools", crate::bench::tools),
    ("bench-typing", crate::bench::typing),
    ("chat-context-banner", crate::chat::fixtures::context_banner),
    ("chat-long-draft", crate::chat::fixtures::long_draft),
    ("chat-wallpaper-conversation", |s| {
        crate::chat::fixtures::wallpaper_area(s, "conversation")
    }),
    (
        "chat-wallpaper-window",
        crate::chat::fixtures::wallpaper_window,
    ),
    ("chat-wallpaper-pane", |s| {
        crate::chat::fixtures::wallpaper_area(s, "pane")
    }),
    // A reconnect's loading screen (the caption shows late).
    ("loading", |s| {
        s.fixture = true;
        s.conn = crate::state::Conn::Connecting;
    }),
    // The launch intro frozen at chosen seconds (`workspace::intro`).
    ("launch-0.15", |s| launch(s, 0.15)),
    ("launch-0.3", |s| launch(s, 0.3)),
    ("launch-0.45", |s| launch(s, 0.45)),
    ("launch-0.6", |s| launch(s, 0.6)),
    ("launch-0.75", |s| launch(s, 0.75)),
    ("launch-0.9", |s| launch(s, 0.9)),
    ("launch-1.05", |s| launch(s, 1.05)),
    ("launch-1.15", |s| launch(s, 1.15)),
    ("launch-1.25", |s| launch_reveal(s, 1.25)),
    ("launch-1.35", |s| launch_reveal(s, 1.35)),
    ("launch-1.5", |s| launch_reveal(s, 1.5)),
    ("launch-wait", |s| launch(s, 2.5)),
    ("launch-0.6-light", |s| {
        launch(s, 0.6);
        light_theme(s);
    }),
    ("launch-1.15-light", |s| {
        launch(s, 1.15);
        light_theme(s);
    }),
    ("launch-1.35-light", |s| {
        launch_reveal(s, 1.35);
        light_theme(s);
    }),
    ("launch-reduced", |s| {
        launch(s, 0.1);
        s.intro = crate::workspace::intro::Intro::frozen(0.1, None, true);
    }),
    ("appearance", |s| {
        page_with(s, Page::Appearance, crate::pages::settings::fixture)
    }),
    // Appearance → UI font size at 20 pt: every shared text style follows.
    ("appearance-large-type", |s| {
        page_with(s, Page::Appearance, crate::pages::settings::fixture);
        s.prefs.ui_font_size = LARGE_TYPE;
    }),
    ("appearance-translucent", |s| {
        page_with(s, Page::Appearance, crate::pages::settings::fixture);
        // Headless renders draw the shell's translucent fills (no material).
        s.prefs.translucent = true;
    }),
    ("computers", |s| {
        page_with(
            s,
            Page::Computers,
            crate::pages::computers::fixtures::computers,
        )
    }),
    ("computers-light", |s| {
        page_with(s, Page::Chat, crate::pages::computers::fixtures::light)
    }),
    ("computers-light-paired", |s| {
        page_with(
            s,
            Page::Computers,
            crate::pages::computers::fixtures::light_paired,
        )
    }),
    ("computers-pairing", |s| {
        page_with(
            s,
            Page::Computers,
            crate::pages::computers::fixtures::pairing,
        )
    }),
    ("computers-remote", |s| {
        page_with(
            s,
            Page::Computers,
            crate::pages::computers::fixtures::remote,
        )
    }),
    ("automation-form", |s| {
        page_with(
            s,
            Page::Automations,
            crate::pages::automations::fixture_form,
        )
    }),
    ("automations", |s| {
        page_with(s, Page::Automations, crate::pages::automations::fixture)
    }),
    ("chat", crate::chat::fixtures::chat),
    ("chat-gate", crate::chat::fixtures::gate),
    ("chat-long", crate::chat::fixtures::long),
    ("chat-large-type", |s| {
        crate::chat::side::fixtures::activity(s);
        s.prefs.ui_font_size = LARGE_TYPE;
    }),
    ("chat-menu", crate::chat::fixtures::menu),
    ("chat-question", crate::chat::fixtures::question),
    ("chat-running", crate::chat::fixtures::running),
    ("chat-cli-turn", crate::chat::fixtures::cli_turn_settled),
    ("chat-cli-turn-open", crate::chat::fixtures::cli_turn_open),
    ("chat-cli-turn-live", crate::chat::fixtures::cli_turn_live),
    (
        "chat-commentary-live",
        crate::chat::fixtures::commentary_live,
    ),
    ("chat-steering", crate::chat::fixtures::steering),
    ("chat-steering-queue-default", |s| {
        crate::chat::fixtures::steering(s);
        s.prefs.follow_ups = crate::design::prefs::FollowUps::Queue;
    }),
    ("command-menu", ws::fixture_command_menu),
    ("computer", |s| crate::chat::computer::scene(s, false)),
    ("computer-big", |s| crate::chat::computer::scene(s, true)),
    ("context-meter", ws::fixture_context_meter),
    ("disconnected", ws::fixture_disconnected),
    ("draft", ws::fixture_draft),
    ("draft-light-theme", |s| {
        ws::fixture_draft(s);
        light_theme(s);
    }),
    ("chat-light-theme", |s| {
        crate::chat::fixtures::chat(s);
        light_theme(s);
    }),
    ("draft-files", ws::fixture_draft_files),
    ("draft-mention", ws::fixture_draft_mention),
    ("extensions", crate::pages::extensions::fixtures::extensions),
    // Plugins: consent, a plugin's settings page, its side tab and badges.
    ("plugin-consent", |s| {
        crate::pages::extensions::fixtures::plugins_consent(s, false)
    }),
    ("plugin-consent-light", |s| {
        crate::pages::extensions::fixtures::plugins_consent(s, true)
    }),
    (
        "plugin-list",
        crate::pages::extensions::fixtures::plugins_list,
    ),
    ("plugin-settings", |s| {
        crate::pages::extensions::fixtures::plugin_settings(s, false)
    }),
    ("plugin-settings-light", |s| {
        crate::pages::extensions::fixtures::plugin_settings(s, true)
    }),
    ("plugin-panel", |s| {
        crate::plugins::fixtures::panel(s, false)
    }),
    ("plugin-panel-light", |s| {
        crate::plugins::fixtures::panel(s, true)
    }),
    ("plugin-panel-error", crate::plugins::fixtures::panel_error),
    ("files", |s| {
        page_with(s, Page::Files, crate::pages::files::fixture)
    }),
    ("goals", crate::chat::goals::scene),
    ("hooks", crate::pages::hooks::fixture_hooks),
    ("inbox", ws::fixture_inbox),
    ("inbox-long", ws::fixture_inbox_long),
    ("inbox-short", ws::fixture_inbox_short),
    ("inbox-short-large-type", |s| {
        ws::fixture_inbox_short(s);
        s.prefs.ui_font_size = LARGE_TYPE;
    }),
    ("inbox-subagents", ws::fixture_inbox_subagents),
    ("kit-gallery", gallery::fixture_kit_gallery),
    ("kit-gallery-light", gallery::fixture_kit_gallery_light),
    ("look-wallpaper-ascii", gallery::fixture_wallpaper_ascii),
    ("look-wallpaper-dither", gallery::fixture_wallpaper_dither),
    (
        "look-wallpaper-dither-full",
        gallery::fixture_wallpaper_dither_full,
    ),
    (
        "look-wallpaper-halftone",
        gallery::fixture_wallpaper_halftone,
    ),
    (
        "look-wallpaper-scanlines",
        gallery::fixture_wallpaper_scanlines,
    ),
    ("look-wallpaper-scene", gallery::fixture_wallpaper_scene),
    ("memory", memory),
    ("memory-draft", memory_draft),
    ("memory-supermemory", memory_supermemory),
    ("model-picker", |s| {
        crate::chat::model_picker::scene(s, false)
    }),
    ("model-picker-large-type", |s| {
        crate::chat::model_picker::scene(s, false);
        s.prefs.ui_font_size = LARGE_TYPE;
    }),
    ("model-picker-effort", |s| {
        crate::chat::model_picker::scene(s, true)
    }),
    ("notifications", |s| {
        page_with(s, Page::Notifications, crate::pages::notifications::fixture)
    }),
    ("operator", crate::pages::operator::fixtures::status),
    ("operator-config", crate::pages::operator::fixtures::config),
    (
        "operator-extensions",
        crate::pages::operator::fixtures::extension_config,
    ),
    (
        "operator-inspector",
        crate::pages::operator::fixtures::inspector,
    ),
    ("operator-logs", crate::pages::operator::fixtures::logs),
    ("operator-setup", crate::pages::operator::fixtures::setup),
    ("orchestration", crate::chat::orchestration::scene),
    ("permissions", |s| {
        page_with(s, Page::Permissions, crate::pages::settings::fixture)
    }),
    ("profile-editor", profile_editor),
    ("profile-menu", ws::fixture_profile_menu),
    ("profile-menu-search", ws::fixture_profile_menu_search),
    ("profile-pickers", profile_pickers),
    ("profiles", profiles),
    ("projects", |s| {
        page_with(s, Page::Projects, crate::pages::projects::fixture)
    }),
    ("onboarding-agents", crate::onboarding::fixtures::agents),
    ("onboarding-connect", crate::onboarding::fixtures::connect),
    ("onboarding-pairing", crate::onboarding::fixtures::pairing),
    ("onboarding-projects", crate::onboarding::fixtures::projects),
    (
        "onboarding-projects-loading",
        crate::onboarding::fixtures::projects_loading,
    ),
    (
        "onboarding-providers",
        crate::onboarding::fixtures::providers,
    ),
    ("onboarding-toast", crate::onboarding::fixtures::toast),
    ("providers", crate::pages::providers::fixtures::providers),
    ("providers-large-type", |s| {
        crate::pages::providers::fixtures::providers(s);
        s.prefs.ui_font_size = LARGE_TYPE;
    }),
    (
        "providers-catalog",
        crate::pages::providers::fixtures::catalog,
    ),
    ("providers-cli", crate::pages::providers::fixtures::cli),
    ("quick-composer", ws::fixture_quick),
    ("settings", |s| {
        page_with(s, Page::Settings, crate::pages::settings::fixture)
    }),
    ("settings-nav", ws::fixture_settings_nav),
    ("shell", ws::fixture_shell),
    ("side-activity", crate::chat::side::fixtures::activity),
    ("side-agents", crate::chat::side::fixtures::agents),
    ("side-browser", crate::chat::side::fixtures::browser),
    ("side-changes", crate::chat::side::fixtures::changes),
    (
        "side-changes-clean",
        crate::chat::side::fixtures::changes_clean,
    ),
    ("side-console", crate::chat::side::fixtures::console),
    (
        "side-console-empty",
        crate::chat::side::fixtures::console_empty,
    ),
    ("side-details", crate::chat::side::fixtures::details),
    ("side-details-light", |s| {
        crate::chat::side::fixtures::details(s);
        light_theme(s);
    }),
    // The wallpaper lies over the whole window: the card must stay opaque.
    ("side-details-wallpaper", |s| {
        crate::chat::side::fixtures::details(s);
        s.prefs.wallpaper = crate::design::prefs::Wallpaper::Scene {
            name: "dusk".into(),
        };
        s.prefs.wallpaper_effect = "none".into();
        s.prefs.wallpaper_area = "window".into();
        s.prefs.wallpaper_opacity = 0.35;
        crate::design::look::fixture_wallpaper(s, (1280, 820));
    }),
    ("side-expanded", crate::chat::side::fixtures::side_expanded),
    ("side-files", crate::chat::side::fixtures::files),
    ("side-machines", crate::chat::side::fixtures::machines),
    ("side-history", crate::chat::side::fixtures::history),
    ("side-no-folder", crate::chat::side::fixtures::no_folder),
    ("side-outputs", crate::chat::side::fixtures::outputs),
    ("side-remote", crate::chat::side::fixtures::remote),
    ("side-routines", crate::chat::side::fixtures::routines),
    ("side-session", crate::chat::side::fixtures::session),
    (
        "side-work-expanded",
        crate::chat::side::fixtures::work_expanded,
    ),
    (
        "side-session-picker",
        crate::chat::side::fixtures::session_picker,
    ),
    ("skills", crate::pages::skills::fixtures::skills),
    (
        "skills-editor",
        crate::pages::skills::fixtures::skills_editor,
    ),
    ("snapshot-chip", snapshot_chip),
    ("status-bar", ws::fixture_status_bar),
    ("thread-drag", ws::fixture_thread_drag),
    ("thread-drag-draft", ws::fixture_thread_drag_draft),
    ("traces", |s| {
        page_with(s, Page::Traces, crate::pages::settings::fixture)
    }),
    ("usage", usage),
    ("usage-compactions", usage_compactions),
    ("usage-history", usage_history),
    ("voice-dictating", voice_dictating),
    ("voice-hud", voice_hud),
    ("voice-settings", voice_settings),
    ("voice-talk", voice_talk),
];

/// The launch intro at `t` seconds, still connecting.
fn launch(s: &mut State, t: f64) {
    s.fixture = true;
    s.prefs = Default::default();
    s.palette = crate::design::resolve(&s.prefs, true);
    s.conn = Conn::Connecting;
    s.intro = crate::workspace::intro::Intro::frozen(t, None, false);
}

/// The launch intro at `t` seconds, irising open onto the shell (the
/// connection landed before the assembly ended).
fn launch_reveal(s: &mut State, t: f64) {
    ws::fixture_shell(s);
    s.prefs = Default::default();
    s.palette = crate::design::resolve(&s.prefs, true);
    s.intro =
        crate::workspace::intro::Intro::frozen(t, Some(crate::workspace::intro::ASSEMBLE), false);
}

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
    s.workspace
        .titles
        .remember("t1", "Port the frontend to Rune");
    s.workspace
        .titles
        .remember("t2", "Fix the flaky integration test");
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

/// The light scheme on IronClaw's light theme, for the light renders.
fn light_theme(s: &mut State) {
    s.prefs.scheme = "light".into();
    s.palette = crate::design::resolve(&s.prefs, false);
}
