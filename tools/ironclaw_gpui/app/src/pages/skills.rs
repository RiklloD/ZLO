//! Skills: installed skills with auto-activation, their instructions, removal,
//! and a catalog search to install more.

use std::collections::HashMap;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::{RebornSkillInfo, RebornSkillSourceKind};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

/// One catalog hit. Catalog rows are untyped JSON on the wire.
struct CatalogSkill {
    name: String,
    description: String,
}

pub struct SkillsView {
    backend: Backend,
    skills: Vec<RebornSkillInfo>,
    open: HashMap<String, Option<String>>,
    query: Entity<InputState>,
    results: Option<Result<Vec<CatalogSkill>, SharedString>>,
    searching: bool,
    busy: Option<String>,
    confirm_remove: Option<String>,
    loaded: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    _query_events: Subscription,
}

impl EventEmitter<PageEvent> for SkillsView {}

impl SkillsView {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search the skill catalog"));
        let query_events = cx.subscribe(&query, |this, _, event, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.search(cx);
            }
        });
        let mut this = Self {
            backend,
            skills: Vec::new(),
            open: HashMap::new(),
            query,
            results: None,
            searching: false,
            busy: None,
            confirm_remove: None,
            loaded: false,
            error: None,
            notice: None,
            _query_events: query_events,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.skills().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        this.skills = list.skills;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn toggle_content(&mut self, name: String, cx: &mut Context<Self>) {
        if self.open.remove(&name).is_some() {
            cx.notify();
            return;
        }
        self.open.insert(name.clone(), None);
        cx.notify();
        let job_name = name.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.skill_content(&job_name).await },
            move |this, result, cx| {
                match result {
                    Ok(skill) if this.open.contains_key(&name) => {
                        this.open.insert(name, Some(skill.content));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        this.open.remove(&name);
                        this.error = Some(e.to_string().into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn set_auto(&mut self, name: String, enabled: bool, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_skill_auto_activate(&name, enabled).await },
            |this, result, cx| match result {
                // Re-list so the switch shows the stored value.
                Ok(_) => this.refresh(cx),
                Err(e) => {
                    this.error = Some(e.to_string().into());
                    cx.notify();
                }
            },
        );
    }

    fn remove(&mut self, name: String, cx: &mut Context<Self>) {
        self.confirm_remove = None;
        self.busy = Some(name.clone());
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.remove_skill(&name).await },
            |this, result, cx| {
                this.busy = None;
                match result {
                    Ok(resp) => {
                        this.notice = Some(resp.message.into());
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().trim().to_string();
        if query.is_empty() || self.searching {
            return;
        }
        self.searching = true;
        self.error = None;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.search_skills(&query).await },
            |this, result, cx| {
                this.searching = false;
                this.results = Some(match result {
                    Ok(resp) => match resp.catalog_error {
                        Some(e) if resp.catalog.is_empty() => Err(e.into()),
                        // An empty registry URL means the gateway has no catalog to search.
                        None if resp.registry_url.is_empty() => {
                            Err("No skill catalog is configured on this gateway.".into())
                        }
                        _ => Ok(resp
                            .catalog
                            .iter()
                            .filter_map(|v| {
                                Some(CatalogSkill {
                                    name: v.get("name")?.as_str()?.to_string(),
                                    description: v
                                        .get("description")
                                        .and_then(|d| d.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                })
                            })
                            .collect()),
                    },
                    Err(e) => Err(e.to_string().into()),
                });
                cx.notify();
            },
        );
    }

    fn install(&mut self, name: String, cx: &mut Context<Self>) {
        self.busy = Some(name.clone());
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.install_skill(&name).await },
            |this, result, cx| {
                this.busy = None;
                match result {
                    Ok(resp) if resp.success => {
                        this.notice = Some(resp.message.into());
                        this.refresh(cx);
                    }
                    Ok(resp) => this.error = Some(resp.message.into()),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_skill(&self, ix: usize, s: &RebornSkillInfo, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let name = s.name.clone();
        let source = match s.source_kind {
            RebornSkillSourceKind::System => "Built in",
            RebornSkillSourceKind::Installed => "Installed",
            RebornSkillSourceKind::User => "Yours",
            RebornSkillSourceKind::Workspace => "Workspace",
        };
        let open = self.open.get(&s.name);
        let confirming = self.confirm_remove.as_deref() == Some(s.name.as_str());
        let busy = self.busy.as_deref() == Some(s.name.as_str());

        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(format!("/{}", s.name)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .whitespace_normal()
                                    .child(s.description.clone()),
                            ),
                    )
                    .child(Tag::secondary().small().child(source))
                    // Built-in skills are read-only; the gateway rejects the toggle for them.
                    .when(s.can_edit, |d| {
                        d.child(
                            Switch::new(("auto", ix))
                                .checked(s.auto_activate)
                                .accessibility_label(format!("Auto-activate {}", s.name))
                                .tooltip("Activate automatically when relevant")
                                .on_click(cx.listener({
                                    let name = name.clone();
                                    move |this, checked: &bool, _, cx| {
                                        this.set_auto(name.clone(), *checked, cx)
                                    }
                                })),
                        )
                    })
                    // The gateway serves skill content only for editable (user/installed) skills.
                    .when(s.can_edit, |d| {
                        d.child(
                            Button::new(("view", ix))
                                .label(if open.is_some() { "Hide" } else { "View" })
                                .ghost()
                                .small()
                                .on_click(cx.listener({
                                    let name = name.clone();
                                    move |this, _, _, cx| this.toggle_content(name.clone(), cx)
                                })),
                        )
                    })
                    .when(s.can_delete, |d| {
                        let name = name.clone();
                        d.child(if confirming {
                            Button::new(("confirm-remove-skill", ix))
                                .label("Remove?")
                                .danger()
                                .small()
                                .disabled(busy)
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.remove(name.clone(), cx)
                                    }),
                                )
                        } else {
                            Button::new(("remove-skill", ix))
                                .icon(IconName::Delete)
                                .ghost()
                                .small()
                                .tooltip("Remove skill")
                                .accessibility_label(format!("Remove {}", s.name))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.confirm_remove = Some(name.clone());
                                    cx.notify();
                                }))
                        })
                    }),
            )
            .when_some(open, |d, content| {
                d.child(match content {
                    None => Spinner::new().into_any_element(),
                    Some(text) => div()
                        .pt_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(markdown(
                            SharedString::from(format!("skill:{}", s.name)),
                            text.clone(),
                            false,
                        ))
                        .into_any_element(),
                })
            })
            .into_any_element()
    }
}

impl Render for SkillsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let actions = refresh_button("refresh-skills")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let installed: Vec<String> = self.skills.iter().map(|s| s.name.clone()).collect();
        let results: Option<AnyElement> =
            self.results.as_ref().map(|r| match r {
                Err(e) => error_line(e, cx).into_any_element(),
                Ok(list) if list.is_empty() => {
                    empty_state("No matching skills.", cx).into_any_element()
                }
                Ok(list) => v_flex()
                    .children(list.iter().enumerate().map(|(ix, hit)| {
                        let have = installed.contains(&hit.name);
                        let name = hit.name.clone();
                        let busy = self.busy.as_deref() == Some(hit.name.as_str());
                        h_flex()
                            .gap_3()
                            .py_2()
                            .items_center()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().font_medium().child(hit.name.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .whitespace_normal()
                                            .child(hit.description.clone()),
                                    ),
                            )
                            .child(if have {
                                Tag::success().small().child("Installed").into_any_element()
                            } else {
                                Button::new(("install-skill", ix))
                                    .label("Install")
                                    .outline()
                                    .small()
                                    .loading(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.install(name.clone(), cx)
                                    }))
                                    .into_any_element()
                            })
                    }))
                    .into_any_element(),
            });
        let rows: Vec<AnyElement> = self
            .skills
            .iter()
            .enumerate()
            .map(|(ix, s)| self.render_skill(ix, s, cx))
            .collect();

        let body = v_flex()
            .child(header("Skills", actions))
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
            .child(
                h_flex()
                    .gap_2()
                    .pb_3()
                    .child(div().flex_1().child(Input::new(&self.query)))
                    .child(
                        Button::new("search-skills")
                            .icon(IconName::Search)
                            .label("Search")
                            .outline()
                            .small()
                            .loading(self.searching)
                            .on_click(cx.listener(|this, _, _, cx| this.search(cx))),
                    ),
            )
            .when_some(results, |d, r| d.child(div().pb_6().child(r)))
            .when(self.loaded && rows.is_empty(), |d| {
                d.child(empty_state("No skills installed.", cx))
            })
            .child(v_flex().gap_3().children(rows));
        page_body("skills-page", body)
    }
}
