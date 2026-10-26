//! Profiles: list (most recently used first), edit, create and archive
//! profiles, start a thread on one, and run the one-time Hermes import.
//! Each profile has its own SOUL.md, default model, tool allowlist, folder
//! and Supermemory container; skills, keys and MCP servers stay shared, and
//! a profile only hides or enables some of them. Every field but the id,
//! name and SOUL.md picks from what IronClaw has (`pickers.rs`).

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::orchestration::PickerPrefs;
use ironclaw_gpui_client::profiles::{HermesImportResponse, ProfileInfo, ProfileSettings};

mod pickers;

use pickers::{Catalog, Menu, Picks};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::design::{self, Palette};
use crate::jobs;
use crate::ui::*;

/// The free-text fields; every other field is a picker (`pickers.rs`).
struct Editor {
    id: Entity<TextareaState>,
    name: Entity<TextareaState>,
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
            soul: input("SOUL.md: who this profile is and how it works", 24),
        }
    }

    fn text(state: &Entity<TextareaState>, cx: &App) -> String {
        state.read(cx).value().trim().to_string()
    }

    /// The settings to save. `base` keeps what this form does not edit.
    fn settings(&self, picks: &Picks, base: Option<&ProfileSettings>, cx: &App) -> ProfileSettings {
        let mut settings = ProfileSettings {
            display_name: Self::text(&self.name, cx),
            soul: self.soul.read(cx).value().to_string(),
            ..ProfileSettings::default()
        };
        picks.apply(&mut settings, base);
        settings
    }

    fn fill(&self, profile: Option<&ProfileInfo>, window: &mut Window, cx: &mut App) {
        let s = profile.map(|p| &p.settings);
        let values = [
            (
                &self.id,
                profile.map(|p| p.profile_id.clone()).unwrap_or_default(),
            ),
            (
                &self.name,
                s.map(|s| s.display_name.clone()).unwrap_or_default(),
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
    picks: Picks,
    /// The picks the form opened with, to tell edited fields apart.
    origin: Picks,
    catalog: Catalog,
    picker_prefs: PickerPrefs,
    /// The open picker menu.
    menu: Option<Menu>,
    /// The provider whose models the model menu lists.
    rail: Option<String>,
    search: Entity<InputState>,
    _search: Subscription,
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
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let _search = cx.subscribe(&search, |_, _, event, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        let mut this = Self {
            backend,
            items: Vec::new(),
            last_used: None,
            selected: None,
            editor,
            picks: Picks::default(),
            origin: Picks::default(),
            catalog: Catalog::default(),
            picker_prefs: PickerPrefs::default(),
            menu: None,
            rail: None,
            search,
            _search,
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
        self.load_catalog(cx);
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

    /// What the pickers offer: signed-in providers, tools, skills, MCP servers.
    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.providers().await },
            |this, result, cx| {
                match result {
                    Ok(catalog) => {
                        let prefs = catalog.picker;
                        this.catalog.providers = catalog
                            .providers
                            .into_iter()
                            .filter(|p| p.authenticated && !prefs.provider_hidden(&p.provider))
                            .collect();
                        this.picker_prefs = prefs;
                    }
                    Err(e) => this.error = Some(format!("Providers: {e}").into()),
                }
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.tool_settings().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        let mut tools: Vec<(String, String)> = list
                            .entries
                            .into_iter()
                            .filter(|e| e.key.starts_with("tool."))
                            .filter_map(|e| {
                                // silent-ok: a row the gateway shapes differently is skipped.
                                serde_json::from_value::<
                                        ironclaw_gpui_client::wire::ToolPermission,
                                    >(e.value)
                                    .ok()
                            })
                            .map(|t| (t.name, t.description))
                            .collect();
                        tools.sort();
                        this.catalog.tools = tools;
                    }
                    Err(e) => this.error = Some(format!("Tools: {e}").into()),
                }
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.skills().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        let mut skills: Vec<(String, String)> = list
                            .skills
                            .into_iter()
                            .map(|s| (s.name, s.description))
                            .collect();
                        skills.sort();
                        this.catalog.skills = skills;
                    }
                    Err(e) => this.error = Some(format!("Skills: {e}").into()),
                }
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.extensions().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        let mut mcp: Vec<(String, String)> = list
                            .extensions
                            .into_iter()
                            .filter(|e| e.runtime == "mcp")
                            .map(|e| (e.package_ref.id.as_str().to_string(), e.display_name))
                            .collect();
                        mcp.sort_by(|a, b| a.1.cmp(&b.1));
                        this.catalog.mcp = mcp;
                    }
                    Err(e) => this.error = Some(format!("MCP servers: {e}").into()),
                }
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
        self.menu = None;
        let profile = self.selected_profile().cloned();
        self.picks = Picks::of(profile.as_ref());
        self.origin = self.picks.clone();
        self.editor.fill(profile.as_ref(), window, cx);
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let settings = self.editor.settings(
            &self.picks,
            self.selected_profile().map(|p| &p.settings),
            cx,
        );
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
        let (picks, origin) = (self.picks.clone(), self.origin.clone());
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move {
                match existing {
                    Some(id) => {
                        // Untouched security fields follow the server's
                        // current value, not the one the form opened with.
                        let mut settings = settings;
                        let list = c.profiles().await?;
                        if let Some(current) = list.profiles.iter().find(|p| p.profile_id == id) {
                            picks.keep_untouched(&origin, &mut settings, &current.settings);
                        }
                        c.update_profile(&id, &settings).await
                    }
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
                        crate::chat::approval_mode::changed(cx);
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
        // A default no signed-in provider offers runs nowhere; say so.
        let offered = |provider: &str| {
            self.catalog.providers.iter().any(|c| {
                c.provider == provider
                    && p.settings
                        .default_model
                        .as_ref()
                        .is_none_or(|m| c.models.is_empty() || c.models.contains(m))
            })
        };
        match p.settings.default_provider.as_deref() {
            Some(provider) if !self.catalog.providers.is_empty() && !offered(provider) => {
                meta.push("model unavailable".into());
            }
            _ => meta.extend(p.settings.default_model.clone()),
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
            .when(!selected, |d| {
                crate::design::kit::smooth_bg(d, format!("profile-row-{ix}"), cx)
            })
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

    /// The chosen folder (or the default) with Choose and Default buttons.
    fn folder_row(&self, cx: &Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let shown = match (&self.picks.folder, self.selected_profile()) {
            (Some(folder), _) => folder.clone(),
            (None, Some(profile)) => format!("Default: {}", profile.effective_folder),
            (None, None) => "Default: ~/.ironclaw/profiles/<id>".into(),
        };
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if self.picks.folder.is_some() {
                        p.text
                    } else {
                        p.text_muted
                    })
                    .child(shown),
            )
            .child(
                Button::new("profile-folder-pick")
                    .icon(IconName::FolderOpen)
                    .label("Choose…")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx))),
            )
            .when(self.picks.folder.is_some(), |d| {
                d.child(
                    Button::new("profile-folder-reset")
                        .label("Use default")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.picks.folder = None;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn render_editor(&self, cx: &Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, state: &Entity<TextareaState>| {
            labeled(label, Textarea::new(state).into_any_element(), cx)
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
            .child(labeled(
                "Default provider and model",
                self.trigger(
                    Menu::Model,
                    self.model_summary(),
                    || self.model_popover(cx),
                    cx,
                ),
                cx,
            ))
            .child(labeled(
                "Approvals",
                self.trigger(
                    Menu::Approval,
                    self.approval_summary(),
                    || self.approval_popover(cx),
                    cx,
                ),
                cx,
            ))
            .child(labeled("Working folder", self.folder_row(cx), cx))
            .child(labeled(
                "Supermemory container",
                self.trigger(
                    Menu::Memory,
                    self.picks.memory.clone().unwrap_or_else(|| "None".into()),
                    || self.memory_popover(cx),
                    cx,
                ),
                cx,
            ))
            .child(labeled(
                "Tool allowlist",
                self.trigger(
                    Menu::Tools,
                    count_summary(self.picks.tools.len(), "All tools", "tool", "allowed"),
                    || self.checklist_popover(Menu::Tools, "All tools (no allowlist)", cx),
                    cx,
                ),
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(labeled(
                        "Hidden skills",
                        self.trigger(
                            Menu::Skills,
                            count_summary(
                                self.picks.hidden_skills.len(),
                                "None hidden",
                                "skill",
                                "hidden",
                            ),
                            || self.checklist_popover(Menu::Skills, "No hidden skills", cx),
                            cx,
                        ),
                        cx,
                    )))
                    .child(div().flex_1().child(labeled(
                        "Enabled MCP servers",
                        self.trigger(
                            Menu::Mcp,
                            count_summary(
                                self.picks.mcp.len(),
                                "All MCP servers",
                                "server",
                                "enabled",
                            ),
                            || self.checklist_popover(Menu::Mcp, "All MCP servers", cx),
                            cx,
                        ),
                        cx,
                    ))),
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

/// A field: its label over its control.
fn labeled(label: &'static str, control: AnyElement, cx: &App) -> gpui_kit::Div {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(Palette::of(cx).text_muted)
                .child(label),
        )
        .child(control)
}

/// "All tools" for an empty set, else "3 tools allowed".
fn count_summary(n: usize, empty: &str, noun: &str, verb: &str) -> String {
    match n {
        0 => empty.to_string(),
        1 => format!("1 {noun} {verb}"),
        n => format!("{n} {noun}s {verb}"),
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
