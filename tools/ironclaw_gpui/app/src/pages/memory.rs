//! What ZLO remembers. Two clearly separate stores:
//!
//! - **Local**: the memory provider's documents (`MEMORY.md`, `USER.md`,
//!   `SOUL.md`, daily logs, notes). Browse, search, edit with a conflict
//!   check, create, and remove — remove archives the document under
//!   `archive/`, nothing is deleted.
//! - **Supermemory**: the chosen profile's container on the user's own
//!   Supermemory account — what was captured there, semantic search, and
//!   deleting a wrong document or forgetting a wrong memory.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::memory_page::{
    MemoryArchiveOutcome, MemoryDocument, MemoryDocumentEntry, MemoryDocumentKind,
    MemoryPageWriteOutcome, MemorySearchHit, SupermemoryBindingOrigin, SupermemoryDocumentPage,
    SupermemoryMemoryHit, SupermemoryStatus,
};

use super::memory_draft::{Effect, SaveReply, apply_archived, apply_save};
use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Local,
    Supermemory,
}

type Open = super::memory_draft::Open<Entity<TextareaState>>;

pub struct MemoryView {
    backend: Backend,
    tab: Tab,
    error: Option<SharedString>,
    notice: Option<SharedString>,
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
    search: Entity<InputState>,
    hits: Option<Vec<MemorySearchHit>>,
    new_path: Entity<InputState>,
    // Supermemory
    profiles: Vec<(String, String)>,
    profile: Option<String>,
    status: Option<SupermemoryStatus>,
    container: Option<String>,
    page: Option<SupermemoryDocumentPage>,
    sm_search: Entity<InputState>,
    sm_hits: Option<Vec<SupermemoryMemoryHit>>,
    confirm_remote: Option<String>,
}

impl EventEmitter<PageEvent> for MemoryView {}

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

impl MemoryView {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search local memory"));
        let new_path = cx
            .new(|cx| InputState::new(window, cx).placeholder("New document, e.g. notes/work.md"));
        let sm_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search Supermemory"));
        for (input, which) in [(&search, 0u8), (&new_path, 1), (&sm_search, 2)] {
            cx.subscribe_in(input, window, move |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    match which {
                        0 => this.run_search(cx),
                        1 => this.create(window, cx),
                        _ => this.run_sm_search(cx),
                    }
                }
            })
            .detach();
        }
        let mut this = Self {
            backend,
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
            search,
            hits: None,
            new_path,
            profiles: Vec::new(),
            profile: None,
            status: None,
            container: None,
            page: None,
            sm_search,
            sm_hits: None,
            confirm_remote: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.tab {
            Tab::Local => self.load_tree(cx),
            Tab::Supermemory => self.load_supermemory(cx),
        }
    }

    fn fail(&mut self, error: impl std::fmt::Display, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(error.to_string().into());
        cx.notify();
    }

    // --- local ---

    fn load_tree(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.memory_tree().await },
            |this, result, cx| {
                this.loaded = true;
                match result {
                    Ok(tree) => {
                        this.available = tree.available;
                        this.documents = tree.documents;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn bump_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn open_document(&mut self, path: String, cx: &mut Context<Self>) {
        self.confirm_archive = false;
        // Only the latest open request may replace what is shown.
        let request = self.bump_generation();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.memory_document(&path).await },
            move |this, result, cx| {
                if this.next_generation != request {
                    return;
                }
                match result {
                    Ok(document) => {
                        let generation = this.bump_generation();
                        this.open = Some(Open {
                            document,
                            editor: None,
                            conflict: false,
                            generation,
                        });
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let content = open.document.content.clone();
        let editor = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(10, 36)
                .default_value(content)
        });
        editor.update(cx, |s, cx| s.focus(window, cx));
        open.editor = Some(editor);
        open.conflict = false;
        self.next_generation += 1;
        if let Some(open) = self.open.as_mut() {
            open.generation = self.next_generation;
        }
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.new_path.read(cx).value().trim().to_string();
        if path.is_empty() {
            return;
        }
        self.new_path
            .update(cx, |s, cx| s.set_value("", window, cx));
        let editor = cx.new(|cx| TextareaState::new(window, cx).auto_grow(10, 36));
        editor.update(cx, |s, cx| s.focus(window, cx));
        let generation = self.bump_generation();
        self.open = Some(Open {
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
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let Some(editor) = open.editor.as_ref() else {
            return;
        };
        let content = editor.read(cx).value().to_string();
        let ticket = open.ticket();
        let path = ticket.path.clone();
        // A new document carries no hash, so the save cannot overwrite one.
        let expected =
            (!open.document.content_hash.is_empty()).then(|| open.document.content_hash.clone());
        self.busy = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.memory_write(&path, &content, expected.as_deref()).await },
            move |this, result, cx| {
                this.busy = false;
                let reply = match result {
                    Ok(response) => match (response.outcome, response.document) {
                        (MemoryPageWriteOutcome::Written, Some(document)) => {
                            SaveReply::Written(document)
                        }
                        (MemoryPageWriteOutcome::Conflict, current) => SaveReply::Conflict(current),
                        (MemoryPageWriteOutcome::Written, None) => {
                            SaveReply::WrittenWithoutReadBack
                        }
                    },
                    Err(e) => {
                        this.error = Some(e.to_string().into());
                        cx.notify();
                        return;
                    }
                };
                let saved = matches!(reply, SaveReply::Written(_));
                // Only the draft that sent this save is touched.
                match apply_save(&mut this.open, &ticket, reply, &mut this.next_generation) {
                    Effect::Notice(text) => {
                        this.notice = Some(text.into());
                        this.error = None;
                    }
                    Effect::Error(text) => this.error = Some(text.into()),
                }
                if saved {
                    this.load_tree(cx);
                }
                cx.notify();
            },
        );
    }

    fn archive(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let ticket = open.ticket();
        let (path, hash) = (ticket.path.clone(), open.document.content_hash.clone());
        self.confirm_archive = false;
        self.busy = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.memory_archive(&path, &hash).await },
            move |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(response) if response.outcome == MemoryArchiveOutcome::Archived => {
                        this.notice = Some(
                            format!(
                                "Removed. Kept at {}.",
                                response.archived_path.unwrap_or_default()
                            )
                            .into(),
                        );
                        apply_archived(&mut this.open, &ticket);
                        this.load_tree(cx);
                    }
                    Ok(_) => {
                        this.error =
                            Some("It changed since you opened it; reopen and try again.".into());
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.hits = None;
            cx.notify();
            return;
        }
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.memory_search(&query, 20).await },
            |this, result, cx| {
                match result {
                    Ok(found) => this.hits = Some(found.hits),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    // --- Supermemory ---

    fn load_supermemory(&mut self, cx: &mut Context<Self>) {
        let profile = self.profile.clone();
        let container = self.container.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                // silent-ok: without the orchestrator there are no profiles, only the default.
                let profiles = c
                    .profiles()
                    .await
                    .map(|list| {
                        list.profiles
                            .into_iter()
                            .map(|p| (p.profile_id, p.settings.display_name))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let status = c.supermemory_status(profile.as_deref()).await?;
                let page = if status.active {
                    Some(
                        c.supermemory_documents(profile.as_deref(), container.as_deref(), 1)
                            .await?,
                    )
                } else {
                    None
                };
                Ok((profiles, status, page))
            },
            |this, result, cx| {
                match result {
                    Ok((profiles, status, page)) => {
                        this.profiles = profiles;
                        this.status = Some(status);
                        this.page = page;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn load_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let (profile, container) = (self.profile.clone(), self.container.clone());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                c.supermemory_documents(profile.as_deref(), container.as_deref(), page)
                    .await
            },
            |this, result, cx| {
                match result {
                    Ok(page) => this.page = Some(page),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn run_sm_search(&mut self, cx: &mut Context<Self>) {
        let query = self.sm_search.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.sm_hits = None;
            cx.notify();
            return;
        }
        let (profile, container) = (self.profile.clone(), self.container.clone());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                c.supermemory_search(profile.as_deref(), &query, container.as_deref())
                    .await
            },
            |this, result, cx| {
                match result {
                    Ok(found) => this.sm_hits = Some(found.hits),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn delete_remote_document(&mut self, id: String, cx: &mut Context<Self>) {
        self.confirm_remote = None;
        self.busy = true;
        let profile = self.profile.clone();
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.supermemory_delete_document(profile.as_deref(), &id).await },
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(evidence) => {
                        this.notice = Some(if evidence.verified {
                            "Deleted from Supermemory.".into()
                        } else {
                            "Supermemory accepted the delete; it may take a moment to disappear."
                                .into()
                        });
                        let page = this.page.as_ref().map_or(1, |p| p.page);
                        this.load_page(page, cx);
                    }
                    Err(e) => this.fail(e, cx),
                }
                cx.notify();
            },
        );
    }

    fn forget_remote_memory(&mut self, container: String, id: String, cx: &mut Context<Self>) {
        self.confirm_remote = None;
        self.busy = true;
        let profile = self.profile.clone();
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                c.supermemory_forget(profile.as_deref(), &container, &id)
                    .await
            },
            |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(evidence) if evidence.forgotten => {
                        this.notice = Some("Supermemory will no longer recall that.".into());
                        if let Some(hits) = this.sm_hits.as_mut() {
                            hits.retain(|hit| hit.id != evidence.id);
                        }
                    }
                    Ok(_) => this.error = Some("Supermemory did not confirm the forget.".into()),
                    Err(e) => this.fail(e, cx),
                }
                cx.notify();
            },
        );
    }

    // --- rendering ---

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: &'static str, which: Tab| {
            let button = Button::new(id).label(label).small();
            let button = if self.tab == which {
                button.primary()
            } else {
                button.ghost()
            };
            button.on_click(cx.listener(move |this, _, _, cx| {
                this.tab = which;
                this.error = None;
                this.notice = None;
                this.refresh(cx);
            }))
        };
        h_flex()
            .gap_2()
            .pb_3()
            .child(tab("memory-tab-local", "Local (on this Mac)", Tab::Local))
            .child(tab(
                "memory-tab-supermemory",
                "Supermemory (remote)",
                Tab::Supermemory,
            ))
    }

    fn render_local(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.loaded && !self.available {
            return empty_state(
                "The bound memory provider has no documents to show (memory is off or \
                 served by a third-party provider).",
                cx,
            )
            .into_any_element();
        }
        let selected = self.open.as_ref().map(|o| o.document.path.clone());
        let list = v_flex().children(self.documents.iter().enumerate().map(|(ix, entry)| {
            let path = entry.path.clone();
            let active = selected.as_deref() == Some(entry.path.as_str());
            h_flex()
                .id(("memory-doc", ix))
                .gap_3()
                .py_1p5()
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .when(active, |d| d.bg(theme.secondary))
                .hover(|d| d.bg(theme.secondary.opacity(0.6)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .truncate()
                        .child(entry.path.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(kind_label(entry.kind)),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_document(path.clone(), cx)))
                .into_any_element()
        }));
        let hits = self.hits.as_ref().map(|hits| {
            if hits.is_empty() {
                return empty_state("Nothing matches.", cx).into_any_element();
            }
            v_flex()
                .pb_4()
                .children(hits.iter().enumerate().map(|(ix, hit)| {
                    let path = hit.path.clone();
                    v_flex()
                        .id(("memory-hit", ix))
                        .py_1p5()
                        .cursor_pointer()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().text_sm().font_medium().child(hit.path.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(hit.snippet.chars().take(240).collect::<String>()),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_document(path.clone(), cx)),
                        )
                }))
                .into_any_element()
        });
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.search)))
                    .child(
                        Button::new("memory-search")
                            .icon(IconName::Search)
                            .outline()
                            .small()
                            .tooltip("Search")
                            .accessibility_label("Search local memory")
                            .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
                    ),
            )
            .when_some(hits, |d, hits| d.child(hits))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.new_path)))
                    .child(
                        Button::new("memory-new")
                            .icon(IconName::Plus)
                            .label("New")
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
            .when(self.loaded && self.documents.is_empty(), |d| {
                d.child(empty_state("No memory documents yet.", cx))
            })
            .child(list)
            .when_some(self.render_open(cx), |d, open| d.child(open))
            .into_any_element()
    }

    fn render_open(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let open = self.open.as_ref()?;
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
                if doc.provenance.version_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
            if let Some(by) = &doc.provenance.last_changed_by {
                line.push_str(&format!(" · last saved by {by}"));
            }
            line.push_str(" · which thread wrote it is not recorded");
            line
        };
        let actions = match (&open.editor, archived) {
            (Some(_), _) => h_flex()
                .gap_2()
                .child(
                    Button::new("memory-cancel")
                        .label("Cancel")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(open) = this.open.as_mut() {
                                open.editor = None;
                                open.conflict = false;
                            }
                            if this
                                .open
                                .as_ref()
                                .is_some_and(|o| o.document.content_hash.is_empty())
                            {
                                this.open = None;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("memory-save")
                        .label(if open.conflict { "Save anyway" } else { "Save" })
                        .primary()
                        .small()
                        .loading(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                ),
            (None, true) => h_flex(),
            (None, false) => h_flex()
                .gap_2()
                .child(
                    Button::new("memory-edit")
                        .label("Edit")
                        .outline()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.start_edit(window, cx))),
                )
                .child(if self.confirm_archive {
                    Button::new("memory-archive-confirm")
                        .label("Remove (keeps a copy in archive/)")
                        .danger()
                        .small()
                        .loading(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.archive(cx)))
                } else {
                    Button::new("memory-archive")
                        .icon(IconName::Delete)
                        .ghost()
                        .small()
                        .tooltip("Remove")
                        .accessibility_label("Remove document")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_archive = true;
                            cx.notify();
                        }))
                }),
        };
        let body = match &open.editor {
            Some(editor) => Textarea::new(editor)
                .font_family(theme.mono_font_family.clone())
                .into_any_element(),
            None => markdown(
                SharedString::from(format!("memory:{}", doc.path)),
                doc.content.clone(),
                false,
            )
            .into_any_element(),
        };
        Some(
            v_flex()
                .gap_2()
                .mt_4()
                .p_4()
                .rounded(px(12.))
                .bg(super::block_fill(cx))
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().font_semibold().child(doc.path.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .whitespace_normal()
                                        .child(provenance),
                                ),
                        )
                        .child(actions),
                )
                .when(open.conflict, |d| {
                    d.child(
                        div()
                            .text_sm()
                            .text_color(theme.danger)
                            .whitespace_normal()
                            .child(
                                "This document changed since you opened it (the agent may have \
                                 written to it). Your text is kept; Save anyway replaces the \
                                 newer version, which stays in the history.",
                            ),
                    )
                })
                .child(body)
                .into_any_element(),
        )
    }

    fn render_supermemory(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(status) = self.status.as_ref() else {
            return Spinner::new().into_any_element();
        };
        if !status.available {
            return empty_state("Supermemory is not part of this build.", cx).into_any_element();
        }
        let profile_picker = h_flex().gap_2().flex_wrap().children(
            std::iter::once((None, "Default".to_string()))
                .chain(
                    self.profiles
                        .iter()
                        .map(|(id, name)| (Some(id.clone()), name.clone())),
                )
                .enumerate()
                .map(|(ix, (id, name))| {
                    let chosen = self.profile == id;
                    let button = Button::new(("sm-profile", ix)).label(name).small();
                    let button = if chosen {
                        button.primary()
                    } else {
                        button.ghost()
                    };
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        this.profile = id.clone();
                        this.container = None;
                        this.sm_hits = None;
                        this.load_supermemory(cx);
                    }))
                }),
        );
        let binding_line = match (&status.binding, status.key_configured) {
            (_, false) => "No Supermemory API key is stored, so local memory is used. Add the \
                           key in Settings to turn Supermemory on."
                .to_string(),
            (None, true) => {
                "This profile has no Supermemory container; local memory is used.".to_string()
            }
            (Some(binding), true) => format!(
                "Container {} ({}){}. Recall {}, capture {}.",
                binding.primary_container,
                match binding.origin {
                    SupermemoryBindingOrigin::Profile => "from this profile",
                    SupermemoryBindingOrigin::Default => "deployment default",
                },
                if binding.read_containers.is_empty() {
                    String::new()
                } else {
                    format!("; also reads {}", binding.read_containers.join(", "))
                },
                if status.auto_recall { "on" } else { "off" },
                if status.auto_capture { "on" } else { "off" },
            ),
        };
        let primary = status.binding.as_ref().map(|b| b.primary_container.clone());
        let containers: Vec<String> = status
            .binding
            .as_ref()
            .map(|b| {
                std::iter::once(b.primary_container.clone())
                    .chain(b.read_containers.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let current = self.container.clone().or(primary.clone());
        let container_picker = h_flex()
            .gap_2()
            .children(containers.into_iter().enumerate().map(|(ix, tag)| {
                let chosen = current.as_deref() == Some(tag.as_str());
                let button = Button::new(("sm-container", ix)).label(tag.clone()).small();
                let button = if chosen {
                    button.outline()
                } else {
                    button.ghost()
                };
                button.on_click(cx.listener(move |this, _, _, cx| {
                    this.container = Some(tag.clone());
                    this.load_page(1, cx);
                }))
            }));
        let writable = current.is_some() && current == primary;
        let hits = self.sm_hits.as_ref().map(|hits| {
            if hits.is_empty() {
                return empty_state("Nothing matches.", cx).into_any_element();
            }
            v_flex()
                .children(hits.iter().enumerate().map(|(ix, hit)| {
                    let key = format!("memory:{}", hit.id);
                    let confirming = self.confirm_remote.as_deref() == Some(key.as_str());
                    let (container, id) = (hit.container.clone(), hit.id.clone());
                    let can_forget = primary.as_deref() == Some(hit.container.as_str());
                    h_flex()
                        .gap_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .whitespace_normal()
                                        .child(hit.content.clone()),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    format!(
                                            "{}{}{}",
                                            hit.container,
                                            hit.similarity
                                                .map(|s| format!(" · {:.0}%", s * 100.))
                                                .unwrap_or_default(),
                                            hit.updated_at
                                                .as_deref()
                                                .map(|t| format!(" · {t}"))
                                                .unwrap_or_default()
                                        ),
                                )),
                        )
                        .when(can_forget, |d| {
                            d.child(if confirming {
                                Button::new(("sm-forget-confirm", ix))
                                    .label("Forget")
                                    .danger()
                                    .small()
                                    .loading(self.busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.forget_remote_memory(container.clone(), id.clone(), cx)
                                    }))
                            } else {
                                Button::new(("sm-forget", ix))
                                    .icon(IconName::Delete)
                                    .ghost()
                                    .small()
                                    .tooltip("Forget this memory")
                                    .accessibility_label("Forget this memory")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.confirm_remote = Some(key.clone());
                                        cx.notify();
                                    }))
                            })
                        })
                }))
                .into_any_element()
        });
        let documents = self.page.as_ref().map(|page| {
            let rows = page.documents.iter().enumerate().map(|(ix, document)| {
                let key = format!("document:{}", document.id);
                let confirming = self.confirm_remote.as_deref() == Some(key.as_str());
                let id = document.id.clone();
                h_flex()
                    .gap_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div().text_sm().font_medium().truncate().child(
                                    document
                                        .title
                                        .clone()
                                        .or(document.custom_id.clone())
                                        .unwrap_or_else(|| document.id.clone()),
                                ),
                            )
                            .when_some(document.preview.clone(), |d, preview| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .whitespace_normal()
                                        .child(preview),
                                )
                            })
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                format!(
                                    "{}{}",
                                    document.status.clone().unwrap_or_default(),
                                    document
                                        .updated_at
                                        .as_deref()
                                        .map(|t| format!(" · {t}"))
                                        .unwrap_or_default()
                                ),
                            )),
                    )
                    .when(writable, |d| {
                        d.child(if confirming {
                            Button::new(("sm-delete-confirm", ix))
                                .label("Delete from Supermemory")
                                .danger()
                                .small()
                                .loading(self.busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_remote_document(id.clone(), cx)
                                }))
                        } else {
                            Button::new(("sm-delete", ix))
                                .icon(IconName::Delete)
                                .ghost()
                                .small()
                                .tooltip("Delete document")
                                .accessibility_label("Delete document")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.confirm_remote = Some(key.clone());
                                    cx.notify();
                                }))
                        })
                    })
            });
            let total = page.total_pages.unwrap_or(page.page);
            let current_page = page.page;
            v_flex()
                .child(
                    div()
                        .pt_4()
                        .pb_1()
                        .font_semibold()
                        .child(format!("Captured documents in {}", page.container)),
                )
                .when(page.documents.is_empty(), |d| {
                    d.child(empty_state("Nothing captured here yet.", cx))
                })
                .children(rows)
                .child(
                    h_flex()
                        .gap_2()
                        .pt_2()
                        .justify_end()
                        .child(
                            Button::new("sm-prev")
                                .icon(IconName::ChevronLeft)
                                .ghost()
                                .small()
                                .disabled(current_page <= 1)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.load_page(current_page.saturating_sub(1).max(1), cx)
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{current_page} / {total}")),
                        )
                        .child(
                            Button::new("sm-next")
                                .icon(IconName::ChevronRight)
                                .ghost()
                                .small()
                                .disabled(current_page >= total)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.load_page(current_page + 1, cx)
                                })),
                        ),
                )
                .into_any_element()
        });
        v_flex()
            .gap_3()
            .child(profile_picker)
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(binding_line),
            )
            .when(status.active, |d| {
                d.child(container_picker)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.sm_search)))
                            .child(
                                Button::new("sm-search")
                                    .icon(IconName::Search)
                                    .outline()
                                    .small()
                                    .tooltip("Search")
                                    .accessibility_label("Search Supermemory")
                                    .on_click(cx.listener(|this, _, _, cx| this.run_sm_search(cx))),
                            ),
                    )
                    .when_some(hits, |d, hits| d.child(hits))
                    .when_some(documents, |d, docs| d.child(docs))
            })
            .into_any_element()
    }
}

impl Render for MemoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let actions = refresh_button("refresh-memory")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let content = match self.tab {
            Tab::Local => self.render_local(cx),
            Tab::Supermemory => self.render_supermemory(cx),
        };
        let body = v_flex()
            .child(header("Memory", actions))
            .child(self.render_tabs(cx))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.clone(), |d, n| {
                d.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(n),
                )
            })
            .child(content);
        page_body("memory-page", body)
    }
}
