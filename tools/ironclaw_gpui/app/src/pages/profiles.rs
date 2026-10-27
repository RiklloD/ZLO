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

use super::{
    PageEvent, block_fill, empty_state, error_line, group_header, header, page_body,
    refresh_button, rows_card, setting_row,
};
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
    /// The detail view is showing (`selected` is `None` while creating).
    open: bool,
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
            open: false,
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
        self.open = true;
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
            move |this, result, _window, cx| {
                match result {
                    Ok(()) => {
                        this.items.retain(|p| p.profile_id != id);
                        this.back(cx);
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

    /// Leave the detail view for the list.
    fn back(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.selected = None;
        self.confirm_delete = false;
        self.menu = None;
        self.notice = None;
        cx.notify();
    }

    /// One list row: initials, name, what it runs and when it last ran.
    fn render_row(&self, ix: usize, p: &ProfileInfo, cx: &Context<Self>) -> AnyElement {
        let palette = *Palette::of(cx);
        let id = p.profile_id.clone();
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
        let group = SharedString::from(format!("profile-row-{ix}"));
        h_flex()
            .id(("profile-row", ix))
            .group(group.clone())
            .role(Role::Button)
            .aria_label(p.settings.display_name.clone())
            .w_full()
            .min_h(px(56.))
            .px(px(16.))
            .py(px(10.))
            .gap_3()
            .items_center()
            .cursor_pointer()
            .hover(|s| s.bg(design::kit::hover_wash(&palette)))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, window, cx| this.select(Some(id.clone()), window, cx)
            }))
            .child(avatar(&id, &p.settings.display_name))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .font_medium()
                                    .child(p.settings.display_name.clone()),
                            )
                            .when(p.is_default, |d| {
                                d.child(Tag::secondary().small().child("Default"))
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(palette.text_muted)
                            .child(meta.join(" · ")),
                    ),
            )
            // Shown on hover: start a thread without opening the profile.
            .child(
                div().invisible().group_hover(group, |s| s.visible()).child(
                    Button::new(("profile-thread", ix))
                        .icon(IconName::Plus)
                        .label("New thread")
                        .ghost()
                        .xsmall()
                        .accessibility_label("New thread on this profile")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.new_thread(id.clone(), cx)
                        })),
                ),
            )
            .child(
                Icon::new(IconName::ChevronRight)
                    .size(px(14.))
                    .text_color(palette.text_faint),
            )
            .into_any_element()
    }

    /// The list: profiles that ran, then the ones that never did.
    fn render_list(&self, cx: &Context<Self>) -> gpui_kit::Div {
        let (used, unused): (Vec<_>, Vec<_>) = self
            .items
            .iter()
            .enumerate()
            .partition(|(_, p)| p.last_used_at.is_some());
        let mut list = v_flex();
        for (title, group) in [("Recently used", used), ("Not used yet", unused)] {
            if group.is_empty() {
                continue;
            }
            let rows: Vec<AnyElement> = group
                .into_iter()
                .map(|(ix, p)| self.render_row(ix, p, cx))
                .collect();
            list = list
                .child(group_header(title, None, cx))
                .child(rows_card(rows, cx));
        }
        list
    }

    /// The chosen folder (or the default) with Choose and Default buttons.
    fn folder_row(&self, cx: &Context<Self>) -> AnyElement {
        let shown: SharedString = match (&self.picks.folder, self.selected_profile()) {
            (Some(folder), _) => folder.clone().into(),
            (None, Some(profile)) => format!("Default: {}", profile.effective_folder).into(),
            (None, None) => "Default: ~/.ironclaw/profiles/<id>".into(),
        };
        let control = h_flex()
            .gap_1()
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
            .child(
                Button::new("profile-folder-pick")
                    .icon(IconName::FolderOpen)
                    .label("Choose…")
                    .outline()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx))),
            );
        setting_row("Working folder", Some(shown), control, cx)
    }

    /// A picker control sized for the right side of a setting row.
    fn picker(
        &self,
        menu: Menu,
        summary: String,
        popover: impl FnOnce() -> AnyElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .w(px(CONTROL_WIDTH))
            .child(self.trigger(menu, summary, popover, cx))
    }

    fn render_editor(&self, cx: &Context<Self>) -> gpui_kit::Div {
        let e = &self.editor;
        let profile = self.selected_profile();
        let creating = self.selected.is_none();
        let text =
            |state: &Entity<TextareaState>| div().w(px(CONTROL_WIDTH)).child(Textarea::new(state));

        let mut general = Vec::new();
        if creating {
            general.push(setting_row(
                "Id",
                Some("Lowercase, used as the folder name. It cannot change later.".into()),
                text(&e.id),
                cx,
            ));
        }
        general.push(setting_row("Display name", None, text(&e.name), cx));

        let model = [
            setting_row(
                "Default model",
                Some("New threads on this profile start here.".into()),
                self.picker(
                    Menu::Model,
                    self.model_summary(),
                    || self.model_popover(cx),
                    cx,
                ),
                cx,
            ),
            setting_row(
                "Approvals",
                Some("Whether tool calls run on their own or wait for you.".into()),
                self.picker(
                    Menu::Approval,
                    self.approval_summary(),
                    || self.approval_popover(cx),
                    cx,
                ),
                cx,
            ),
        ];
        let workspace = [
            self.folder_row(cx),
            setting_row(
                "Supermemory container",
                Some("Long-term memory this profile reads and writes.".into()),
                self.picker(
                    Menu::Memory,
                    self.picks.memory.clone().unwrap_or_else(|| "None".into()),
                    || self.memory_popover(cx),
                    cx,
                ),
                cx,
            ),
        ];
        let capabilities = [
            setting_row(
                "Tools",
                Some("Limit the tools this profile can call.".into()),
                self.picker(
                    Menu::Tools,
                    count_summary(self.picks.tools.len(), "All tools", "tool", "allowed"),
                    || self.checklist_popover(Menu::Tools, "All tools (no allowlist)", cx),
                    cx,
                ),
                cx,
            ),
            setting_row(
                "Skills",
                Some("Skills hidden from this profile.".into()),
                self.picker(
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
            ),
            setting_row(
                "MCP servers",
                Some("Servers this profile can reach.".into()),
                self.picker(
                    Menu::Mcp,
                    count_summary(self.picks.mcp.len(), "All MCP servers", "server", "enabled"),
                    || self.checklist_popover(Menu::Mcp, "All MCP servers", cx),
                    cx,
                ),
                cx,
            ),
        ];
        let soul = div()
            .p(px(12.))
            .rounded(px(12.))
            .bg(block_fill(cx))
            .child(Textarea::new(&e.soul));

        let can_delete = profile.is_some_and(|p| !p.is_default);
        let footer = h_flex()
            .pt(px(24.))
            .gap_2()
            .items_center()
            .when(can_delete, |d| {
                d.child(if self.confirm_delete {
                    Button::new("profile-delete-confirm")
                        .label("Archive this profile?")
                        .danger()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx)))
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
            .child(div().flex_1())
            .when_some(self.notice.clone(), |d, n| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(Palette::of(cx).text_muted)
                        .child(n),
                )
            })
            .child(
                Button::new("profile-cancel")
                    .label(if creating { "Cancel" } else { "Done" })
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
            )
            .child(
                Button::new("profile-save")
                    .label(if creating { "Create profile" } else { "Save" })
                    .primary()
                    .small()
                    .loading(self.saving)
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            );

        v_flex()
            .child(group_header("General", None, cx).pt(px(16.)))
            .child(rows_card(general, cx))
            .child(group_header("Model and access", None, cx))
            .child(rows_card(model, cx))
            .child(group_header("Workspace", None, cx))
            .child(rows_card(workspace, cx))
            .child(group_header(
                "Capabilities",
                Some("Skills, keys and MCP servers are shared; a profile only narrows them."),
                cx,
            ))
            .child(rows_card(capabilities, cx))
            .child(group_header(
                "SOUL.md",
                Some("Who this profile is and how it works."),
                cx,
            ))
            .child(soul)
            .child(footer)
    }

    /// The detail view's top: back link, name, and a New thread button.
    fn render_detail_header(&self, cx: &Context<Self>) -> gpui_kit::Div {
        let palette = *Palette::of(cx);
        let profile = self.selected_profile();
        let title: SharedString = profile
            .map(|p| p.settings.display_name.clone().into())
            .unwrap_or_else(|| "New profile".into());
        let back = h_flex()
            .id("profiles-back")
            .role(Role::Button)
            .aria_label("Back to profiles")
            .gap_1()
            .px(px(8.))
            .pb(px(10.))
            .items_center()
            .text_xs()
            .text_color(palette.text_muted)
            .cursor_pointer()
            .hover(|s| s.text_color(palette.text))
            .child(Icon::new(IconName::ChevronLeft).size(px(12.)))
            .child("Profiles")
            .on_click(cx.listener(|this, _, _, cx| this.back(cx)));
        let actions = h_flex().gap_1().when_some(profile, |d, p| {
            let id = p.profile_id.clone();
            d.child(
                Button::new("profile-detail-thread")
                    .icon(IconName::Plus)
                    .label("New thread")
                    .outline()
                    .small()
                    .on_click(cx.listener(move |this, _, _, cx| this.new_thread(id.clone(), cx))),
            )
        });
        v_flex().child(back).child(
            h_flex()
                .px(px(8.))
                .gap_3()
                .items_center()
                .when_some(profile, |d, p| {
                    d.child(avatar(&p.profile_id, &p.settings.display_name))
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(gpui_kit::rems(20. / 16.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .truncate()
                                .child(title),
                        )
                        .when_some(profile, |d, p| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(palette.text_muted)
                                    .child(format!(
                                        "{} · {} thread{}",
                                        p.profile_id,
                                        p.thread_count,
                                        if p.thread_count == 1 { "" } else { "s" }
                                    )),
                            )
                        }),
                )
                .child(actions),
        )
    }
}

/// Width of the controls on the right of each setting row.
const CONTROL_WIDTH: f32 = 280.;

/// A profile's initials on a tint picked from its id, so each profile keeps
/// its color across launches.
fn avatar(id: &str, name: &str) -> impl IntoElement {
    let hue = id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
        % 360;
    let tint = gpui_kit::hsla(hue as f32 / 360., 0.5, 0.6, 1.);
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    div()
        .flex_none()
        .size(px(30.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(tint.opacity(0.16))
        .text_color(tint)
        .text_xs()
        .font_semibold()
        .child(initials)
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
        let error = self.error.clone();
        let body = if self.open {
            v_flex()
                .child(self.render_detail_header(cx))
                .when_some(error, |d, e| {
                    d.child(div().pt_3().child(error_line(&e, cx)))
                })
                .child(self.render_editor(cx))
        } else {
            let actions = h_flex()
                .gap_1()
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
                )
                .child(
                    Button::new("profile-new")
                        .icon(IconName::Plus)
                        .label("New profile")
                        .primary()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.select(None, window, cx))),
                );
            v_flex()
                .child(header("Profiles", actions))
                .when_some(error, |d, e| d.child(error_line(&e, cx)))
                .when_some(self.notice.clone(), |d, n| {
                    d.child(
                        div()
                            .px(px(8.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .whitespace_normal()
                            .child(n),
                    )
                })
                .when(self.loaded && self.items.is_empty(), |d| {
                    d.child(empty_state("No profiles yet.", cx))
                })
                .child(self.render_list(cx))
        };
        page_body("profiles-page", body)
    }
}
