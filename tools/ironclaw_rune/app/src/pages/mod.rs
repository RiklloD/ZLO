//! Settings and management pages, one module per page, each with a slot in
//! [`PagesState`]. Owners: PLAN.md areas C, D, E (and F for the voice page).
//! Shared page helpers (headers, sections, rows) go in `pages/common.rs`,
//! owned by area D; until it lands, keep helpers local.

pub mod automations;
pub mod c_widgets;
pub mod extensions;
pub mod files;
pub mod hooks;
pub mod memory;
pub mod notifications;
pub mod operator;
pub mod profiles;
pub mod projects;
pub mod providers;
pub mod settings;
pub mod skills;
pub mod usage;

use rune::{Cx, El};
use rune_kit::Kit;

use crate::state::{Page, State};

#[derive(Default)]
pub struct PagesState {
    pub providers: providers::ProvidersState,
    pub extensions: extensions::ExtensionsState,
    pub operator: operator::OperatorState,
    pub skills: skills::SkillsState,
    pub hooks: hooks::HooksState,
    pub automations: automations::AutomationsState,
    pub notifications: notifications::NotificationsState,
    pub files: files::FilesState,
    pub projects: projects::ProjectsState,
    pub profiles: profiles::ProfilesState,
    pub memory: memory::MemoryState,
    pub usage: usage::UsageState,
    pub settings: settings::SettingsState,
}

/// `page` became visible: load it on first visit, refresh otherwise.
pub fn opened(s: &mut State, page: Page) {
    match page {
        Page::Chat | Page::Draft => {}
        Page::Settings | Page::Appearance | Page::Permissions | Page::Traces => settings::opened(s),
        Page::Voice => crate::voice::settings_opened(s),
        Page::Notifications => notifications::opened(s),
        Page::Automations => automations::opened(s),
        Page::Files => files::opened(s),
        Page::Projects => projects::opened(s),
        Page::Profiles => profiles::opened(s),
        Page::Extensions => extensions::opened(s),
        Page::Skills => skills::opened(s),
        Page::Providers => providers::opened(s),
        Page::Operator => operator::opened(s),
        Page::Usage => usage::opened(s),
        Page::Memory => memory::opened(s),
        Page::Hooks => hooks::opened(s),
    }
}

pub fn view(s: &State, page: Page, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    match page {
        Page::Chat | Page::Draft => crate::chat::view(s, cx, k, layer),
        Page::Settings | Page::Appearance | Page::Permissions | Page::Traces => {
            settings::view(s, page, cx, k, layer)
        }
        Page::Voice => crate::voice::settings_view(s, cx, k, layer),
        Page::Notifications => notifications::view(s, cx, k, layer),
        Page::Automations => automations::view(s, cx, k, layer),
        Page::Files => files::view(s, cx, k, layer),
        Page::Projects => projects::view(s, cx, k, layer),
        Page::Profiles => profiles::view(s, cx, k, layer),
        Page::Extensions => extensions::view(s, cx, k, layer),
        Page::Skills => skills::view(s, cx, k, layer),
        Page::Providers => providers::view(s, cx, k, layer),
        Page::Operator => operator::view(s, cx, k, layer),
        Page::Usage => usage::view(s, cx, k, layer),
        Page::Memory => memory::view(s, cx, k, layer),
        Page::Hooks => hooks::view(s, cx, k, layer),
    }
}
