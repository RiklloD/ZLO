//! Profiles (GPUI `pages/profiles.rs`): list (most recently used first),
//! edit, create and archive profiles, start a thread on one, and run the
//! one-time Hermes import. Each profile has its own SOUL.md, default model,
//! tool allowlist, folder and Supermemory container; skills, keys and MCP
//! servers stay shared, and a profile only hides or enables some of them.
//! Every field but the id, name and SOUL.md picks from what IronClaw has
//! (`pickers.rs`).
//!
//! Other areas call [`new_profile`] (the inbox's New profile button).

mod pickers;

use ironclaw_gpui_client::orchestration::PickerPrefs;
use ironclaw_gpui_client::profiles::{HermesImportResponse, ProfileInfo, ProfileSettings};
use rune::{Color, Cursor, Cx, El, TextEdit, col, el, row};
use rune_kit::{Badge, Kit, TextArea, TextInput};

use self::pickers::{Catalog, Menu, Picks};
use super::usage::common_stub::{
    Look, SM, SM_MEDIUM, TITLE, XS, XS_SEMIBOLD, block_fill, button, empty_state, error_line, glyph,
    group_header, header, hover_wash, label, page_body, refresh_button, rows_card, setting_row,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

/// Width of the controls on the right of each setting row.
const CONTROL_WIDTH: f32 = 280.;

#[derive(Default)]
pub struct ProfilesState {
    items: Vec<ProfileInfo>,
    last_used: Option<String>,
    /// The profile in the editor; `None` while creating a new one.
    selected: Option<String>,
    // The free-text fields; every other field is a picker (`pickers.rs`).
    id: TextEdit,
    name: TextEdit,
    soul: TextEdit,
    picks: Picks,
    /// The picks the form opened with, to tell edited fields apart.
    origin: Picks,
    catalog: Catalog,
    picker_prefs: PickerPrefs,
    /// The open picker menu.
    menu: Option<Menu>,
    /// The provider whose models the model menu lists.
    rail: Option<String>,
    search: TextEdit,
    saving: bool,
    importing: bool,
    loaded: bool,
    error: Option<String>,
    notice: Option<String>,
    confirm_delete: bool,
    /// The detail view is showing (`selected` is `None` while creating).
    open: bool,
}

impl ProfilesState {
    fn selected_profile(&self) -> Option<&ProfileInfo> {
        let id = self.selected.as_deref()?;
        self.items.iter().find(|p| p.profile_id == id)
    }

    /// The settings to save. `base` keeps what this form does not edit.
    fn settings(&self, base: Option<&ProfileSettings>) -> ProfileSettings {
        let mut settings = ProfileSettings {
            display_name: self.name.text.trim().to_string(),
            soul: self.soul.text.clone(),
            ..ProfileSettings::default()
        };
        self.picks.apply(&mut settings, base);
        settings
    }

    fn fill(&mut self) {
        let profile = self.selected_profile().cloned();
        let s = profile.as_ref().map(|p| &p.settings);
        self.id.replace_text(profile.as_ref().map(|p| p.profile_id.clone()).unwrap_or_default());
        self.name.replace_text(s.map(|s| s.display_name.clone()).unwrap_or_default());
        self.soul.replace_text(s.map(|s| s.soul.clone()).unwrap_or_default());
    }
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

fn refresh(s: &mut State) {
    load_catalog(s);
    jobs::run(
        s,
        "profiles",
        |c| async move { c.profiles().await },
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            match result {
                Ok(list) => {
                    ps.items = list.profiles;
                    ps.last_used = list.last_used_profile_id;
                    ps.error = None;
                }
                Err(e) => ps.error = Some(e.to_string()),
            }
            ps.loaded = true;
        },
    );
}

/// What the pickers offer: signed-in providers, tools, skills, MCP servers.
fn load_catalog(s: &mut State) {
    jobs::run(
        s,
        "profiles",
        |c| async move { c.providers().await },
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            match result {
                Ok(catalog) => {
                    let prefs = catalog.picker;
                    ps.catalog.providers = catalog
                        .providers
                        .into_iter()
                        .filter(|p| p.authenticated && !prefs.provider_hidden(&p.provider))
                        .collect();
                    ps.picker_prefs = prefs;
                }
                Err(e) => ps.error = Some(format!("Providers: {e}")),
            }
        },
    );
    jobs::run(
        s,
        "profiles",
        |c| async move { c.tool_settings().await },
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            match result {
                Ok(list) => {
                    let mut tools: Vec<(String, String)> = list
                        .entries
                        .into_iter()
                        .filter(|e| e.key.starts_with("tool."))
                        .filter_map(|e| {
                            // silent-ok: a row the gateway shapes differently is skipped.
                            serde_json::from_value::<ironclaw_gpui_client::wire::ToolPermission>(e.value).ok()
                        })
                        .map(|t| (t.name, t.description))
                        .collect();
                    tools.sort();
                    ps.catalog.tools = tools;
                }
                Err(e) => ps.error = Some(format!("Tools: {e}")),
            }
        },
    );
    jobs::run(
        s,
        "profiles",
        |c| async move { c.skills().await },
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            match result {
                Ok(list) => {
                    let mut skills: Vec<(String, String)> =
                        list.skills.into_iter().map(|s| (s.name, s.description)).collect();
                    skills.sort();
                    ps.catalog.skills = skills;
                }
                Err(e) => ps.error = Some(format!("Skills: {e}")),
            }
        },
    );
    jobs::run(
        s,
        "profiles",
        |c| async move { c.extensions().await },
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            match result {
                Ok(list) => {
                    let mut mcp: Vec<(String, String)> = list
                        .extensions
                        .into_iter()
                        .filter(|e| e.runtime == "mcp")
                        .map(|e| (e.package_ref.id.as_str().to_string(), e.display_name))
                        .collect();
                    mcp.sort_by(|a, b| a.1.cmp(&b.1));
                    ps.catalog.mcp = mcp;
                }
                Err(e) => ps.error = Some(format!("MCP servers: {e}")),
            }
        },
    );
}

/// Open the editor on a blank profile (the inbox's New profile button).
pub fn new_profile(s: &mut State) {
    select(s, None);
}

fn select(s: &mut State, id: Option<String>) {
    let ps = &mut s.pages.profiles;
    ps.selected = id;
    ps.open = true;
    ps.confirm_delete = false;
    ps.notice = None;
    ps.menu = None;
    ps.picks = Picks::of(ps.selected_profile());
    ps.origin = ps.picks.clone();
    ps.fill();
}

fn save(s: &mut State) {
    let ps = &mut s.pages.profiles;
    if ps.saving {
        return;
    }
    let settings = ps.settings(ps.selected_profile().map(|p| &p.settings));
    let existing = ps.selected.clone();
    let id = existing.clone().unwrap_or_else(|| ps.id.text.trim().to_string());
    if id.is_empty() || settings.display_name.is_empty() {
        ps.error = Some("A profile needs an id and a display name.".into());
        return;
    }
    ps.saving = true;
    let (picks, origin) = (ps.picks.clone(), ps.origin.clone());
    jobs::run(
        s,
        "profiles",
        move |c| async move {
            match existing {
                Some(id) => {
                    // Untouched security fields follow the server's current
                    // value, not the one the form opened with.
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
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            ps.saving = false;
            match result {
                Ok(saved) => {
                    let id = saved.profile_id.clone();
                    match ps.items.iter_mut().find(|p| p.profile_id == id) {
                        Some(row) => *row = saved,
                        None => ps.items.insert(0, saved),
                    }
                    ps.error = None;
                    select(s, Some(id));
                    s.pages.profiles.notice = Some("Saved.".into());
                    approval_mode_changed(s);
                }
                Err(e) => ps.error = Some(e.to_string()),
            }
        },
    );
}

/// GPUI `chat::approval_mode::changed`: every approval pill reloads.
/// PORT-STUB(area A): call crate::chat::approval_mode::changed(s).
fn approval_mode_changed(_s: &mut State) {}

fn delete(s: &mut State) {
    let ps = &mut s.pages.profiles;
    let Some(id) = ps.selected.clone() else {
        return;
    };
    ps.confirm_delete = false;
    let job_id = id.clone();
    jobs::run(
        s,
        "profiles",
        move |c| async move { c.delete_profile(&job_id).await },
        move |s, result, _tx| match result {
            Ok(()) => {
                s.pages.profiles.items.retain(|p| p.profile_id != id);
                back(s);
            }
            Err(e) => s.pages.profiles.error = Some(e.to_string()),
        },
    );
}

fn new_thread(s: &mut State, id: String) {
    jobs::run(
        s,
        "profiles",
        move |c| async move { c.create_profile_thread(&id).await },
        |s, result, _tx| match result {
            Ok(created) => crate::workspace::open_thread(s, created.thread_id),
            Err(e) => s.pages.profiles.error = Some(e.to_string()),
        },
    );
}

fn import(s: &mut State) {
    let ps = &mut s.pages.profiles;
    if ps.importing {
        return;
    }
    ps.importing = true;
    jobs::run(
        s,
        "profiles",
        |c| async move { c.import_hermes_profiles().await },
        |s, result, _tx| {
            let ps = &mut s.pages.profiles;
            ps.importing = false;
            match result {
                Ok(report) => {
                    ps.notice = Some(import_summary(&report));
                    ps.error = None;
                    refresh(s);
                }
                Err(e) => ps.error = Some(e.to_string()),
            }
        },
    );
}

/// Leave the detail view for the list.
fn back(s: &mut State) {
    let ps = &mut s.pages.profiles;
    ps.open = false;
    ps.selected = None;
    ps.confirm_delete = false;
    ps.menu = None;
    ps.notice = None;
}

// --- rendering ---

/// One list row: initials, name, what it runs and when it last ran.
fn render_row(s: &State, k: &Kit, p_info: &ProfileInfo) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let id = p_info.profile_id.clone();
    let mut meta = vec![format!(
        "{} thread{}",
        p_info.thread_count,
        if p_info.thread_count == 1 { "" } else { "s" }
    )];
    // A default no signed-in provider offers runs nowhere; say so.
    let offered = |provider: &str| {
        ps.catalog.providers.iter().any(|c| {
            c.provider == provider
                && p_info
                    .settings
                    .default_model
                    .as_ref()
                    .is_none_or(|m| c.models.is_empty() || c.models.contains(m))
        })
    };
    match p_info.settings.default_provider.as_deref() {
        Some(provider) if !ps.catalog.providers.is_empty() && !offered(provider) => {
            meta.push("model unavailable".into());
        }
        _ => meta.extend(p_info.settings.default_model.clone()),
    }
    if let Some(branch) = &p_info.git_branch {
        meta.push(format!("on {branch}"));
    }
    if let Some(at) = p_info.last_used_at.and_then(chrono::DateTime::from_timestamp_millis) {
        meta.push(format!("used {}", jobs::when(at)));
    }
    let mut name_row = row()
        .gap(8.0)
        .center_y()
        .child(label(p_info.settings.display_name.clone(), SM_MEDIUM, p.text).truncate().shrink());
    if p_info.is_default {
        name_row = name_row.child(Badge::new("Default").build(k));
    }
    let thread_id = id.clone();
    row()
        .key(format!("profile-row-{id}"))
        .role("button")
        .label(p_info.settings.display_name.clone())
        .hover_group()
        .w_full()
        .min_h(56.0)
        .px(16.0)
        .py(10.0)
        .gap(12.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover(hover_wash(p))
        .on_press(move |s: &mut State| select(s, Some(id.clone())))
        .child(avatar(&p_info.profile_id, &p_info.settings.display_name))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .child(name_row)
                .child(label(meta.join(" · "), XS, p.text_muted).truncate()),
        )
        // Shown on hover: start a thread without opening the profile.
        .child(
            el().group_opacity(0.0, 1.0).child(
                button(k, "profile-thread", "New thread", Look::Ghost, Some(&icons::PLUS), false, move |s: &mut State| {
                    new_thread(s, thread_id.clone())
                })
                .hover_alone(),
            ),
        )
        .child(glyph(&icons::CHEVRON_RIGHT, 14.0, p.text_faint))
}

/// The list: profiles that ran, then the ones that never did.
fn render_list(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let (used, unused): (Vec<_>, Vec<_>) = s.pages.profiles.items.iter().partition(|p| p.last_used_at.is_some());
    let mut list = col().key("profiles-list").w_full();
    for (key, title, group) in [("used", "Recently used", used), ("unused", "Not used yet", unused)] {
        if group.is_empty() {
            continue;
        }
        let rows: Vec<El> = group.into_iter().map(|pi| render_row(s, k, pi)).collect();
        list = list
            .child(group_header(p, title, None).key(format!("group-{key}")))
            .child(rows_card(p, rows).key(format!("card-{key}")));
    }
    list
}

/// The chosen folder (or the default) with Choose and Default buttons.
fn folder_row(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let shown = match (&ps.picks.folder, ps.selected_profile()) {
        (Some(folder), _) => folder.clone(),
        (None, Some(profile)) => format!("Default: {}", profile.effective_folder),
        (None, None) => "Default: ~/.ironclaw/profiles/<id>".into(),
    };
    let mut control = row().gap(4.0).center_y();
    if ps.picks.folder.is_some() {
        control = control.child(button(k, "profile-folder-reset", "Use default", Look::Ghost, None, false, |s: &mut State| {
            s.pages.profiles.picks.folder = None
        }));
    }
    control = control.child(
        rune_kit::Button::new("Choose…")
            .variant(rune_kit::ButtonVariant::Outline)
            .control_size(rune_kit::ControlSize::Regular)
            .icon(&icons::FOLDER_OPEN)
            .on_press_tx(pickers::pick_folder)
            .build(k)
            .key("profile-folder-pick"),
    );
    setting_row(p, "row-folder", "Working folder", Some(shown), control)
}

/// A picker control sized for the right side of a setting row.
fn picker(s: &State, k: &Kit, layer: &mut Vec<El>, menu: Menu, summary: String) -> El {
    el().w(CONTROL_WIDTH).child(pickers::trigger(s, k, layer, menu, summary))
}

/// "All tools" for an empty set, else "3 tools allowed".
fn count_summary(n: usize, empty: &str, noun: &str, verb: &str) -> String {
    match n {
        0 => empty.to_string(),
        1 => format!("1 {noun} {verb}"),
        n => format!("{n} {noun}s {verb}"),
    }
}

fn render_editor(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let profile = ps.selected_profile();
    let creating = ps.selected.is_none();
    let field = |edit: &TextEdit, placeholder: &str, key: &str, access: fn(&mut State) -> &mut TextEdit| {
        TextInput::new(edit)
            .width(Some(CONTROL_WIDTH))
            .placeholder(placeholder)
            .label(placeholder)
            .on_edit(access)
            .build(k)
            .key(key.to_string())
    };

    let mut general = Vec::new();
    if creating {
        general.push(setting_row(
            p,
            "row-id",
            "Id",
            Some("Lowercase, used as the folder name. It cannot change later.".into()),
            field(&ps.id, "profile-id (lowercase, used as folder name)", "profile-id", |s| {
                &mut s.pages.profiles.id
            }),
        ));
    }
    general.push(setting_row(
        p,
        "row-name",
        "Display name",
        None,
        field(&ps.name, "Display name", "profile-name", |s| &mut s.pages.profiles.name),
    ));

    let model = [
        setting_row(
            p,
            "row-model",
            "Default model",
            Some("New threads on this profile start here.".into()),
            picker(s, k, layer, Menu::Model, pickers::model_summary(ps)),
        ),
        setting_row(
            p,
            "row-approval",
            "Approvals",
            Some("Whether tool calls run on their own or wait for you.".into()),
            picker(s, k, layer, Menu::Approval, pickers::approval_summary(ps)),
        ),
    ];
    let workspace = [
        folder_row(s, k),
        setting_row(
            p,
            "row-memory",
            "Supermemory container",
            Some("Long-term memory this profile reads and writes.".into()),
            picker(s, k, layer, Menu::Memory, ps.picks.memory.clone().unwrap_or_else(|| "None".into())),
        ),
    ];
    let capabilities = [
        setting_row(
            p,
            "row-tools",
            "Tools",
            Some("Limit the tools this profile can call.".into()),
            picker(s, k, layer, Menu::Tools, count_summary(ps.picks.tools.len(), "All tools", "tool", "allowed")),
        ),
        setting_row(
            p,
            "row-skills",
            "Skills",
            Some("Skills hidden from this profile.".into()),
            picker(
                s,
                k,
                layer,
                Menu::Skills,
                count_summary(ps.picks.hidden_skills.len(), "None hidden", "skill", "hidden"),
            ),
        ),
        setting_row(
            p,
            "row-mcp",
            "MCP servers",
            Some("Servers this profile can reach.".into()),
            picker(s, k, layer, Menu::Mcp, count_summary(ps.picks.mcp.len(), "All MCP servers", "server", "enabled")),
        ),
    ];
    let soul = el().key("profile-soul").w_full().padding(12.0).radius(12.0).fill(block_fill(p)).child(
        TextArea::new(&ps.soul)
            .width(None)
            .auto_grow(1, 24)
            .placeholder("SOUL.md: who this profile is and how it works")
            .label("SOUL.md")
            .on_edit(|s: &mut State| &mut s.pages.profiles.soul)
            .build(k),
    );

    let can_delete = profile.is_some_and(|p| !p.is_default);
    let mut footer = row().key("profile-footer").w_full().pt(24.0).gap(8.0).center_y();
    if can_delete {
        footer = footer.child(if ps.confirm_delete {
            button(k, "profile-delete-confirm", "Archive this profile?", Look::Danger, None, false, delete)
        } else {
            button(k, "profile-delete", "Archive", Look::Ghost, None, false, |s: &mut State| {
                s.pages.profiles.confirm_delete = true
            })
        });
    }
    footer = footer.child(el().grow(1.0));
    if let Some(n) = &ps.notice {
        footer = footer.child(label(n.clone(), XS, p.text_muted));
    }
    footer = footer
        .child(button(k, "profile-cancel", if creating { "Cancel" } else { "Done" }, Look::Ghost, None, false, back))
        .child(button(
            k,
            "profile-save",
            if creating { "Create profile" } else { "Save" },
            Look::Primary,
            None,
            ps.saving,
            save,
        ));

    col()
        .key("profile-editor")
        .w_full()
        .child(group_header(p, "General", None).pt(16.0).key("g-general"))
        .child(rows_card(p, general).key("c-general"))
        .child(group_header(p, "Model and access", None).key("g-model"))
        .child(rows_card(p, model).key("c-model"))
        .child(group_header(p, "Workspace", None).key("g-workspace"))
        .child(rows_card(p, workspace).key("c-workspace"))
        .child(
            group_header(p, "Capabilities", Some("Skills, keys and MCP servers are shared; a profile only narrows them."))
                .key("g-capabilities"),
        )
        .child(rows_card(p, capabilities).key("c-capabilities"))
        .child(group_header(p, "SOUL.md", Some("Who this profile is and how it works.")).key("g-soul"))
        .child(soul)
        .child(footer)
}

/// The detail view's top: back link, name, and a New thread button.
fn render_detail_header(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let profile = s.pages.profiles.selected_profile();
    let title = profile.map(|p| p.settings.display_name.clone()).unwrap_or_else(|| "New profile".into());
    let back_link = row()
        .key("profiles-back")
        .role("button")
        .label("Back to profiles")
        .gap(4.0)
        .px(8.0)
        .pb(10.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .child(glyph(&icons::CHEVRON_LEFT, 12.0, p.text_muted))
        .child(label("Profiles", XS, p.text_muted))
        .on_press(back);
    let mut top = row().w_full().px(8.0).gap(12.0).center_y();
    if let Some(pi) = profile {
        top = top.child(avatar(&pi.profile_id, &pi.settings.display_name));
    }
    let mut titles = col().grow(1.0).min_w(0.0).child(label(title, TITLE, p.text).truncate());
    if let Some(pi) = profile {
        titles = titles.child(label(
            format!(
                "{} · {} thread{}",
                pi.profile_id,
                pi.thread_count,
                if pi.thread_count == 1 { "" } else { "s" }
            ),
            XS,
            p.text_muted,
        ));
    }
    top = top.child(titles);
    if let Some(pi) = profile {
        let id = pi.profile_id.clone();
        top = top.child(button(
            k,
            "profile-detail-thread",
            "New thread",
            Look::Outline,
            Some(&icons::PLUS),
            false,
            move |s: &mut State| new_thread(s, id.clone()),
        ));
    }
    col().key("profile-detail-header").w_full().child(back_link).child(top)
}

/// A profile's initials on a tint picked from its id, so each profile keeps
/// its color across launches.
fn avatar(id: &str, name: &str) -> El {
    let hue = id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) % 360;
    let tint = hsl(hue as f32 / 360., 0.5, 0.6);
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    row()
        .w(30.0)
        .h(30.0)
        .radius(15.0)
        .center()
        .fill(tint.alpha(0.16))
        .child(label(initials, XS_SEMIBOLD, tint))
        .a11y_hidden()
}

/// HSL (all 0..1) to an opaque colour, as GPUI's `hsla`.
fn hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1. - (2. * l - 1.).abs()) * s;
    let hp = h * 6.;
    let x = c * (1. - (hp % 2. - 1.).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    let m = l - c / 2.;
    let byte = |v: f32| ((v + m).clamp(0., 1.) * 255.).round() as u32;
    crate::design::rgb(byte(r) << 16 | byte(g) << 8 | byte(b))
}

fn import_summary(report: &HermesImportResponse) -> String {
    let mut out = if report.imported.is_empty() {
        "Nothing new to import.".to_string()
    } else {
        format!("Imported {}: {}.", report.imported.len(), report.imported.join(", "))
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

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let body = if ps.open {
        let mut b = col().w_full().child(render_detail_header(s, k));
        if let Some(e) = &ps.error {
            b = b.child(el().pt(12.0).child(error_line(p, e)));
        }
        b.child(render_editor(s, k, layer))
    } else {
        let actions = row()
            .gap(4.0)
            .center_y()
            .child(button(k, "profiles-import", "Import from Hermes", Look::Ghost, None, ps.importing, import))
            .child(refresh_button(k, "refresh-profiles", refresh))
            .child(button(k, "profile-new", "New profile", Look::Primary, Some(&icons::PLUS), false, new_profile));
        let mut b = col().w_full().child(header(p, "Profiles", actions));
        if let Some(e) = &ps.error {
            b = b.child(error_line(p, e));
        }
        if let Some(n) = &ps.notice {
            b = b.child(el().px(8.0).child(label(n.clone(), SM, p.text_muted).wrap()));
        }
        if ps.loaded && ps.items.is_empty() {
            b = b.child(empty_state(p, "No profiles yet."));
        }
        b.child(render_list(s, k))
    };
    page_body("profiles-page", body)
}

// --- fixtures (scenes.rs) ---

fn fixture_profile(id: &str, name: &str, used_hours_ago: Option<i64>, threads: u64) -> ProfileInfo {
    let now = chrono::Utc::now().timestamp_millis();
    ProfileInfo {
        profile_id: id.into(),
        settings: ProfileSettings {
            display_name: name.into(),
            soul: format!("You are {name}. Work carefully, explain briefly."),
            ..ProfileSettings::default()
        },
        is_default: id == "default",
        effective_folder: format!("~/.ironclaw/profiles/{id}"),
        git_branch: None,
        thread_count: threads,
        last_used_at: used_hours_ago.map(|h| now - h * 3_600_000),
        created_at: now - 30 * 86_400_000,
        updated_at: now - 86_400_000,
    }
}

fn fixture_catalog(ps: &mut ProfilesState) {
    let provider = |id: &str, name: &str, models: &[&str]| ironclaw_gpui_client::orchestration::RouteProvider {
        provider: id.into(),
        name: name.into(),
        runtime: id.into(),
        authenticated: true,
        models: models.iter().map(|m| m.to_string()).collect(),
        ..Default::default()
    };
    ps.catalog.providers = vec![
        provider("claude_cli", "Claude", &["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-4-5"]),
        provider("codex", "Codex", &["gpt-5.1-codex", "gpt-5.1"]),
    ];
    ps.catalog.tools = vec![
        ("shell.exec".into(), "Run a shell command".into()),
        ("files.read".into(), "Read a file".into()),
        ("files.write".into(), "Write a file".into()),
        ("web.fetch".into(), "Fetch a URL".into()),
    ];
    ps.catalog.skills = vec![("deploy".into(), "Ship to staging".into())];
    ps.catalog.mcp = vec![("github".into(), "GitHub".into()), ("linear".into(), "Linear".into())];
}

/// The list: recently used and never used profiles, an import notice.
pub fn fixture_list(s: &mut State) {
    let ps = &mut s.pages.profiles;
    fixture_catalog(ps);
    let mut work = fixture_profile("work", "Work", Some(2), 14);
    work.settings.default_provider = Some("claude_cli".into());
    work.settings.default_model = Some("claude-opus-4-5".into());
    work.git_branch = Some("rune-port".into());
    let mut research = fixture_profile("research", "Deep Research", Some(30), 3);
    research.settings.default_provider = Some("gemini".into());
    ps.items = vec![
        work,
        research,
        fixture_profile("default", "Default", Some(80), 52),
        fixture_profile("home", "Home Lab", None, 0),
    ];
    ps.last_used = Some("work".into());
    ps.loaded = true;
    ps.notice = Some("Imported 1: home. Skipped: coder (already exists).".into());
}

/// The editor on "Work" with the tools checklist open.
pub fn fixture_pickers(s: &mut State) {
    fixture_list(s);
    select(s, Some("work".into()));
    let ps = &mut s.pages.profiles;
    ps.picks.tools.insert("files.read".into());
    ps.picks.tools.insert("old.tool".into());
    ps.menu = Some(Menu::Tools);
}

/// The editor on "Work", no menu open.
pub fn fixture_editor(s: &mut State) {
    fixture_list(s);
    select(s, Some("work".into()));
}
