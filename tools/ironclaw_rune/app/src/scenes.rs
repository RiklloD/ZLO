//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("extensions", crate::pages::extensions::fixtures::extensions),
    ("hooks", crate::pages::hooks::fixture_hooks),
    ("operator", crate::pages::operator::fixtures::status),
    ("operator-config", crate::pages::operator::fixtures::config),
    ("operator-extensions", crate::pages::operator::fixtures::extension_config),
    ("operator-inspector", crate::pages::operator::fixtures::inspector),
    ("operator-logs", crate::pages::operator::fixtures::logs),
    ("operator-setup", crate::pages::operator::fixtures::setup),
    ("providers", crate::pages::providers::fixtures::providers),
    ("providers-catalog", crate::pages::providers::fixtures::catalog),
    ("providers-cli", crate::pages::providers::fixtures::cli),
    ("shell", shell),
    ("skills", crate::pages::skills::fixtures::skills),
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
