//! The profile editor's pickers (GPUI `pages/profiles/pickers.rs`): every
//! choice comes from what IronClaw has (providers and models, tools,
//! skills, MCP servers, the Supermemory containers profiles use), never
//! from free text.

use crate::chat::model_picker::pretty_model;
use crate::pages::common::controls::{floating, hairline, hover_row, styled};
use crate::pages::common::glyph;
use crate::pages::common::ts::{SM, XS};
use std::collections::BTreeSet;

use ironclaw_gpui_client::orchestration::RouteProvider;
use ironclaw_gpui_client::profiles::{
    ApprovalMode, ProfileInfo, ProfileSettings, ProfileSupermemory,
};
use rune::{Color, Cursor, El, FileDialogOptions, ScrollSpec, Tx, col, el, row};
use rune_kit::{Kit, Placement, TextInput};

use super::ProfilesState;
use crate::design::smooth_scroll::WheelGlide;
use crate::design::{self, Palette};
use crate::icons;
use crate::state::State;

/// What IronClaw offers each picker, loaded with the page.
#[derive(Default)]
pub(super) struct Catalog {
    /// Signed-in providers the model picker does not hide.
    pub providers: Vec<RouteProvider>,
    /// `(capability id, description)`.
    pub tools: Vec<(String, String)>,
    /// `(skill name, description)`.
    pub skills: Vec<(String, String)>,
    /// `(extension id, display name)` of installed MCP servers.
    pub mcp: Vec<(String, String)>,
}

/// The editor's picked values.
#[derive(Default, Clone, PartialEq)]
pub(super) struct Picks {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub folder: Option<String>,
    pub memory: Option<String>,
    /// Empty: every tool.
    pub tools: BTreeSet<String>,
    pub hidden_skills: BTreeSet<String>,
    /// Empty: every MCP server.
    pub mcp: BTreeSet<String>,
    /// `None`: follow the global auto-approve setting.
    pub approval: Option<ApprovalMode>,
}

impl Picks {
    pub fn of(profile: Option<&ProfileInfo>) -> Self {
        let Some(s) = profile.map(|p| &p.settings) else {
            return Self::default();
        };
        let set = |v: Option<&Vec<String>>| v.into_iter().flatten().cloned().collect();
        Self {
            provider: s.default_provider.clone(),
            model: s.default_model.clone(),
            folder: s.working_folder.clone(),
            memory: s.supermemory.as_ref().map(|m| m.container.clone()),
            tools: set(s.allowed_capabilities.as_ref()),
            hidden_skills: s.hidden_skills.iter().cloned().collect(),
            mcp: set(s.enabled_mcp_servers.as_ref()),
            approval: s.approval_mode,
        }
    }

    /// Writes the picks into `settings`; `base` keeps the Supermemory fields
    /// this form does not edit.
    pub fn apply(&self, settings: &mut ProfileSettings, base: Option<&ProfileSettings>) {
        let all_or = |set: &BTreeSet<String>| {
            Some(set.iter().cloned().collect::<Vec<_>>()).filter(|l| !l.is_empty())
        };
        let previous = base.and_then(|b| b.supermemory.clone());
        settings.supermemory = self.memory.clone().map(|container| ProfileSupermemory {
            container,
            read_containers: previous
                .as_ref()
                .map(|m| m.read_containers.clone())
                .unwrap_or_default(),
            instructions: previous.and_then(|m| m.instructions),
        });
        settings.allowed_capabilities = all_or(&self.tools);
        settings.hidden_skills = self.hidden_skills.iter().cloned().collect();
        settings.enabled_mcp_servers = all_or(&self.mcp);
        settings.default_provider = self.provider.clone();
        settings.default_model = self.model.clone();
        settings.working_folder = self.folder.clone();
        settings.approval_mode = self.approval;
    }

    /// The approval mode and allowlists the person did not change since
    /// `origin` (the picks the form opened with) take `current`, the
    /// profile as the server has it now. So a stale form never puts back
    /// "Full access" set elsewhere, and an untouched "allow nothing" list
    /// never turns into "allow everything".
    pub fn keep_untouched(
        &self,
        origin: &Picks,
        settings: &mut ProfileSettings,
        current: &ProfileSettings,
    ) {
        if self.approval == origin.approval {
            settings.approval_mode = current.approval_mode;
        }
        if self.tools == origin.tools {
            settings.allowed_capabilities = current.allowed_capabilities.clone();
        }
        if self.mcp == origin.mcp {
            settings.enabled_mcp_servers = current.enabled_mcp_servers.clone();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Menu {
    Model,
    Memory,
    Tools,
    Skills,
    Mcp,
    Approval,
}

/// One row of a multi-choice list.
struct Choice {
    id: String,
    label: String,
    detail: String,
}

use crate::chat::approval_mode::{
    CHOICES as APPROVAL_CHOICES, key as approval_key, label as approval_label,
};

fn pr(s: &mut State) -> &mut ProfilesState {
    &mut s.pages.profiles
}

pub(super) fn toggle_menu(s: &mut State, menu: Menu) {
    let ps = pr(s);
    if ps.menu == Some(menu) {
        ps.menu = None;
    } else {
        ps.menu = Some(menu);
        ps.rail = None;
        ps.search.replace_text("");
    }
}

fn close(s: &mut State) {
    pr(s).menu = None;
}

pub(super) fn pick_folder(_s: &mut State, tx: &mut Tx) {
    let options = FileDialogOptions {
        title: "Use this folder".into(),
        ..Default::default()
    };
    tx.open_folder(options, |s: &mut State, result, _tx| match result {
        Ok(Some(path)) => pr(s).picks.folder = Some(path.to_string_lossy().into_owned()),
        // Cancelled.
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "folder dialog failed");
            pr(s).error = Some(e.to_string());
        }
    });
}

/// Containers other profiles use, plus a new one named after this profile.
fn containers(ps: &ProfilesState) -> Vec<(String, bool)> {
    let mut used = BTreeSet::new();
    for p in &ps.items {
        if let Some(m) = &p.settings.supermemory {
            used.insert(m.container.clone());
            used.extend(m.read_containers.iter().cloned());
        }
    }
    if let Some(current) = &ps.picks.memory {
        used.insert(current.clone());
    }
    let own = ps
        .selected
        .clone()
        .unwrap_or_else(|| ps.id.text.trim().to_string());
    let mut out: Vec<(String, bool)> = used.into_iter().map(|c| (c, false)).collect();
    if !own.is_empty() && !out.iter().any(|(c, _)| *c == own) {
        out.push((own, true));
    }
    out
}

fn query(ps: &ProfilesState) -> String {
    ps.search.text.trim().to_lowercase()
}

/// The button that shows a field's value and opens its menu; while open,
/// its popover goes into `layer`.
pub(super) fn trigger(s: &State, k: &Kit, layer: &mut Vec<El>, menu: Menu, summary: String) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let open = ps.menu == Some(menu);
    let key = format!("profile-menu-{}", menu as u8);
    if open {
        let content = match menu {
            Menu::Model => model_popover(s, k),
            Menu::Memory => memory_popover(s),
            Menu::Approval => approval_popover(s),
            Menu::Tools => checklist_popover(s, k, menu, "All tools (no allowlist)"),
            Menu::Skills => checklist_popover(s, k, menu, "No hidden skills"),
            Menu::Mcp => checklist_popover(s, k, menu, "All MCP servers"),
        };
        let width = if menu == Menu::Model { 460.0 } else { 380.0 };
        layer.push(rune_kit::scrim(format!("{key}-scrim"), close));
        layer.push(rune_kit::popover::anchored(
            k,
            floating(k, content.w(width).key(format!("{key}-pop"))),
            &key,
            Placement::BottomEnd,
            4.0,
        ));
    }
    row()
        .key(key)
        .role("button")
        .label(summary.clone())
        .expanded(open)
        .w_full()
        .h(32.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .border(1.0, if open { p.accent } else { p.border })
        .cursor(Cursor::Pointer)
        .hover(p.hover)
        .child(styled(summary, SM, p.text).truncate().grow(1.0).min_w(0.0))
        .child(glyph(&icons::CHEVRON_DOWN, 12.0, p.text_muted))
        .on_press(move |s: &mut State| toggle_menu(s, menu))
}

pub(super) fn model_summary(ps: &ProfilesState) -> String {
    let name = |id: &str| {
        ps.catalog
            .providers
            .iter()
            .find(|p| p.provider == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    match (&ps.picks.provider, &ps.picks.model) {
        (None, None) => "IronClaw default".into(),
        (Some(p), None) => format!("{} · its default model", name(p)),
        (Some(p), Some(m)) => format!("{} · {}", pretty_model(m), name(p)),
        (None, Some(m)) => pretty_model(m),
    }
}

fn search_field(k: &Kit) -> impl Fn(&ProfilesState) -> El + '_ {
    move |ps: &ProfilesState| {
        TextInput::new(&ps.search)
            .width(None)
            .placeholder("Search")
            .label("Search")
            .on_edit(|s: &mut State| &mut s.pages.profiles.search)
            .build(k)
            .key("profile-picker-search")
    }
}

/// A scroll viewport capped at `max` points tall.
fn capped(key: &str, max: f32, rows: El) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .max_h(max)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .wheel_glide(key)
        .child(rows)
}

/// Providers on the left, the rail provider's models on the right.
fn model_popover(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let providers = &ps.catalog.providers;
    let rail = ps
        .rail
        .clone()
        .or_else(|| ps.picks.provider.clone())
        .or_else(|| providers.first().map(|p| p.provider.clone()));
    let mut rail_rows = col().w_full();
    for rp in providers {
        let id = rp.provider.clone();
        let on = rail.as_deref() == Some(id.as_str());
        rail_rows = rail_rows.child(
            hover_row(p, format!("rail-{id}"), on)
                .child(crate::chat::model_picker::provider_mark(
                    s,
                    rp.brand.as_deref(),
                    &rp.runtime,
                    &rp.provider,
                    14.0,
                ))
                .child(
                    styled(rp.name.clone(), SM, p.text)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .on_press(move |s: &mut State| pr(s).rail = Some(id.clone())),
        );
    }
    let q = query(ps);
    let mut model_rows = col().w_full();
    if let Some(rp) = providers
        .iter()
        .find(|rp| Some(&rp.provider) == rail.as_ref())
    {
        let provider_on =
            ps.picks.provider.as_ref() == Some(&rp.provider) && ps.picks.model.is_none();
        let provider = rp.provider.clone();
        model_rows = model_rows.child(
            pick_row(
                p,
                format!("model-{provider}-default"),
                "Its default model".into(),
                provider_on,
            )
            .on_press(move |s: &mut State| {
                let ps = pr(s);
                ps.picks.provider = Some(provider.clone());
                ps.picks.model = None;
                ps.menu = None;
            }),
        );
        for model in rp.models.iter().filter(|m| {
            !ps.picker_prefs.model_hidden(&rp.provider, m)
                && (q.is_empty() || m.to_lowercase().contains(&q))
        }) {
            let on = ps.picks.provider.as_ref() == Some(&rp.provider)
                && ps.picks.model.as_ref() == Some(model);
            let (provider, model) = (rp.provider.clone(), model.clone());
            model_rows = model_rows.child(
                pick_row(
                    p,
                    format!("model-{provider}-{model}"),
                    pretty_model(&model),
                    on,
                )
                .on_press(move |s: &mut State| {
                    let ps = pr(s);
                    ps.picks.provider = Some(provider.clone());
                    ps.picks.model = Some(model.clone());
                    ps.menu = None;
                }),
            );
        }
    }
    let none_on = ps.picks.provider.is_none() && ps.picks.model.is_none();
    let mut c = col()
        .padding(4.0)
        .gap(4.0)
        .child(
            pick_row(p, "model-none".into(), "IronClaw default".into(), none_on).on_press(
                |s: &mut State| {
                    let ps = pr(s);
                    ps.picks.provider = None;
                    ps.picks.model = None;
                    ps.menu = None;
                },
            ),
        )
        .child(hairline(p).w_full())
        .child(
            row()
                .w_full()
                .items_start()
                .gap(4.0)
                .child(
                    el().w(170.0)
                        .child(capped("profile-model-rail", 320.0, rail_rows)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(search_field(k)(ps))
                        .child(capped("profile-model-list", 280.0, model_rows)),
                ),
        );
    if providers.is_empty() {
        c = c.child(note(p, "No signed-in provider. Add one under Providers."));
    }
    c
}

fn memory_popover(s: &State) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let none_on = ps.picks.memory.is_none();
    let mut c = col().padding(4.0).child(
        pick_row(p, "memory-none".into(), "None".into(), none_on).on_press(|s: &mut State| {
            let ps = pr(s);
            ps.picks.memory = None;
            ps.menu = None;
        }),
    );
    for (container, new) in containers(ps) {
        let on = ps.picks.memory.as_ref() == Some(&container);
        let text = if new {
            format!("{container} (new, created on first write)")
        } else {
            container.clone()
        };
        c = c.child(
            pick_row(p, format!("memory-{container}"), text, on).on_press(move |s: &mut State| {
                let ps = pr(s);
                ps.picks.memory = Some(container.clone());
                ps.menu = None;
            }),
        );
    }
    c
}

pub(super) fn approval_summary(ps: &ProfilesState) -> String {
    approval_label(ps.picks.approval).into()
}

/// Full access, ask first, or follow the global setting.
fn approval_popover(s: &State) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let mut c = col().padding(4.0);
    for mode in APPROVAL_CHOICES {
        let on = ps.picks.approval == mode;
        c = c.child(
            pick_row(
                p,
                format!("approval-{}", approval_key(mode)),
                approval_label(mode).into(),
                on,
            )
            .on_press(move |s: &mut State| {
                let ps = pr(s);
                ps.picks.approval = mode;
                ps.menu = None;
            }),
        );
    }
    c
}

fn picked_set(ps: &mut ProfilesState, menu: Menu) -> &mut BTreeSet<String> {
    match menu {
        Menu::Tools => &mut ps.picks.tools,
        Menu::Skills => &mut ps.picks.hidden_skills,
        _ => &mut ps.picks.mcp,
    }
}

/// A searchable checklist. `empty_label` is the row meaning "nothing
/// picked" (all tools, no hidden skill).
fn checklist_popover(s: &State, k: &Kit, menu: Menu, empty_label: &'static str) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let (offered, picked): (&[(String, String)], &BTreeSet<String>) = match menu {
        Menu::Tools => (&ps.catalog.tools, &ps.picks.tools),
        Menu::Skills => (&ps.catalog.skills, &ps.picks.hidden_skills),
        _ => (&ps.catalog.mcp, &ps.picks.mcp),
    };
    let mut choices: Vec<Choice> = offered
        .iter()
        .map(|(id, detail)| Choice {
            id: id.clone(),
            label: if menu == Menu::Mcp {
                detail.clone()
            } else {
                id.clone()
            },
            detail: if menu == Menu::Mcp {
                id.clone()
            } else {
                detail.clone()
            },
        })
        .collect();
    // A saved pick IronClaw no longer has stays visible so it can be removed.
    for id in picked {
        if !offered.iter().any(|(o, _)| o == id) {
            choices.push(Choice {
                id: id.clone(),
                label: id.clone(),
                detail: "Not installed".into(),
            });
        }
    }
    let q = query(ps);
    let mut rows = col().w_full();
    for c in choices.into_iter().filter(|c| {
        q.is_empty() || c.label.to_lowercase().contains(&q) || c.detail.to_lowercase().contains(&q)
    }) {
        let on = picked.contains(&c.id);
        let id = c.id.clone();
        let mut text = col()
            .grow(1.0)
            .min_w(0.0)
            .child(styled(c.label, SM, p.text).truncate());
        if !c.detail.is_empty() {
            text = text.child(styled(c.detail, XS, p.text_faint).truncate());
        }
        rows = rows.child(
            hover_row(p, format!("check-{}-{}", menu as u8, c.id), false)
                .role("checkbox")
                .checked(on)
                .items_start()
                .py(4.0)
                .child(check_box(p, on))
                .child(text)
                .on_press(move |s: &mut State| {
                    let set = picked_set(pr(s), menu);
                    if !set.remove(&id) {
                        set.insert(id.clone());
                    }
                }),
        );
    }
    let mut c = col()
        .padding(4.0)
        .gap(4.0)
        .child(search_field(k)(ps))
        .child(
            pick_row(
                p,
                format!("check-{}-none", menu as u8),
                empty_label.into(),
                picked.is_empty(),
            )
            .on_press(move |s: &mut State| picked_set(pr(s), menu).clear()),
        )
        .child(hairline(p).w_full())
        .child(capped(
            &format!("profile-check-list-{}", menu as u8),
            320.0,
            rows,
        ));
    if offered.is_empty() {
        c = c.child(note(p, "Nothing installed yet."));
    }
    c
}

/// A single-choice row: the label and a tick when picked.
fn pick_row(p: &Palette, key: String, text: String, on: bool) -> El {
    let mut r = hover_row(p, key, on)
        .role("menuitemradio")
        .checked(on)
        .child(styled(text, SM, p.text).truncate().grow(1.0).min_w(0.0));
    if on {
        r = r.child(glyph(&icons::CHECK, 13.0, p.text_muted));
    }
    r
}

fn check_box(p: &Palette, on: bool) -> El {
    let b = row()
        .mt(2.0)
        .w(14.0)
        .h(14.0)
        .radius(4.0)
        .center()
        .border(1.0, if on { p.accent } else { p.border_strong })
        .a11y_hidden();
    if on {
        b.fill(p.accent)
            .child(glyph(&icons::CHECK, 10.0, p.on_solid))
    } else {
        b.fill(Color::TRANSPARENT)
    }
}

fn note(p: &Palette, text: &'static str) -> El {
    el().px(8.0).py(4.0).child(styled(text, XS, p.text_faint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_security_fields_follow_the_server() {
        let origin = Picks::default();
        let picks = Picks {
            model: Some("m".into()),
            ..Picks::default()
        };
        let mut settings = ProfileSettings::default();
        picks.apply(&mut settings, None);
        let current = ProfileSettings {
            approval_mode: Some(ApprovalMode::FullAccess),
            allowed_capabilities: Some(vec![]),
            ..ProfileSettings::default()
        };
        picks.keep_untouched(&origin, &mut settings, &current);
        assert_eq!(settings.approval_mode, Some(ApprovalMode::FullAccess));
        assert_eq!(settings.allowed_capabilities, Some(vec![]));
        assert_eq!(settings.default_model.as_deref(), Some("m"));
    }
}
