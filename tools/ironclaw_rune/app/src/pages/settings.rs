//! The app's own settings, one topic per sidebar page (Zeron's layout):
//! General (the desktop app), Appearance, Permissions (what tools may do
//! without asking) and Trace Commons. Models and provider logins live on
//! the Providers page; Voice is `voice::settings_view` (area F).
//! Port of GPUI `pages/settings.rs`.

mod appearance;
mod compaction;
pub mod session_import;
mod snapshots;
mod tools;
mod traces;

use std::collections::HashSet;

use ironclaw_gpui_client::product_wire::SettingsToolPermissionState;
use ironclaw_gpui_client::wire::ToolPermission;
use rune::{Cx, El, TextEdit, col};
use rune_kit::Kit;

use super::common::{
    error_line, group_header, header, notice_line, page_body, refresh_button, rows_card,
    setting_row, switch,
};
use crate::jobs;
use crate::state::{Page, State};

const AUTO_APPROVE_KEY: &str = "agent.auto_approve_tools";
const TOOL_KEY_PREFIX: &str = "tool.";

const STATES: [(SettingsToolPermissionState, &str); 4] = [
    (SettingsToolPermissionState::Default, "Default"),
    (SettingsToolPermissionState::AlwaysAllow, "Allow"),
    (SettingsToolPermissionState::AskEachTime, "Ask"),
    (SettingsToolPermissionState::Disabled, "Off"),
];

/// Which settings page is showing; follows `State::page`.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Section {
    #[default]
    General,
    Appearance,
    Permissions,
    Traces,
}

impl Section {
    pub fn of(page: Page) -> Self {
        match page {
            Page::Appearance => Self::Appearance,
            Page::Permissions => Self::Permissions,
            Page::Traces => Self::Traces,
            _ => Self::General,
        }
    }
}

#[derive(Default)]
pub struct SettingsState {
    pub auto_approve: Option<bool>,
    pub tools: Vec<(String, ToolPermission)>,
    pub tool_search: TextEdit,
    /// Permissions shows only tools with their own setting.
    pub tools_changed_only: bool,
    /// Tool groups unfolded by hand.
    pub tool_groups_open: HashSet<String>,
    pub traces: traces::TraceState,
    pub compaction: compaction::CompactionState,
    pub session_import: session_import::SessionImportState,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// The Appearance select whose menu is open.
    pub appearance_menu: Option<&'static str>,
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    traces::refresh(s);
    compaction::refresh(s);
    session_import::refresh(s);
    jobs::run(
        s,
        "settings",
        |c| async move { c.tool_settings().await },
        |s, result, _| {
            let this = &mut s.pages.settings;
            match result {
                Ok(list) => {
                    let mut tools = Vec::new();
                    for entry in list.entries {
                        if entry.key == AUTO_APPROVE_KEY {
                            this.auto_approve = entry.value.as_bool();
                        } else if let Some(id) = entry.key.strip_prefix(TOOL_KEY_PREFIX) {
                            // silent-ok: a row the gateway shapes differently is skipped, not fatal.
                            if let Ok(tool) = serde_json::from_value::<ToolPermission>(entry.value)
                            {
                                tools.push((id.to_string(), tool));
                            }
                        }
                    }
                    tools.sort_by(|a, b| a.0.cmp(&b.0));
                    this.tools = tools;
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn set_auto_approve(s: &mut State, enabled: bool) {
    jobs::run(
        s,
        "settings",
        move |c| async move { c.set_auto_approve(enabled).await },
        |s, result, _| match result {
            // Show the stored value the gateway read back, not the click.
            Ok(resp) => {
                s.pages.settings.auto_approve = resp.entry.value.as_bool();
                s.pages.settings.error = None;
                crate::chat::approval_mode_changed(s);
                // Tools without their own setting follow the switch.
                refresh(s);
            }
            Err(e) => s.pages.settings.error = Some(e.to_string()),
        },
    );
}

fn set_tool(s: &mut State, id: String, state: SettingsToolPermissionState) {
    let job_id = id.clone();
    jobs::run(
        s,
        "settings",
        move |c| async move { c.set_tool_permission(&job_id, state).await },
        move |s, result, _| {
            let this = &mut s.pages.settings;
            match result {
                Ok(resp) => {
                    if let Ok(tool) = serde_json::from_value::<ToolPermission>(resp.entry.value)
                        && let Some(row) = this.tools.iter_mut().find(|(k, _)| *k == id)
                    {
                        row.1 = tool;
                    }
                    this.error = None;
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn render_desktop(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let bundled = crate::login_item::bundle_path().is_some();
    let login = switch(
        k,
        crate::login_item::is_enabled(),
        !bundled,
        "Open at login",
        |s: &mut State| {
            let on = !crate::login_item::is_enabled();
            match crate::login_item::set_enabled(on) {
                Ok(()) => s.pages.settings.error = None,
                Err(e) => s.pages.settings.error = Some(e),
            }
        },
    );
    let status_bar = switch(
        k,
        !s.prefs.status_bar_hidden,
        false,
        "Show the status bar",
        |s: &mut State| s.update_prefs(|p| p.status_bar_hidden = !p.status_bar_hidden),
    );
    rows_card(
        p,
        [
            setting_row(
                p,
                "Open at login",
                Some(if bundled {
                    "Starts ZLO when you log in to your Mac."
                } else {
                    "Available when ZLO runs from ZLO.app (tools/ironclaw_gpui/bundle.sh)."
                }),
                login,
            ),
            setting_row(
                p,
                "Status bar",
                Some("Connection, model and token use along the bottom of the window."),
                status_bar,
            ),
        ],
    )
}

pub fn view(s: &State, page: Page, _cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let this = &s.pages.settings;
    let section = Section::of(page);
    let title = match section {
        Section::General => "General",
        Section::Appearance => "Appearance",
        Section::Permissions => "Permissions",
        Section::Traces => "Trace Commons",
    };
    let mut body = col()
        .w_full()
        .child(header(p, title, refresh_button(k, refresh)));
    if let Some(e) = &this.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &this.notice {
        body = body.child(notice_line(n, p.success));
    }
    let body = match section {
        Section::General => body
            .child(group_header(p, "Desktop app", None))
            .child(render_desktop(s, k))
            .child(snapshots::view(s, k))
            .child(group_header(p, "Conversations", None))
            .child(compaction::view(s, k))
            .child(group_header(
                p,
                "Import terminal sessions",
                Some("Claude Code and Codex sessions you ran in a terminal, as threads you can search and continue."),
            ))
            .child(session_import::view(s, k)),
        Section::Appearance => body.child(appearance::view(s, k, layer)),
        Section::Permissions => body.child(tools::view(s, k)),
        Section::Traces => body
            .child(group_header(
                p,
                "Contributions",
                Some("Traces you contributed and the credits they earned."),
            ))
            .child(rows_card(p, [col().w_full().padding(16.).child(traces::view(s, k))])),
    };
    page_body("settings-page", body)
}

/// Fixture state for the settings scenes.
pub fn fixture(s: &mut State) {
    tools::fixture(s);
    compaction::fixture(s);
    session_import::fixture(s);
    traces::fixture(s);
}
