//! The profile editor's pickers: every choice comes from what IronClaw has
//! (providers and models, tools, skills, MCP servers, the Supermemory
//! containers profiles use), never from free text.

use std::collections::BTreeSet;

use ironclaw_gpui_client::orchestration::RouteProvider;
use ironclaw_gpui_client::profiles::{
    ApprovalMode, ProfileInfo, ProfileSettings, ProfileSupermemory,
};

use super::ProfilesView;
use crate::chat::model_picker::{pretty_model, provider_mark};
use crate::design::{self, Palette, kit};
use crate::ui::*;

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
#[derive(Default, Clone)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
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

impl ProfilesView {
    pub(super) fn toggle_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
        } else {
            self.menu = Some(menu);
            self.rail = None;
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    pub(super) fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = picked.await
                && let Some(path) = paths.pop()
            {
                // silent-ok: the page closed while the dialog was open.
                let _ = this.update(cx, |this, cx| {
                    this.picks.folder = Some(path.to_string_lossy().into_owned());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Containers other profiles use, plus a new one named after this profile.
    fn containers(&self, cx: &App) -> Vec<(String, bool)> {
        let mut used = BTreeSet::new();
        for p in &self.items {
            if let Some(m) = &p.settings.supermemory {
                used.insert(m.container.clone());
                used.extend(m.read_containers.iter().cloned());
            }
        }
        if let Some(current) = &self.picks.memory {
            used.insert(current.clone());
        }
        let own = self
            .selected
            .clone()
            .unwrap_or_else(|| super::Editor::text(&self.editor.id, cx));
        let mut out: Vec<(String, bool)> = used.into_iter().map(|c| (c, false)).collect();
        if !own.is_empty() && !out.iter().any(|(c, _)| *c == own) {
            out.push((own, true));
        }
        out
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().trim().to_lowercase()
    }

    /// The button that shows a field's value and opens its menu.
    pub(super) fn trigger(
        &self,
        menu: Menu,
        summary: String,
        popover: impl FnOnce() -> AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let open = self.menu == Some(menu);
        let id = SharedString::from(format!("profile-menu-{}", menu as u8));
        div()
            .relative()
            .w_full()
            .child(
                h_flex()
                    .id(id)
                    .role(Role::Button)
                    .aria_label(summary.clone())
                    .w_full()
                    .h(px(32.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(px(design::CONTROL_RADIUS))
                    .border_1()
                    .border_color(if open { p.accent } else { p.border })
                    .text_sm()
                    .cursor_pointer()
                    .hover(|s| s.border_color(p.border_strong))
                    .child(div().flex_1().min_w_0().truncate().child(summary))
                    .child(Icon::new(IconName::ChevronDown).xsmall())
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.toggle_menu(menu, window, cx)),
                    ),
            )
            .when(open, |d| {
                d.child(
                    gpui_kit::deferred(
                        div()
                            .absolute()
                            .top(px(36.))
                            .right_0()
                            .w(px(if menu == Menu::Model { 460. } else { 380. }))
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                cx.notify();
                            }))
                            .child(popover()),
                    )
                    .with_priority(1),
                )
            })
            .into_any_element()
    }

    pub(super) fn model_summary(&self) -> String {
        let name = |id: &str| {
            self.catalog
                .providers
                .iter()
                .find(|p| p.provider == id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string())
        };
        match (&self.picks.provider, &self.picks.model) {
            (None, None) => "IronClaw default".into(),
            (Some(p), None) => format!("{} · its default model", name(p)),
            (Some(p), Some(m)) => format!("{} · {}", pretty_model(m), name(p)),
            (None, Some(m)) => pretty_model(m),
        }
    }

    /// Providers on the left, the rail provider's models on the right.
    pub(super) fn model_popover(&self, cx: &Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let providers = &self.catalog.providers;
        let rail = self
            .rail
            .clone()
            .or_else(|| self.picks.provider.clone())
            .or_else(|| providers.first().map(|p| p.provider.clone()));
        let rail_rows = providers.iter().map(|rp| {
            let id = rp.provider.clone();
            let on = rail.as_deref() == Some(id.as_str());
            kit::row(SharedString::from(format!("rail-{id}")), on, cx)
                .gap_2()
                .text_sm()
                .child(provider_mark(
                    rp.brand.as_deref(),
                    &rp.runtime,
                    &rp.provider,
                    14.,
                    cx,
                ))
                .child(div().flex_1().truncate().child(rp.name.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.rail = Some(id.clone());
                    cx.notify();
                }))
        });
        let query = self.query(cx);
        let rail_provider = providers
            .iter()
            .find(|rp| Some(&rp.provider) == rail.as_ref());
        let mut model_rows: Vec<AnyElement> = Vec::new();
        if let Some(rp) = rail_provider {
            let provider_on =
                self.picks.provider.as_ref() == Some(&rp.provider) && self.picks.model.is_none();
            let provider = rp.provider.clone();
            model_rows.push(
                pick_row(
                    format!("model-{provider}-default"),
                    "Its default model".into(),
                    provider_on,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.picks.provider = Some(provider.clone());
                    this.picks.model = None;
                    this.menu = None;
                    cx.notify();
                }))
                .into_any_element(),
            );
            let hidden = &self.picker_prefs;
            for model in rp.models.iter().filter(|m| {
                !hidden.model_hidden(&rp.provider, m)
                    && (query.is_empty() || m.to_lowercase().contains(&query))
            }) {
                let on = self.picks.provider.as_ref() == Some(&rp.provider)
                    && self.picks.model.as_ref() == Some(model);
                let (provider, model) = (rp.provider.clone(), model.clone());
                model_rows.push(
                    pick_row(
                        format!("model-{provider}-{model}"),
                        pretty_model(&model),
                        on,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.picks.provider = Some(provider.clone());
                        this.picks.model = Some(model.clone());
                        this.menu = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                );
            }
        }
        let none_on = self.picks.provider.is_none() && self.picks.model.is_none();
        let pop = kit::popover("profile-model-pop", cx);
        pop(v_flex()
            .w_full()
            .p_1()
            .gap_1()
            .child(
                pick_row("model-none".into(), "IronClaw default".into(), none_on, cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.picks.provider = None;
                        this.picks.model = None;
                        this.menu = None;
                        cx.notify();
                    }),
                ),
            )
            .child(div().h_px().bg(p.border))
            .child(
                h_flex()
                    .items_start()
                    .gap_1()
                    .child(
                        div()
                            .id("profile-model-rail")
                            .w(px(170.))
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .children(rail_rows),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(Input::new(&self.search).small())
                            .child(
                                div()
                                    .id("profile-model-list")
                                    .max_h(px(280.))
                                    .overflow_y_scroll()
                                    .children(model_rows),
                            ),
                    ),
            )
            .when(providers.is_empty(), |d| {
                d.child(note("No signed-in provider. Add one under Providers.", cx))
            }))
    }

    pub(super) fn memory_popover(&self, cx: &Context<Self>) -> AnyElement {
        let none_on = self.picks.memory.is_none();
        let rows = self.containers(cx).into_iter().map(|(container, new)| {
            let on = self.picks.memory.as_ref() == Some(&container);
            let label = if new {
                format!("{container} (new, created on first write)")
            } else {
                container.clone()
            };
            pick_row(format!("memory-{container}"), label, on, cx).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.picks.memory = Some(container.clone());
                    this.menu = None;
                    cx.notify();
                },
            ))
        });
        let pop = kit::popover("profile-memory-pop", cx);
        pop(v_flex()
            .w_full()
            .p_1()
            .child(
                pick_row("memory-none".into(), "None".into(), none_on, cx).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.picks.memory = None;
                        this.menu = None;
                        cx.notify();
                    },
                )),
            )
            .children(rows))
    }

    pub(super) fn approval_summary(&self) -> String {
        crate::chat::approval_mode::label(self.picks.approval).into()
    }

    /// Full access, ask first, or follow the global setting.
    pub(super) fn approval_popover(&self, cx: &Context<Self>) -> AnyElement {
        let rows = crate::chat::approval_mode::CHOICES.iter().map(|&mode| {
            let on = self.picks.approval == mode;
            pick_row(
                format!("approval-{}", crate::chat::approval_mode::key(mode)),
                crate::chat::approval_mode::label(mode).into(),
                on,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.picks.approval = mode;
                this.menu = None;
                cx.notify();
            }))
        });
        let pop = kit::popover("profile-approval-pop", cx);
        pop(v_flex().w_full().p_1().children(rows))
    }

    /// A searchable checklist. `empty_label` is the row meaning "nothing
    /// picked" (all tools, no hidden skill).
    pub(super) fn checklist_popover(
        &self,
        menu: Menu,
        empty_label: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let (offered, picked): (&[(String, String)], &BTreeSet<String>) = match menu {
            Menu::Tools => (&self.catalog.tools, &self.picks.tools),
            Menu::Skills => (&self.catalog.skills, &self.picks.hidden_skills),
            _ => (&self.catalog.mcp, &self.picks.mcp),
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
        let query = self.query(cx);
        let rows = choices
            .into_iter()
            .filter(|c| {
                query.is_empty()
                    || c.label.to_lowercase().contains(&query)
                    || c.detail.to_lowercase().contains(&query)
            })
            .map(|c| {
                let on = picked.contains(&c.id);
                let id = c.id.clone();
                kit::row(
                    SharedString::from(format!("check-{}-{}", menu as u8, c.id)),
                    false,
                    cx,
                )
                .gap_2()
                .items_start()
                .child(check_box(on, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(c.label))
                        .when(!c.detail.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(p.text_faint)
                                    .truncate()
                                    .child(c.detail),
                            )
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let set = match menu {
                        Menu::Tools => &mut this.picks.tools,
                        Menu::Skills => &mut this.picks.hidden_skills,
                        _ => &mut this.picks.mcp,
                    };
                    if !set.remove(&id) {
                        set.insert(id.clone());
                    }
                    cx.notify();
                }))
            });
        let pop = kit::popover(("profile-check-pop", menu as usize), cx);
        pop(v_flex()
            .w_full()
            .p_1()
            .gap_1()
            .child(Input::new(&self.search).small())
            .child(
                pick_row(
                    format!("check-{}-none", menu as u8),
                    empty_label.into(),
                    picked.is_empty(),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    match menu {
                        Menu::Tools => this.picks.tools.clear(),
                        Menu::Skills => this.picks.hidden_skills.clear(),
                        _ => this.picks.mcp.clear(),
                    }
                    cx.notify();
                })),
            )
            .child(div().h_px().bg(p.border))
            .child(
                div()
                    .id(("profile-check-list", menu as usize))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .when(offered.is_empty(), |d| {
                d.child(note("Nothing installed yet.", cx))
            }))
    }
}

/// A single-choice row: the label and a tick when picked.
fn pick_row(id: String, label: String, on: bool, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = Palette::of(cx);
    kit::row(SharedString::from(id), on, cx)
        .gap_2()
        .text_sm()
        .child(div().flex_1().truncate().child(label))
        .when(on, |d| {
            d.child(
                Icon::new(IconName::Check)
                    .size(px(13.))
                    .text_color(p.text_muted),
            )
        })
}

fn check_box(on: bool, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    div()
        .flex_none()
        .mt(px(2.))
        .size(px(14.))
        .rounded(px(4.))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .border_color(if on { p.accent } else { p.border_strong })
        .when(on, |d| {
            d.bg(p.accent).child(
                Icon::new(IconName::Check)
                    .size(px(10.))
                    .text_color(p.on_solid),
            )
        })
}

fn note(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .text_xs()
        .text_color(Palette::of(cx).text_faint)
        .child(text)
}
