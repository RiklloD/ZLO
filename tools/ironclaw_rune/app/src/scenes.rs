//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("memory", memory),
    ("memory-draft", memory_draft),
    ("memory-supermemory", memory_supermemory),
    ("profile-editor", profile_editor),
    ("profile-pickers", profile_pickers),
    ("profiles", profiles),
    ("shell", shell),
    ("usage", usage),
    ("usage-compactions", usage_compactions),
    ("usage-history", usage_history),
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
