//! Projects: list, create, archive, restore and delete. Shown only when the
//! gateway reports `features.reborn_projects`. Port of GPUI
//! `pages/projects.rs`.

use ironclaw_gpui_client::workspace_views::{RebornProjectInfo, RebornProjectState};
use rune::{Cx, El, TextEdit, col, row};
use rune_kit::{Kit, TextArea, TextInput};

use super::common::{
    BASE, Btn, SM, Tone, XS, actions, button, card, empty_state, error_line, glyph, header,
    icon_button, label, loading, page_body, para, refresh_button, tag,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

#[derive(Default)]
pub struct ProjectsState {
    pub items: Vec<RebornProjectInfo>,
    pub show_archived: bool,
    pub name: TextEdit,
    pub description: TextEdit,
    pub creating: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub confirm_delete: Option<String>,
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "projects",
        |c| async move { c.list_projects().await },
        |s, result, _| {
            let this = &mut s.pages.projects;
            match result {
                Ok(page) => {
                    this.items = page.projects;
                    this.error = None;
                }
                Err(e) => this.error = Some(e.to_string()),
            }
            this.loaded = true;
        },
    );
}

fn create(s: &mut State) {
    let this = &mut s.pages.projects;
    let name = this.name.text.trim().to_string();
    let description = this.description.text.trim().to_string();
    if name.is_empty() || this.creating {
        return;
    }
    this.creating = true;
    // Clear only after the gateway confirmed, so a failure keeps the text.
    jobs::run(
        s,
        "projects",
        move |c| async move { c.create_project(&name, &description).await },
        |s, result, _| {
            let this = &mut s.pages.projects;
            this.creating = false;
            match result {
                Ok(created) => {
                    this.items.insert(0, created.project);
                    this.error = None;
                    this.name.clear();
                    this.description.clear();
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn set_state(s: &mut State, id: String, state: RebornProjectState) {
    jobs::run(
        s,
        "projects",
        move |c| async move { c.set_project_state(&id, state).await },
        |s, result, _| {
            let this = &mut s.pages.projects;
            match result {
                Ok(updated) => {
                    let project = updated.project;
                    if let Some(row) = this
                        .items
                        .iter_mut()
                        .find(|p| p.project_id == project.project_id)
                    {
                        *row = project;
                    }
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn delete(s: &mut State, id: String) {
    s.pages.projects.confirm_delete = None;
    let job_id = id.clone();
    jobs::run(
        s,
        "projects",
        move |c| async move { c.delete_project(&job_id).await },
        move |s, result, _| {
            let this = &mut s.pages.projects;
            match result {
                Ok(()) => this.items.retain(|p| p.project_id != id),
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn render_row(s: &State, k: &Kit, pr: &RebornProjectInfo) -> El {
    let p = &s.palette;
    let archived = matches!(pr.state, RebornProjectState::Archived);
    let id = pr.project_id.clone();
    let (icon, tip, next) = if archived {
        (&icons::UNDO, "Restore", RebornProjectState::Active)
    } else {
        (&icons::INBOX, "Archive", RebornProjectState::Archived)
    };
    let mut head = row()
        .gap(8.)
        .center_y()
        .child(glyph(&icons::FOLDER_OPEN, 16., p.text))
        .child(
            label(pr.name.clone(), BASE, 600., p.text)
                .truncate()
                .grow(1.0)
                .min_w(0.),
        );
    if archived {
        head = head.child(tag(p, "Archived", Tone::Secondary));
    }
    let state_button = {
        let id = id.clone();
        icon_button(icon, tip)
            .on_press(move |s: &mut State| set_state(s, id.clone(), next))
            .build(k)
    };
    let delete_button = if s.pages.projects.confirm_delete.as_deref() == Some(id.as_str()) {
        let id = id.clone();
        button("Delete?", Btn::Danger)
            .on_press(move |s: &mut State| delete(s, id.clone()))
            .build(k)
    } else {
        let id = id.clone();
        icon_button(&icons::DELETE, "Delete project")
            .on_press(move |s: &mut State| s.pages.projects.confirm_delete = Some(id.clone()))
            .build(k)
    };
    head = head.child(actions([state_button, delete_button]));
    let mut c = card(p).key(format!("project-{id}")).child(head);
    if !pr.description.trim().is_empty() {
        c = c.child(para(pr.description.clone(), SM, p.text));
    }
    c.child(label(
        format!("Created {}", jobs::when(pr.created_at)),
        XS,
        400.,
        p.text_muted,
    ))
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let this = &s.pages.projects;
    let archived_count = this
        .items
        .iter()
        .filter(|pr| matches!(pr.state, RebornProjectState::Archived))
        .count();
    let mut head = Vec::new();
    if archived_count > 0 {
        let title = if this.show_archived {
            "Hide archived".to_string()
        } else {
            format!("Show archived ({archived_count})")
        };
        head.push(
            button(&title, Btn::Ghost)
                .on_press(|s: &mut State| {
                    s.pages.projects.show_archived = !s.pages.projects.show_archived
                })
                .build(k),
        );
    }
    head.push(refresh_button(k, refresh));

    let form = card(p)
        .key("new-project")
        .child(label("New project", SM, 600., p.text))
        .child(
            TextInput::new(&this.name)
                .placeholder("Project name")
                .width(None)
                .on_edit(|s: &mut State| &mut s.pages.projects.name)
                // Enter creates (GPUI `submit_on_enter`).
                .on_submit(|s: &mut State, _| create(s))
                .build(k),
        )
        .child(
            TextArea::new(&this.description)
                .placeholder("What is it for? (optional)")
                .auto_grow(1, 4)
                .width(None)
                .on_edit(|s: &mut State| &mut s.pages.projects.description)
                .build(k),
        )
        .child(
            row().w_full().justify_end().child(
                loading(button("Create", Btn::Primary), this.creating)
                    .on_press(create)
                    .build(k),
            ),
        );

    let show_archived = this.show_archived;
    let rows: Vec<El> = this
        .items
        .iter()
        .filter(|pr| show_archived || !matches!(pr.state, RebornProjectState::Archived))
        .map(|pr| render_row(s, k, pr))
        .collect();

    let mut body = col().w_full().child(header(p, "Projects", actions(head)));
    if let Some(e) = &this.error {
        body = body.child(error_line(p, e));
    }
    body = body.child(form);
    if this.loaded && rows.is_empty() {
        body = body.child(empty_state(p, "No projects yet."));
    }
    body = body.child(col().w_full().pt(16.).gap(12.).children(rows));
    page_body("projects-page", body)
}

pub fn fixture(s: &mut State) {
    let now = chrono::Utc::now();
    let project = |id: &str, name: &str, desc: &str, state: &str, days: i64| {
        serde_json::json!({
            "project_id": id, "name": name, "description": desc, "metadata": {},
            "state": state, "role": "owner",
            "created_at": now - chrono::Duration::days(days),
            "updated_at": now - chrono::Duration::days(days),
        })
    };
    let items = serde_json::from_value::<Vec<RebornProjectInfo>>(serde_json::json!([
        project(
            "p1",
            "ZLO on Rune",
            "Port the desktop app from GPUI to Rune, file by file.",
            "active",
            1
        ),
        project("p2", "Tax return 2026", "", "active", 9),
        project("p3", "Old blog", "Static site, retired.", "archived", 120),
    ]));
    let this = &mut s.pages.projects;
    match items {
        Ok(items) => this.items = items,
        Err(e) => tracing::warn!(error = %e, "projects fixture"),
    }
    this.loaded = true;
}
