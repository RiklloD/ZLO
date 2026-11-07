//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::State;
use crate::workspace::fixtures as ws;

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("command-menu", ws::fixture_command_menu),
    ("context-meter", ws::fixture_context_meter),
    ("disconnected", ws::fixture_disconnected),
    ("draft", ws::fixture_draft),
    ("inbox", ws::fixture_inbox),
    ("inbox-subagents", ws::fixture_inbox_subagents),
    ("quick-composer", ws::fixture_quick),
    ("settings-nav", ws::fixture_settings_nav),
    ("shell", ws::fixture_shell),
    ("status-bar", ws::fixture_status_bar),
];
