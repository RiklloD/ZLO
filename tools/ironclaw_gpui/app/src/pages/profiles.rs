//! Profiles: list (most recently used first), edit, create and archive
//! profiles, start a thread on one, and run the one-time Hermes import.
//! Each profile has its own SOUL.md, default model, tool allowlist, folder
//! and Supermemory container; skills, keys and MCP servers stay shared.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::profiles::{
    HermesImportResponse, ProfileInfo, ProfileSettings, ProfileSupermemory,
};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::design::{self, Palette};
use crate::jobs;
use crate::ui::*;

/// One text input per editable field.
struct Editor {
    id: Entity<TextareaState>,
    name: Entity<TextareaState>,
    provider: Entity<TextareaState>,
    model: Entity<TextareaState>,
    folder: Entity<TextareaState>,
    memory: Entity<TextareaState>,
    capabilities: Entity<TextareaState>,
    hidden_skills: Entity<TextareaState>,
    mcp_servers: Entity<TextareaState>,
    soul: Entity<TextareaState>,
}

impl Editor {
    fn new(window: &mut Window, cx: &mut Context<ProfilesView>) -> Self {
        let mut input = |placeholder: &'static str, max_rows: usize| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, max_rows)
                    .placeholder(placeholder)
            })
        };
        Self {
            id: input("profile-id (lowercase, used as folder name)", 1),
            name: input("Display name", 1),
            provider: input("Default provider (optional)", 1),
            model: input("Default model (optional)", 1),
            folder: input(
                "Working folder, e.g. ~/ironclaw (default: ~/.ironclaw/profiles/<id>)",
                1,
            ),
            memory: input("Supermemory container (optional)", 1),
            capabilities: input("Allowed tools, one per line (empty: all)", 6),
            hidden_skills: input("Hidden skills, one per line (empty: none)", 6),
            mcp_servers: input("Enabled MCP servers, one per line (empty: all)", 6),
            soul: input("SOUL.md: who this profile is and how it works", 24),
        }
    }

    fn text(state: &Entity<TextareaState>, cx: &App) -> String {
        state.read(cx).value().trim().to_string()
    }

    fn optional(state: &Entity<TextareaState>, cx: &App) -> Option<String> {
        Some(Self::text(state, cx)).filter(|v| !v.is_empty())
    }

    fn lines(state: &Entity<TextareaState>, cx: &App) -> Vec<String> {
        state
            .read(cx)
            .value()
            .split(['\n', ','])
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The settings to save. `base` keeps what this form does not edit.
    fn settings(&self, base: Option<&ProfileSettings>, cx: &App) -> ProfileSettings {
        let all_or = |list: Vec<String>| Some(list).filter(|l| !l.is_empty());
        let supermemory = Self::optional(&self.memory, cx).map(|container| {
            let previous = base.and_then(|b| b.supermemory.clone());
            ProfileSupermemory {
                container,
                read_containers: previous
                    .as_ref()
                    .map(|m| m.read_containers.clone())
                    .unwrap_or_default(),
                instructions: previous.and_then(|m| m.instructions),
            }
        });
        ProfileSettings {
            display_name: Self::text(&self.name, cx),
            soul: self.soul.read(cx).value().to_string(),
            allowed_capabilities: all_or(Self::lines(&self.capabilities, cx)),
            hidden_skills: Self::lines(&self.hidden_skills, cx),
            enabled_mcp_servers: all_or(Self::lines(&self.mcp_servers, cx)),
            default_provider: Self::optional(&self.provider, cx),
            default_model: Self::optional(&self.model, cx),
            working_folder: Self::optional(&self.folder, cx),
            supermemory,
        }
    }

    fn fill(&self, profile: Option<&ProfileInfo>, window: &mut Window, cx: &mut App) {
        let s = profile.map(|p| &p.settings);
        let opt = |v: Option<&String>| v.cloned().unwrap_or_default();
        let join = |v: Option<&Vec<String>>| v.map(|l| l.join("\n")).unwrap_or_default();
        let values = [
            (
                &self.id,
                profile.map(|p| p.profile_id.clone()).unwrap_or_default(),
            ),
            (
                &self.name,
                s.map(|s| s.display_name.clone()).unwrap_or_default(),
            ),
            (
                &self.provider,
                opt(s.and_then(|s| s.default_provider.as_ref())),
            ),
            (&self.model, opt(s.and_then(|s| s.default_model.as_ref()))),
            (&self.folder, opt(s.and_then(|s| s.working_folder.as_ref()))),
            (
                &self.memory,
                opt(s.and_then(|s| s.supermemory.as_ref().map(|m| &m.container))),
            ),
            (
                &self.capabilities,
                join(s.and_then(|s| s.allowed_capabilities.as_ref())),
            ),
            (&self.hidden_skills, join(s.map(|s| &s.hidden_skills))),
            (
                &self.mcp_servers,
                join(s.and_then(|s| s.enabled_mcp_servers.as_ref())),
            ),
            (&self.soul, s.map(|s| s.soul.clone()).unwrap_or_default()),
        ];
        for (state, value) in values {
            state.update(cx, |st, cx| st.set_value(value, window, cx));
        }
    }
}

pub struct ProfilesView {
    backend: Backend,
    items: Vec<ProfileInfo>,
    last_used: Option<String>,
    /// The profile in the editor; `None` while creating a new one.
    selected: Option<String>,
    editor: Editor,
    saving: bool,
    importing: bool,
    loaded: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    confirm_delete: bool,
}

impl EventEmitter<PageEvent> for ProfilesView {}

impl ProfilesView {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = Editor::new(window, cx);
        let mut this = Self {
            backend,
            items: Vec::new(),
            last_used: None,
            selected: None,
            editor,
            saving: false,
            importing: false,
            loaded: false,
            error: None,
            notice: None,
            confirm_delete: false,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.profiles().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        this.items = list.profiles;
                        this.last_used = list.last_used_profile_id;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    /// Open the editor on a blank profile (the inbox's New profile button).
    pub fn new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select(None, window, cx);
    }

    fn selected_profile(&self) -> Option<&ProfileInfo> {
        let id = self.selected.as_deref()?;
        self.items.iter().find(|p| p.profile_id == id)
    }

    fn select(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id;
        self.confirm_delete = false;
        self.notice = None;
        let profile = self.selected_profile().cloned();
        self.editor.fill(profile.as_ref(), window, cx);
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let settings = self
            .editor
            .settings(self.selected_profile().map(|p| &p.settings), cx);
        let existing = self.selected.clone();
        let id = existing
            .clone()
            .unwrap_or_else(|| Editor::text(&self.editor.id, cx));
        if id.is_empty() || settings.display_name.is_empty() {
            self.error = Some("A profile needs an id and a display name.".into());
            cx.notify();
            return;
        }
        self.saving = true;
        cx.notify();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move {
                match existing {
                    Some(id) => c.update_profile(&id, &settings).await,
                    None => c.create_profile(&id, settings).await,
                }
            },
            |this, result, window, cx| {
                this.saving = false;
                match result {
                    Ok(saved) => {
                        let id = saved.profile_id.clone();
                        match this.items.iter_mut().find(|p| p.profile_id == id) {
                            Some(row) => *row = saved,
                            None => this.items.insert(0, saved),
                        }
                        this.error = None;
                        this.select(Some(id), window, cx);
                        this.notice = Some("Saved.".into());
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        self.confirm_delete = false;
        let job_id = id.clone();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move { c.delete_profile(&job_id).await },
            move |this, result, window, cx| {
                match result {
                    Ok(()) => {
                        this.items.retain(|p| p.profile_id != id);
                        this.select(None, window, cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn new_thread(&mut self, id: String, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.create_profile_thread(&id).await },
            |this, result, cx| match result {
                Ok(created) => cx.emit(PageEvent::OpenThread(created.thread_id)),
                Err(e) => {
                    this.error = Some(e.to_string().into());
                    cx.notify();
                }
            },
        );
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        if self.importing {
            return;
        }
        self.importing = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.import_hermes_profiles().await },
            |this, result, cx| {
                this.importing = false;
                match result {
                    Ok(report) => {
                        this.notice = Some(import_summary(&report).into());
                        this.error = None;
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_row(&self, ix: usize, p: &ProfileInfo, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = p.profile_id.clone();
        let selected = self.selected.as_deref() == Some(id.as_str());
        let last_used = self.last_used.as_deref() == Some(id.as_str());
        let mut meta = vec![format!(
            "{} thread{}",
            p.thread_count,
            if p.thread_count == 1 { "" } else { "s" }
        )];
        if let Some(model) = &p.settings.default_model {
            meta.push(model.clone());
        }
        if let Some(branch) = &p.git_branch {
            meta.push(format!("on {branch}"));
        }
        if let Some(at) = p
            .last_used_at
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            meta.push(format!("used {}", jobs::when(at)));
        }
        let palette = Palette::of(cx);
        h_flex()
            .id(("profile-row", ix))
            .role(Role::Button)
            .w_full()
            .gap_2()
            .px_2()
            .py_2()
            .rounded(px(design::CONTROL_RADIUS))
            .text_sm()
            .cursor_pointer()
            .when(selected, |d| d.bg(palette.active))
            .when(!selected, |d| crate::design::kit::smooth_bg(d, format!("profile-row-{ix}"), cx))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, window, cx| this.select(Some(id.clone()), window, cx)
            }))
            .child(Icon::new(IconName::User).small())
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .truncate()
                                    .font_semibold()
                                    .child(p.settings.display_name.clone()),
                            )
                            .when(last_used, |d| {
                                d.child(Tag::primary().small().child("Last used"))
                            })
                            .when(p.is_default, |d| {
                                d.child(Tag::secondary().small().child("Default"))
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(meta.join(" · ")),
                    ),
            )
            .child(
                Button::new(("profile-thread", ix))
                    .icon(IconName::Plus)
                    .ghost()
                    .small()
                    .tooltip("New thread on this profile")
                    .accessibility_label("New thread on this profile")
                    .on_click(cx.listener(move |this, _, _, cx| this.new_thread(id.clone(), cx))),
            )
            .into_any_element()
    }

    fn render_editor(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let field = |label: &'static str, state: &Entity<TextareaState>| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(Textarea::new(state))
        };
        let e = &self.editor;
        let profile = self.selected_profile();
        let creating = self.selected.is_none();
        let title = match profile {
            Some(p) => format!("Edit {}", p.settings.display_name),
            None => "New profile".to_string(),
        };
        let can_delete = profile.is_some_and(|p| !p.is_default);
        card(cx)
            .child(div().text_sm().font_semibold().child(title))
            .when(creating, |d| d.child(field("Id", &e.id)))
            .child(field("Display name", &e.name))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(field("Default provider", &e.provider)))
                    .child(div().flex_1().child(field("Default model", &e.model))),
            )
            .child(field("Working folder", &e.folder))
            .when_some(profile, |d, p| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Runs in {}", p.effective_folder)),
                )
            })
            .child(field("Supermemory container", &e.memory))
            .child(field("Tool allowlist", &e.capabilities))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(field("Hidden skills", &e.hidden_skills)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(field("Enabled MCP servers", &e.mcp_servers)),
                    ),
            )
            .child(field("SOUL.md", &e.soul))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(can_delete, |d| {
                        d.child(if self.confirm_delete {
                            Button::new("profile-delete-confirm")
                                .label("Archive?")
                                .danger()
                                .small()
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.delete(window, cx)),
                                )
                        } else {
                            Button::new("profile-delete")
                                .label("Archive")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_delete = true;
                                    cx.notify();
                                }))
                        })
                    })
                    .child(
                        Button::new("profile-save")
                            .label(if creating { "Create" } else { "Save" })
                            .primary()
                            .small()
                            .loading(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}

fn import_summary(report: &HermesImportResponse) -> String {
    let mut out = if report.imported.is_empty() {
        "Nothing new to import.".to_string()
    } else {
        format!(
            "Imported {}: {}.",
            report.imported.len(),
            report.imported.join(", ")
        )
    };
    let skipped: Vec<String> = report
        .skipped
        .iter()
        .filter(|s| s.reason != "excluded")
        .map(|s| format!("{} ({})", s.source, s.reason))
        .collect();
    if !skipped.is_empty() {
        out.push_str(&format!(" Skipped: {}.", skipped.join("; ")));
    }
    out
}

impl Render for ProfilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = h_flex()
            .gap_1()
            .child(
                Button::new("profile-new")
                    .label("New profile")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.select(None, window, cx))),
            )
            .child(
                Button::new("profiles-import")
                    .label("Import from Hermes")
                    .ghost()
                    .small()
                    .loading(self.importing)
                    .on_click(cx.listener(|this, _, _, cx| this.import(cx))),
            )
            .child(
                refresh_button("refresh-profiles")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let rows: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, p)| self.render_row(ix, p, cx))
            .collect();
        let body = v_flex()
            .child(header("Profiles", actions))
            .when_some(self.error.as_ref(), |this, e| this.child(error_line(e, cx)))
            .when_some(self.notice.clone(), |this, n| {
                this.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .whitespace_normal()
                        .child(n),
                )
            })
            .when(self.loaded && rows.is_empty(), |this| {
                this.child(empty_state("No profiles yet.", cx))
            })
            .child(v_flex().gap_px().pb_4().children(rows))
            .child(self.render_editor(cx));
        page_body("profiles-page", body)
    }
}
