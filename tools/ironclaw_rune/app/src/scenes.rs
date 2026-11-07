//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("shell", shell),
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
