//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("automation-form", |s| page(s, Page::Automations, crate::pages::automations::fixture_form)),
    ("automations", |s| page(s, Page::Automations, crate::pages::automations::fixture)),
    ("appearance", |s| page(s, Page::Appearance, crate::pages::settings::fixture)),
    ("files", |s| page(s, Page::Files, crate::pages::files::fixture)),
    ("notifications", |s| page(s, Page::Notifications, crate::pages::notifications::fixture)),
    ("permissions", |s| page(s, Page::Permissions, crate::pages::settings::fixture)),
    ("projects", |s| page(s, Page::Projects, crate::pages::projects::fixture)),
    ("settings", |s| page(s, Page::Settings, crate::pages::settings::fixture)),
    ("shell", shell),
    ("traces", |s| page(s, Page::Traces, crate::pages::settings::fixture)),
];

/// A page scene: the shell, then `fill` puts the page's fixture data in.
fn page(s: &mut State, page: Page, fill: fn(&mut State)) {
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
    s.workspace.threads = vec![
        ("t1".into(), "Port the frontend to Rune".into()),
        ("t2".into(), "Fix the flaky integration test".into()),
    ];
}
