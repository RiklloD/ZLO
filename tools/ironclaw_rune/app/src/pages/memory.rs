//! What ZLO remembers (GPUI `pages/memory.rs`). Two clearly separate stores:
//!
//! - **Local**: the memory provider's documents (`MEMORY.md`, `USER.md`,
//!   `SOUL.md`, daily logs, notes). Browse, search, edit with a conflict
//!   check, create, and remove — remove archives the document under
//!   `archive/`, nothing is deleted.
//! - **Supermemory** (`supermemory.rs`): the chosen profile's container on
//!   the user's own Supermemory account — what was captured there, semantic
//!   search, and deleting a wrong document or forgetting a wrong memory.
//!
//! `draft.rs` (GPUI `memory_draft.rs`) holds the open document and the
//! late-reply rules for saves and archives.

mod draft;
mod supermemory;

use ironclaw_gpui_client::memory_page::{
    MemoryArchiveOutcome, MemoryDocument, MemoryDocumentEntry, MemoryDocumentKind,
    MemoryPageWriteOutcome, MemorySearchHit, SupermemoryDocumentPage, SupermemoryMemoryHit,
    SupermemoryStatus,
};
use rune::{Cursor, Cx, El, TextEdit, col, el, row};
use rune_kit::{Kit, Markdown, TextArea, TextInput};

use self::draft::{Effect, SaveReply, apply_archived, apply_save};
use super::usage::common_stub::{
    BODY_SEMIBOLD, Look, SM, SM_MEDIUM, XS, block_fill, button, empty_state, error_line, header,
    icon_button, label, page_body, refresh_button,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Local,
    Supermemory,
}

type Open = draft::Open<TextEdit>;

pub struct MemoryState {
    tab: Tab,
    error: Option<String>,
    notice: Option<String>,
    busy: bool,
    // local
    available: bool,
    loaded: bool,
    documents: Vec<MemoryDocumentEntry>,
    open: Option<Open>,
    /// Source of `Open::generation`; also tags open requests so only the
    /// latest one is shown.
    next_generation: u64,
    confirm_archive: bool,
    search: TextEdit,
    hits: Option<Vec<MemorySearchHit>>,
    new_path: TextEdit,
    // Supermemory
    profiles: Vec<(String, String)>,
    profile: Option<String>,
    status: Option<SupermemoryStatus>,
    container: Option<String>,
    page: Option<SupermemoryDocumentPage>,
    sm_search: TextEdit,
    sm_hits: Option<Vec<SupermemoryMemoryHit>>,
    confirm_remote: Option<String>,
    /// Takes an edit that arrives after the editor closed (`on_edit` must
    /// hand Rune some `TextEdit`).
    scratch: TextEdit,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            tab: Tab::Local,
            error: None,
            notice: None,
            busy: false,
            available: true,
            loaded: false,
            documents: Vec::new(),
            open: None,
            next_generation: 0,
            confirm_archive: false,
            search: TextEdit::new(""),
            hits: None,
            new_path: TextEdit::new(""),
            profiles: Vec::new(),
            profile: None,
            status: None,
            container: None,
            page: None,
            sm_search: TextEdit::new(""),
            sm_hits: None,
            confirm_remote: None,
            scratch: TextEdit::new(""),
        }
    }
}

fn kind_label(kind: MemoryDocumentKind) -> &'static str {
    match kind {
        MemoryDocumentKind::Standing => "Standing memory",
        MemoryDocumentKind::Identity => "Identity",
        MemoryDocumentKind::Profile => "Profile",
        MemoryDocumentKind::DailyLog => "Daily log",
        MemoryDocumentKind::Archived => "Archived",
        MemoryDocumentKind::Note => "Note",
    }
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

fn refresh(s: &mut State) {
    match s.pages.memory.tab {
        Tab::Local => load_tree(s),
        Tab::Supermemory => supermemory::load(s),
    }
}

fn fail(s: &mut State, error: impl std::fmt::Display) {
    let m = &mut s.pages.memory;
    m.busy = false;
    m.error = Some(error.to_string());
}

// --- local ---

fn load_tree(s: &mut State) {
    jobs::run(
        s,
        "memory",
        |c| async move { c.memory_tree().await },
        |s, result, _tx| {
            let m = &mut s.pages.memory;
            m.loaded = true;
            match result {
                Ok(tree) => {
                    m.available = tree.available;
                    m.documents = tree.documents;
                    m.error = None;
                }
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

impl MemoryState {
    fn bump_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }
}

fn open_document(s: &mut State, path: String) {
    let m = &mut s.pages.memory;
    m.confirm_archive = false;
    // Only the latest open request may replace what is shown.
    let request = m.bump_generation();
    jobs::run(
        s,
        "memory",
        move |c| async move { c.memory_document(&path).await },
        move |s, result, _tx| {
            let m = &mut s.pages.memory;
            if m.next_generation != request {
                return;
            }
            match result {
                Ok(document) => {
                    let generation = m.bump_generation();
                    m.open = Some(Open {
                        document,
                        editor: None,
                        conflict: false,
                        generation,
                    });
                    m.error = None;
                }
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

fn start_edit(s: &mut State) {
    let m = &mut s.pages.memory;
    let Some(open) = m.open.as_mut() else {
        return;
    };
    let mut editor = TextEdit::new(open.document.content.clone());
    editor.focused = true;
    open.editor = Some(editor);
    open.conflict = false;
    m.next_generation += 1;
    if let Some(open) = m.open.as_mut() {
        open.generation = m.next_generation;
    }
}

fn create(s: &mut State) {
    let m = &mut s.pages.memory;
    let path = m.new_path.text.trim().to_string();
    if path.is_empty() {
        return;
    }
    m.new_path.clear();
    let mut editor = TextEdit::new("");
    editor.focused = true;
    let generation = m.bump_generation();
    m.open = Some(Open {
        generation,
        document: MemoryDocument {
            kind: MemoryDocumentKind::Note,
            path,
            content: String::new(),
            content_hash: String::new(),
            word_count: 0,
            provenance: Default::default(),
        },
        editor: Some(editor),
        conflict: false,
    });
}

fn save(s: &mut State) {
    let m = &mut s.pages.memory;
    let Some(open) = m.open.as_ref() else {
        return;
    };
    let Some(editor) = open.editor.as_ref() else {
        return;
    };
    let content = editor.text.clone();
    let ticket = open.ticket();
    let path = ticket.path.clone();
    // A new document carries no hash, so the save cannot overwrite one.
    let expected = (!open.document.content_hash.is_empty()).then(|| open.document.content_hash.clone());
    m.busy = true;
    jobs::run(
        s,
        "memory",
        move |c| async move { c.memory_write(&path, &content, expected.as_deref()).await },
        move |s, result, _tx| {
            let m = &mut s.pages.memory;
            m.busy = false;
            let reply = match result {
                Ok(response) => match (response.outcome, response.document) {
                    (MemoryPageWriteOutcome::Written, Some(document)) => SaveReply::Written(document),
                    (MemoryPageWriteOutcome::Conflict, current) => SaveReply::Conflict(current),
                    (MemoryPageWriteOutcome::Written, None) => SaveReply::WrittenWithoutReadBack,
                },
                Err(e) => {
                    m.error = Some(e.to_string());
                    return;
                }
            };
            let saved = matches!(reply, SaveReply::Written(_));
            // Only the draft that sent this save is touched.
            match apply_save(&mut m.open, &ticket, reply, &mut m.next_generation) {
                Effect::Notice(text) => {
                    m.notice = Some(text);
                    m.error = None;
                }
                Effect::Error(text) => m.error = Some(text),
            }
            if saved {
                load_tree(s);
            }
        },
    );
}

fn cancel_edit(s: &mut State) {
    let m = &mut s.pages.memory;
    if let Some(open) = m.open.as_mut() {
        open.editor = None;
        open.conflict = false;
    }
    if m.open.as_ref().is_some_and(|o| o.document.content_hash.is_empty()) {
        m.open = None;
    }
}

fn archive(s: &mut State) {
    let m = &mut s.pages.memory;
    let Some(open) = m.open.as_ref() else {
        return;
    };
    let ticket = open.ticket();
    let (path, hash) = (ticket.path.clone(), open.document.content_hash.clone());
    m.confirm_archive = false;
    m.busy = true;
    jobs::run(
        s,
        "memory",
        move |c| async move { c.memory_archive(&path, &hash).await },
        move |s, result, _tx| {
            let m = &mut s.pages.memory;
            m.busy = false;
            match result {
                Ok(response) if response.outcome == MemoryArchiveOutcome::Archived => {
                    m.notice = Some(format!(
                        "Removed. Kept at {}.",
                        response.archived_path.unwrap_or_default()
                    ));
                    apply_archived(&mut m.open, &ticket);
                    load_tree(s);
                }
                Ok(_) => {
                    m.error = Some("It changed since you opened it; reopen and try again.".into());
                }
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

fn run_search(s: &mut State) {
    let m = &mut s.pages.memory;
    let query = m.search.text.trim().to_string();
    if query.is_empty() {
        m.hits = None;
        return;
    }
    jobs::run(
        s,
        "memory",
        move |c| async move { c.memory_search(&query, 20).await },
        |s, result, _tx| {
            let m = &mut s.pages.memory;
            match result {
                Ok(found) => m.hits = Some(found.hits),
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

// --- rendering ---

fn set_tab(s: &mut State, tab: Tab) {
    let m = &mut s.pages.memory;
    m.tab = tab;
    m.error = None;
    m.notice = None;
    refresh(s);
}

fn tabs(s: &State, k: &Kit) -> El {
    let tab = |key: &str, title: &str, which: Tab| {
        let look = if s.pages.memory.tab == which { Look::Primary } else { Look::Ghost };
        button(k, key, title, look, None, false, move |s: &mut State| set_tab(s, which))
    };
    row()
        .key("memory-tabs")
        .gap(8.0)
        .pb(12.0)
        .child(tab("memory-tab-local", "Local (on this Mac)", Tab::Local))
        .child(tab("memory-tab-supermemory", "Supermemory (remote)", Tab::Supermemory))
}

/// A text field with a trailing button, GPUI's `h_flex(Input, Button)`.
pub(super) fn field_row(field: El, action: El) -> El {
    row().w_full().gap(8.0).center_y().child(el().grow(1.0).min_w(0.0).child(field)).child(action)
}

fn render_local(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let m = &s.pages.memory;
    if m.loaded && !m.available {
        return empty_state(
            p,
            "The bound memory provider has no documents to show (memory is off or \
             served by a third-party provider).",
        );
    }
    let selected = m.open.as_ref().map(|o| o.document.path.as_str());
    let mut list = col().key("memory-docs").w_full();
    for entry in &m.documents {
        let path = entry.path.clone();
        let active = selected == Some(entry.path.as_str());
        list = list.child(
            row()
                .key(format!("memory-doc-{}", entry.path))
                .role("button")
                .label(entry.path.clone())
                .w_full()
                .gap(12.0)
                .py(6.0)
                .px(8.0)
                .radius(6.0)
                .center_y()
                .cursor(Cursor::Pointer)
                .fill(if active { p.active } else { rune::Color::TRANSPARENT })
                .hover(p.hover)
                .child(label(entry.path.clone(), SM, p.text).truncate().grow(1.0).min_w(0.0))
                .child(label(kind_label(entry.kind), XS, p.text_muted))
                .on_press(move |s: &mut State| open_document(s, path.clone())),
        );
    }
    let search = field_row(
        TextInput::new(&m.search)
            .width(None)
            .placeholder("Search local memory")
            .label("Search local memory")
            .on_edit(|s: &mut State| &mut s.pages.memory.search)
            .on_submit(|s: &mut State, _| run_search(s))
            .build(k)
            .key("memory-search-field"),
        icon_button(k, "memory-search", &icons::SEARCH, "Search local memory", run_search),
    );
    let mut c = col().w_full().gap(12.0).child(search);
    if let Some(hits) = &m.hits {
        if hits.is_empty() {
            c = c.child(empty_state(p, "Nothing matches.").key("memory-hits"));
        } else {
            let mut h = col().key("memory-hits").w_full().pb(16.0);
            for (ix, hit) in hits.iter().enumerate() {
                let path = hit.path.clone();
                h = h.child(
                    col()
                        .key(format!("memory-hit-{ix}"))
                        .role("button")
                        .w_full()
                        .py(6.0)
                        .cursor(Cursor::Pointer)
                        .child(label(hit.path.clone(), SM_MEDIUM, p.text))
                        .child(
                            label(hit.snippet.chars().take(240).collect::<String>(), XS, p.text_muted).wrap(),
                        )
                        .on_press(move |s: &mut State| open_document(s, path.clone())),
                );
                h = h.child(super::usage::common_stub::rule(p).key(format!("memory-hit-rule-{ix}")));
            }
            c = c.child(h);
        }
    }
    c = c.child(field_row(
        TextInput::new(&m.new_path)
            .width(None)
            .placeholder("New document, e.g. notes/work.md")
            .label("New document path")
            .on_edit(|s: &mut State| &mut s.pages.memory.new_path)
            .on_submit(|s: &mut State, _| create(s))
            .build(k)
            .key("memory-new-field"),
        button(k, "memory-new", "New", Look::Outline, Some(&icons::PLUS), false, create),
    ));
    if m.loaded && m.documents.is_empty() {
        c = c.child(empty_state(p, "No memory documents yet."));
    }
    c = c.child(list);
    if let Some(open) = render_open(s, k) {
        c = c.child(open);
    }
    c
}

fn render_open(s: &State, k: &Kit) -> Option<El> {
    let p = &s.palette;
    let m = &s.pages.memory;
    let open = m.open.as_ref()?;
    let doc = &open.document;
    let is_new = doc.content_hash.is_empty();
    let archived = doc.kind == MemoryDocumentKind::Archived;
    let provenance = if is_new {
        "New document".to_string()
    } else {
        let mut line = format!(
            "{} · {} words · {} earlier version{}",
            kind_label(doc.kind),
            doc.word_count,
            doc.provenance.version_count,
            if doc.provenance.version_count == 1 { "" } else { "s" }
        );
        if let Some(by) = &doc.provenance.last_changed_by {
            line.push_str(&format!(" · last saved by {by}"));
        }
        line.push_str(" · which thread wrote it is not recorded");
        line
    };
    let actions = match (&open.editor, archived) {
        (Some(_), _) => row()
            .gap(8.0)
            .child(button(k, "memory-cancel", "Cancel", Look::Ghost, None, false, cancel_edit))
            .child(button(
                k,
                "memory-save",
                if open.conflict { "Save anyway" } else { "Save" },
                Look::Primary,
                None,
                m.busy,
                save,
            )),
        (None, true) => row(),
        (None, false) => row()
            .gap(8.0)
            .child(button(k, "memory-edit", "Edit", Look::Outline, None, false, start_edit))
            .child(if m.confirm_archive {
                button(
                    k,
                    "memory-archive-confirm",
                    "Remove (keeps a copy in archive/)",
                    Look::Danger,
                    None,
                    m.busy,
                    archive,
                )
            } else {
                icon_button(k, "memory-archive", &icons::DELETE, "Remove", |s: &mut State| {
                    s.pages.memory.confirm_archive = true
                })
            }),
    };
    let body = match &open.editor {
        // PORT-STUB(area G): GPUI draws the editor in the mono font; kit's
        // TextArea takes no text style yet.
        Some(editor) => TextArea::new(editor)
            .auto_grow(10, 36)
            .width(None)
            .label(format!("Edit {}", doc.path))
            .on_edit(|s: &mut State| match s.pages.memory.open.as_mut().and_then(|o| o.editor.as_mut()) {
                Some(e) => e,
                // The editor closed between frames: the edit lands nowhere.
                None => &mut s.pages.memory.scratch,
            })
            .build(k)
            .key(format!("memory-editor-{}", open.generation)),
        None => Markdown::new(doc.content.clone())
            .selectable(true)
            .build(k)
            .key(format!("memory:{}", doc.path)),
    };
    let mut card = col()
        .key("memory-open")
        .w_full()
        .gap(8.0)
        .mt(16.0)
        .padding(16.0)
        .radius(12.0)
        .fill(block_fill(p))
        .child(
            row()
                .w_full()
                .gap(12.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .child(label(doc.path.clone(), BODY_SEMIBOLD, p.text))
                        .child(label(provenance, XS, p.text_muted).wrap()),
                )
                .child(actions),
        );
    if open.conflict {
        card = card.child(
            label(
                "This document changed since you opened it (the agent may have \
                 written to it). Your text is kept; Save anyway replaces the \
                 newer version, which stays in the history.",
                SM,
                p.danger,
            )
            .wrap(),
        );
    }
    Some(card.child(body))
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let m = &s.pages.memory;
    let actions = refresh_button(k, "refresh-memory", refresh);
    let content = match m.tab {
        Tab::Local => render_local(s, k).key("memory-local"),
        Tab::Supermemory => supermemory::render(s, k).key("memory-supermemory"),
    };
    let mut body = col().w_full().child(header(p, "Memory", actions)).child(tabs(s, k));
    if let Some(e) = &m.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &m.notice {
        body = body.child(el().pb(12.0).child(label(n.clone(), SM, p.text_muted)));
    }
    page_body("memory-page", body.child(content))
}

// --- fixtures (scenes.rs) ---

fn fixture_tree(m: &mut MemoryState) {
    use MemoryDocumentKind::*;
    m.loaded = true;
    m.available = true;
    m.documents = [
        ("MEMORY.md", Standing),
        ("USER.md", Profile),
        ("SOUL.md", Identity),
        ("daily/2026-11-07.md", DailyLog),
        ("daily/2026-11-06.md", DailyLog),
        ("notes/rune-port.md", Note),
        ("archive/notes/old-plan.md", Archived),
    ]
    .into_iter()
    .map(|(path, kind)| MemoryDocumentEntry { path: path.into(), kind })
    .collect();
}

fn fixture_doc() -> MemoryDocument {
    MemoryDocument {
        path: "MEMORY.md".into(),
        kind: MemoryDocumentKind::Standing,
        content: "# Standing memory\n\n- The user ports **ZLO** from GPUI to Rune, file by file.\n\
                  - Prefers short commit messages ending with a co-author line.\n\
                  - Never deletes LLM data; archives instead.\n\n## Projects\n\n\
                  1. IronClaw Reborn\n2. ZLO desktop app\n"
            .into(),
        content_hash: "sha256:fixture".into(),
        word_count: 38,
        provenance: ironclaw_gpui_client::memory_page::MemoryProvenance {
            version_count: 4,
            last_changed_by: Some("agent".into()),
        },
    }
}

/// Local tab: the document list, a search with hits, MEMORY.md open.
pub fn fixture_local(s: &mut State) {
    let m = &mut s.pages.memory;
    fixture_tree(m);
    m.search = TextEdit::new("rune");
    m.hits = Some(vec![MemorySearchHit {
        path: "notes/rune-port.md".into(),
        snippet: "Port the GPUI app to Rune: one state tree, slots per area, headless scenes for every page."
            .into(),
        score: 0.82,
    }]);
    m.next_generation = 1;
    m.open = Some(Open {
        document: fixture_doc(),
        editor: None,
        conflict: false,
        generation: 1,
    });
}

/// Local tab: MEMORY.md in the editor after a save hit a newer version.
pub fn fixture_draft(s: &mut State) {
    fixture_local(s);
    let m = &mut s.pages.memory;
    m.hits = None;
    m.search = TextEdit::new("");
    m.error = Some("MEMORY.md changed since you opened it; your text is kept.".into());
    if let Some(open) = m.open.as_mut() {
        open.editor = Some(TextEdit::new(
            "# Standing memory\n\n- The user ports ZLO to Rune.\n- My edit, not yet saved.\n",
        ));
        open.conflict = true;
    }
}

/// Supermemory tab: a profile with its container, a search and documents.
pub fn fixture_supermemory(s: &mut State) {
    supermemory::fixture(&mut s.pages.memory);
    s.pages.memory.tab = Tab::Supermemory;
}
