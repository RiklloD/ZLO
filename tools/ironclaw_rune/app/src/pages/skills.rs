//! Skills: installed skills with auto-activation, their instructions (view
//! and edit), removal, and a catalog search to install more. Port of GPUI
//! `pages/skills.rs`.

pub mod fixtures;

use crate::pages::common::controls::{
    Look, Size, button_sized, header_with, icon_button_sized, input, outlined_tag, refresh_icon,
    rule, spinner,
};
use crate::pages::common::ts::{BODY_SEMIBOLD, SM_MEDIUM, XS};
use crate::pages::common::{Tone, card, empty_state, error_line, loading, notice_line, page_body};
use std::collections::HashMap;

use ironclaw_gpui_client::product_wire::{RebornSkillInfo, RebornSkillSourceKind};
use rune::{Cx, El, TextEdit, col, row, text};
use rune_kit::{Kit, Markdown, Switch};

use crate::design::kit::mono_editor;
use crate::icons;
use crate::jobs;
use crate::state::State;

/// One catalog hit. Catalog rows are untyped JSON on the wire.
pub struct CatalogSkill {
    pub name: String,
    pub description: String,
}

#[derive(Default)]
pub struct SkillsState {
    pub skills: Vec<RebornSkillInfo>,
    /// Open skills and their SKILL.md (`None` while it loads).
    pub open: HashMap<String, Option<String>>,
    /// SKILL.md editor: the skill's name; its text is `editor_text`.
    pub editor: Option<String>,
    pub editor_text: TextEdit,
    /// Global switch: skills activate on their own when relevant.
    pub auto_learned: Option<bool>,
    pub query: TextEdit,
    pub results: Option<Result<Vec<CatalogSkill>, String>>,
    pub searching: bool,
    pub busy: Option<String>,
    pub confirm_remove: Option<String>,
    pub loaded: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
}

fn st(s: &mut State) -> &mut SkillsState {
    &mut s.pages.skills
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "skills",
        |c| async move { c.skills().await },
        |s, result, _| {
            let p = st(s);
            match result {
                Ok(list) => {
                    p.skills = list.skills;
                    p.auto_learned = Some(list.auto_activate_learned);
                    p.error = None;
                }
                Err(e) => p.error = Some(e.to_string()),
            }
            p.loaded = true;
        },
    );
}

fn toggle_content(s: &mut State, name: String) {
    let p = st(s);
    if p.open.remove(&name).is_some() {
        return;
    }
    p.open.insert(name.clone(), None);
    let job_name = name.clone();
    jobs::run(
        s,
        "skills",
        move |c| async move { c.skill_content(&job_name).await },
        move |s, result, _| {
            let p = st(s);
            match result {
                Ok(skill) if p.open.contains_key(&name) => {
                    p.open.insert(name, Some(skill.content));
                }
                Ok(_) => {}
                Err(e) => {
                    p.open.remove(&name);
                    p.error = Some(e.to_string());
                }
            }
        },
    );
}

fn set_auto(s: &mut State, name: String, enabled: bool) {
    jobs::run(
        s,
        "skills",
        move |c| async move { c.set_skill_auto_activate(&name, enabled).await },
        |s, result, _| match result {
            // Re-list so the switch shows the stored value.
            Ok(_) => refresh(s),
            Err(e) => st(s).error = Some(e.to_string()),
        },
    );
}

fn set_auto_learned(s: &mut State, enabled: bool) {
    jobs::run(
        s,
        "skills",
        move |c| async move { c.set_auto_activate_learned(enabled).await },
        |s, result, _| match result {
            Ok(_) => refresh(s),
            Err(e) => st(s).error = Some(e.to_string()),
        },
    );
}

fn open_editor(s: &mut State, name: String) {
    let p = st(s);
    let Some(Some(content)) = p.open.get(&name) else {
        return;
    };
    p.editor_text = TextEdit::new(content.clone());
    p.editor_text.focused = true;
    p.editor = Some(name);
}

fn save_editor(s: &mut State) {
    let p = st(s);
    let Some(name) = p.editor.clone() else {
        return;
    };
    let content = p.editor_text.text.clone();
    p.busy = Some(name.clone());
    jobs::run(
        s,
        "skills",
        move |c| async move {
            c.update_skill(&name, &content).await?;
            // Read back what the gateway stored.
            c.skill_content(&name).await
        },
        |s, result, _| {
            let p = st(s);
            p.busy = None;
            match result {
                Ok(skill) => {
                    p.notice = Some(format!("Saved /{}.", skill.name));
                    p.error = None;
                    p.editor = None;
                    p.open.insert(skill.name, Some(skill.content));
                    refresh(s);
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn remove(s: &mut State, name: String) {
    let p = st(s);
    p.confirm_remove = None;
    p.busy = Some(name.clone());
    jobs::run(
        s,
        "skills",
        move |c| async move { c.remove_skill(&name).await },
        |s, result, _| {
            let p = st(s);
            p.busy = None;
            match result {
                Ok(resp) => {
                    p.notice = Some(resp.message);
                    refresh(s);
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn search(s: &mut State) {
    let p = st(s);
    let query = p.query.text.trim().to_string();
    if query.is_empty() || p.searching {
        return;
    }
    p.searching = true;
    p.error = None;
    jobs::run(
        s,
        "skills",
        move |c| async move { c.search_skills(&query).await },
        |s, result, _| {
            let p = st(s);
            p.searching = false;
            p.results = Some(match result {
                Ok(resp) => {
                    // Catalog hits first, then installed skills that match; the
                    // gateway searches installed skills even without a catalog.
                    let mut hits: Vec<CatalogSkill> = resp
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
                        .collect();
                    for x in resp.installed {
                        if !hits.iter().any(|h| h.name == x.name) {
                            hits.push(CatalogSkill {
                                name: x.name,
                                description: x.description,
                            });
                        }
                    }
                    match resp.catalog_error {
                        Some(e) if hits.is_empty() => Err(e),
                        // An empty registry URL means there is no online catalog.
                        None if hits.is_empty() && resp.registry_url.is_empty() => Err(
                            "No installed skill matches. No online skill catalog is connected."
                                .into(),
                        ),
                        _ => Ok(hits),
                    }
                }
                Err(e) => Err(e.to_string()),
            });
        },
    );
}

fn install(s: &mut State, name: String) {
    st(s).busy = Some(name.clone());
    jobs::run(
        s,
        "skills",
        move |c| async move { c.install_skill(&name).await },
        |s, result, _| {
            let p = st(s);
            p.busy = None;
            match result {
                Ok(resp) if resp.success => {
                    p.notice = Some(resp.message);
                    refresh(s);
                }
                Ok(resp) => p.error = Some(resp.message),
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn render_skill(s: &State, k: &Kit, ix: usize, x: &RebornSkillInfo) -> El {
    let p = &s.palette;
    let sk = &s.pages.skills;
    let source = match x.source_kind {
        RebornSkillSourceKind::System => "Built in",
        RebornSkillSourceKind::Installed => "Installed",
        RebornSkillSourceKind::User => "Yours",
        RebornSkillSourceKind::Workspace => "Workspace",
    };
    let open = sk.open.get(&x.name);
    let confirming = sk.confirm_remove.as_deref() == Some(x.name.as_str());
    let busy = sk.busy.as_deref() == Some(x.name.as_str());
    let editing = sk.editor.as_deref() == Some(x.name.as_str());

    let mut head = row()
        .w_full()
        .gap(8.)
        .center_y()
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(
                    text(format!("/{}", x.name))
                        .font(BODY_SEMIBOLD)
                        .color(p.text),
                )
                .child(
                    text(x.description.clone())
                        .font(XS)
                        .color(p.text_muted)
                        .wrap(),
                ),
        )
        .child(outlined_tag(p, Tone::Secondary, source));
    // Built-in skills are read-only; the gateway rejects the toggle for them.
    if x.can_edit {
        let (name, on) = (x.name.clone(), x.auto_activate);
        head = head.child(
            Switch::new(on)
                .label(format!("Auto-activate {}", x.name))
                .hide_label()
                .on_toggle(move |s: &mut State| set_auto(s, name.clone(), !on))
                .build(k)
                .help("Activate automatically when relevant")
                .key(format!("auto-{ix}")),
        );
        // The gateway serves skill content only for editable skills.
        let name = x.name.clone();
        head = head.child(
            button_sized(
                if open.is_some() { "Hide" } else { "View" },
                Look::Ghost,
                Size::Small,
            )
            .on_press(move |s: &mut State| toggle_content(s, name.clone()))
            .build(k)
            .key(format!("view-{ix}")),
        );
    }
    if x.can_edit && matches!(open, Some(Some(_))) && !editing {
        let name = x.name.clone();
        head = head.child(
            button_sized("Edit", Look::Ghost, Size::Small)
                .on_press(move |s: &mut State| open_editor(s, name.clone()))
                .build(k)
                .key(format!("edit-{ix}")),
        );
    }
    if x.can_delete {
        let name = x.name.clone();
        head = head.child(if confirming {
            button_sized("Remove?", Look::Danger, Size::Small)
                .disabled(busy)
                .on_press(move |s: &mut State| remove(s, name.clone()))
                .build(k)
                .key(format!("confirm-remove-skill-{ix}"))
        } else {
            icon_button_sized(&icons::DELETE, "Remove skill", Size::Small)
                .label(format!("Remove {}", x.name))
                .on_press(move |s: &mut State| st(s).confirm_remove = Some(name.clone()))
                .build(k)
                .key(format!("remove-skill-{ix}"))
        });
    }

    let mut c = card(p).key(format!("skill-{}", x.name)).child(head);
    if let Some(content) = open {
        c = c.child(match (content, editing) {
            (_, true) => col()
                .w_full()
                .gap(8.)
                .child(rule(p))
                .child(
                    mono_editor(&sk.editor_text)
                        .rows(8, 30)
                        .label(format!("/{} instructions", x.name))
                        .on_edit(|s: &mut State| &mut s.pages.skills.editor_text)
                        .build(k)
                        .key(format!("editor-{ix}")),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.)
                        .justify_end()
                        .child(
                            button_sized("Cancel", Look::Ghost, Size::Small)
                                .on_press(|s: &mut State| st(s).editor = None)
                                .build(k)
                                .key(format!("cancel-edit-{ix}")),
                        )
                        .child(
                            loading(
                                button_sized("Save", Look::Primary, Size::Small)
                                    .on_press(|s: &mut State| save_editor(s)),
                                busy,
                            )
                            .build(k)
                            .key(format!("save-edit-{ix}")),
                        ),
                ),
            (None, false) => spinner(k),
            (Some(body), false) => col().w_full().gap(8.).child(rule(p)).child(
                Markdown::new(body.clone())
                    .build(k)
                    .key(format!("skill-md-{ix}")),
            ),
        });
    }
    c
}

fn render_results(s: &State, k: &Kit) -> Option<El> {
    let p = &s.palette;
    let sk = &s.pages.skills;
    let r = sk.results.as_ref()?;
    Some(match r {
        Err(e) => error_line(p, e),
        Ok(list) if list.is_empty() => empty_state(p, "No matching skills."),
        Ok(list) => col()
            .w_full()
            .children(list.iter().enumerate().map(|(ix, hit)| {
                let have = sk.skills.iter().any(|x| x.name == hit.name);
                let name = hit.name.clone();
                let busy = sk.busy.as_deref() == Some(hit.name.as_str());
                col()
                    .key(format!("hit-{}", hit.name))
                    .w_full()
                    .child(
                        row()
                            .w_full()
                            .gap(12.)
                            .py(8.)
                            .center_y()
                            .child(
                                col()
                                    .grow(1.)
                                    .min_w(0.)
                                    .child(text(hit.name.clone()).font(SM_MEDIUM).color(p.text))
                                    .child(
                                        text(hit.description.clone())
                                            .font(XS)
                                            .color(p.text_muted)
                                            .wrap(),
                                    ),
                            )
                            .child(if have {
                                outlined_tag(p, Tone::Success, "Installed")
                            } else {
                                loading(
                                    button_sized("Install", Look::Outline, Size::Small)
                                        .on_press(move |s: &mut State| install(s, name.clone())),
                                    busy,
                                )
                                .build(k)
                                .key(format!("install-skill-{ix}"))
                            }),
                    )
                    .child(rule(p))
            })),
    })
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let sk = &s.pages.skills;
    let actions = vec![
        refresh_icon()
            .on_press(|s: &mut State| refresh(s))
            .build(k)
            .key("refresh-skills"),
    ];
    let mut body = col().w_full().child(header_with(p, "Skills", actions));
    if let Some(e) = &sk.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &sk.notice {
        body = body.child(notice_line(n, p.text_muted));
    }
    if let Some(on) = sk.auto_learned {
        body = body.child(
            row()
                .w_full()
                .gap(12.)
                .pb(12.)
                .center_y()
                .child(
                    col()
                        .grow(1.)
                        .child(text("Auto-activate skills").font(SM_MEDIUM).color(p.text))
                        .child(
                            text(if on {
                                "Skills turn on by themselves when a message matches them."
                            } else {
                                "Skills only run when you type /name in a message."
                            })
                            .font(XS)
                            .color(p.text_muted)
                            .wrap(),
                        ),
                )
                .child(
                    Switch::new(on)
                        .label("Auto-activate skills")
                        .hide_label()
                        .on_toggle(move |s: &mut State| set_auto_learned(s, !on))
                        .build(k)
                        .key("auto-learned"),
                ),
        );
    }
    body = body.child(
        row()
            .w_full()
            .gap(8.)
            .pb(12.)
            .center_y()
            .child(
                col().grow(1.).min_w(0.).child(
                    input(&sk.query)
                        .placeholder("Search skills")
                        .on_edit(|s: &mut State| &mut s.pages.skills.query)
                        .on_submit(|s: &mut State, _| search(s))
                        .build(k)
                        .key("skills-query"),
                ),
            )
            .child(
                loading(
                    button_sized("Search", Look::Outline, Size::Small)
                        .icon(&icons::SEARCH)
                        .on_press(|s: &mut State| search(s)),
                    sk.searching,
                )
                .build(k)
                .key("search-skills"),
            ),
    );
    if let Some(r) = render_results(s, k) {
        body = body.child(col().w_full().pb(24.).child(r));
    }
    if sk.loaded && sk.skills.is_empty() {
        body = body.child(empty_state(p, "No skills installed."));
    }
    body = body.child(
        col().w_full().gap(12.).children(
            sk.skills
                .iter()
                .enumerate()
                .map(|(ix, x)| render_skill(s, k, ix, x)),
        ),
    );
    page_body("skills-page", body)
}
