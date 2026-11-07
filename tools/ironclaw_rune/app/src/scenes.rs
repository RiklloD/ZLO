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
    ("model-picker", |s| crate::chat::model_picker::scene(s, false)),
    ("model-picker-effort", |s| crate::chat::model_picker::scene(s, true)),
    ("orchestration", crate::chat::orchestration::scene),
    ("shell", shell),
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

/// The shell with thread `t1` open as an empty chat: the base of the chat
/// extras' scenes (area H).
pub fn chat(s: &mut State) {
    shell(s);
    s.chat = Some(crate::chat::ChatState::new("t1".into()));
}
