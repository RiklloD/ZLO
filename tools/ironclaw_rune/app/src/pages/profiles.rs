//! Profiles: list (most recently used first), edit, create and archive
//! profiles, start a thread on one, and run the one-time Hermes import. Each
//! profile has its own SOUL.md, default model, tool allowlist, folder and
//! Supermemory container; skills, keys and MCP servers stay shared, and a
//! profile only hides or enables some of them. Every field but the id, name and
//! SOUL.md picks from what IronClaw has (`pickers.rs`).
//!
//! Other areas call [`new_profile`] (the inbox's New profile button).

mod fixtures;
#[cfg(test)]
mod motion_tests;
mod pickers;
#[cfg(test)]
mod refresh_tests;
mod view;

pub use fixtures::{fixture_editor, fixture_list, fixture_pickers};

use crate::design::kit::hover_wash;
use crate::pages::common::controls::{
    Look, header_with, keyed_refresh, keyed_setting_row, press_button, styled,
};
use crate::pages::common::ts::{sm, sm_medium, title as page_title, xs, xs_semibold};
use crate::pages::common::{
    block_fill, empty_state, error_line, glyph, group_header, page_body, rows_card,
};
use rune::{Color, Cursor, Cx, El, TextEdit, col, el, row};
use rune_kit::{Badge, Kit, TextArea, TextInput};
use zlo_client::orchestration::PickerPrefs;
use zlo_client::profiles::{HermesImportResponse, ProfileInfo, ProfileSettings};

use self::pickers::{Catalog, Menu, Picks};
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
    deleting: bool,
    mutation_gen: jobs::Generation,
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
        self.id.replace_text(
            profile
                .as_ref()
                .map(|p| p.profile_id.clone())
                .unwrap_or_default(),
        );
        self.name
            .replace_text(s.map(|s| s.display_name.clone()).unwrap_or_default());
        self.soul
            .replace_text(s.map(|s| s.soul.clone()).unwrap_or_default());
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
                            serde_json::from_value::<zlo_client::wire::ToolPermission>(e.value).ok()
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
                    let mut skills: Vec<(String, String)> = list
                        .skills
                        .into_iter()
                        .map(|s| (s.name, s.description))
                        .collect();
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

/// Every profile id Settings knows (archived ones it lists included).
pub fn known_ids(s: &State) -> Vec<String> {
    s.pages
        .profiles
        .items
        .iter()
        .map(|p| p.profile_id.clone())
        .collect()
}

/// A profile made outside the editor (the pickers' "Add profile") joins
/// Settings' list.
pub fn remember_saved(s: &mut State, saved: &ProfileInfo) {
    let ps = &mut s.pages.profiles;
    if !ps.items.iter().any(|p| p.profile_id == saved.profile_id) {
        ps.items.insert(0, saved.clone());
    }
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
    if ps.saving || ps.deleting {
        return;
    }
    let settings = ps.settings(ps.selected_profile().map(|p| &p.settings));
    let existing = ps.selected.clone();
    let id = existing
        .clone()
        .unwrap_or_else(|| ps.id.text.trim().to_string());
    if id.is_empty() || settings.display_name.is_empty() {
        ps.error = Some("A profile needs an id and a display name.".into());
        return;
    }
    ps.saving = true;
    let generation = ps.mutation_gen.bump();
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
        move |s, result, tx| {
            // The reply to the person's own action: let it move.
            tx.animated();

            let ps = &mut s.pages.profiles;
            ps.saving = false;
            if ps.mutation_gen != generation {
                return;
            }
            match result {
                Ok(saved) => {
                    crate::workspace::profiles_changed(
                        s,
                        crate::workspace::ProfileChange::Saved(&saved),
                    );
                    let ps = &mut s.pages.profiles;
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

/// Every approval pill reloads.
fn approval_mode_changed(s: &mut State) {
    crate::chat::approval_mode_changed(s);
}

fn delete(s: &mut State) {
    let ps = &mut s.pages.profiles;
    if ps.saving || ps.deleting {
        return;
    }
    let Some(id) = ps.selected.clone() else {
        return;
    };
    ps.confirm_delete = false;
    ps.deleting = true;
    let generation = ps.mutation_gen.bump();
    let job_id = id.clone();
    jobs::run(
        s,
        "profiles",
        move |c| async move { c.delete_profile(&job_id).await },
        move |s, result, tx| {
            // The reply to the person's own action: let it move.
            tx.animated();
            s.pages.profiles.deleting = false;
            if s.pages.profiles.mutation_gen != generation {
                return;
            }
            match result {
                Ok(()) => {
                    crate::workspace::profiles_changed(
                        s,
                        crate::workspace::ProfileChange::Deleted(&id),
                    );
                    s.pages.profiles.items.retain(|p| p.profile_id != id);
                    back(s);
                }
                Err(e) => s.pages.profiles.error = Some(e.to_string()),
            }
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
        |s, result, tx| {
            // The reply to the person's own action: let it move.
            tx.animated();

            let ps = &mut s.pages.profiles;
            ps.importing = false;
            match result {
                Ok(report) => {
                    ps.notice = Some(view::import_summary(&report));
                    ps.error = None;
                    refresh(s);
                    crate::workspace::profiles_changed(
                        s,
                        crate::workspace::ProfileChange::Reloaded,
                    );
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

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    view::page(s, k, layer)
}
