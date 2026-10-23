//! Projects: list, create, archive, restore and delete. Shown only when the gateway
//! reports `features.reborn_projects`.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::workspace_views::{RebornProjectInfo, RebornProjectState};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

pub struct ProjectsView {
    backend: Backend,
    items: Vec<RebornProjectInfo>,
    show_archived: bool,
    name: Entity<TextareaState>,
    description: Entity<TextareaState>,
    creating: bool,
    loaded: bool,
    error: Option<SharedString>,
    confirm_delete: Option<String>,
    _name_events: Subscription,
}

impl EventEmitter<PageEvent> for ProjectsView {}

impl ProjectsView {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 1)
                .placeholder("Project name")
                .submit_on_enter(true)
        });
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 4)
                .placeholder("What is it for? (optional)")
        });
        let name_events = cx.subscribe_in(&name, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.create(window, cx);
            }
        });
        let mut this = Self {
            backend,
            items: Vec::new(),
            show_archived: false,
            name,
            description,
            creating: false,
            loaded: false,
            error: None,
            confirm_delete: None,
            _name_events: name_events,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.list_projects().await },
            |this, result, cx| {
                match result {
                    Ok(page) => {
                        this.items = page.projects;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let description = self.description.read(cx).value().trim().to_string();
        if name.is_empty() || self.creating {
            return;
        }
        self.creating = true;
        cx.notify();
        // Clear only after the gateway confirmed, so a failure keeps the text.
        let (name_input, description_input) = (self.name.clone(), self.description.clone());
        let window_handle = window.window_handle();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.create_project(&name, &description).await },
            move |this, result, cx| {
                this.creating = false;
                match result {
                    Ok(created) => {
                        this.items.insert(0, created.project);
                        this.error = None;
                        let _ = window_handle.update(cx, |_, window, cx| {
                            name_input.update(cx, |s, cx| s.set_value("", window, cx));
                            description_input.update(cx, |s, cx| s.set_value("", window, cx));
                        });
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn set_state(&mut self, id: String, state: RebornProjectState, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_project_state(&id, state).await },
            |this, result, cx| match result {
                Ok(updated) => {
                    let project = updated.project;
                    if let Some(row) = this
                        .items
                        .iter_mut()
                        .find(|p| p.project_id == project.project_id)
                    {
                        *row = project;
                    }
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(e.to_string().into());
                    cx.notify();
                }
            },
        );
    }

    fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.delete_project(&job_id).await },
            move |this, result, cx| {
                match result {
                    Ok(()) => this.items.retain(|p| p.project_id != id),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_row(&self, ix: usize, p: &RebornProjectInfo, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let archived = matches!(p.state, RebornProjectState::Archived);
        let id = p.project_id.clone();
        let (icon, tip, next) = if archived {
            (IconName::Undo, "Restore", RebornProjectState::Active)
        } else {
            (IconName::Inbox, "Archive", RebornProjectState::Archived)
        };
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::FolderOpen).small())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .child(p.name.clone()),
                    )
                    .when(archived, |d| {
                        d.child(Tag::secondary().small().child("Archived"))
                    })
                    .child({
                        let id = id.clone();
                        Button::new(("project-state", ix))
                            .icon(icon)
                            .ghost()
                            .small()
                            .tooltip(tip)
                            .accessibility_label(tip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_state(id.clone(), next, cx)
                            }))
                    })
                    .child(if self.confirm_delete.as_deref() == Some(id.as_str()) {
                        Button::new(("project-delete-confirm", ix))
                            .label("Delete?")
                            .danger()
                            .small()
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.delete(id.clone(), cx)),
                            )
                    } else {
                        Button::new(("project-delete", ix))
                            .icon(IconName::Delete)
                            .ghost()
                            .small()
                            .tooltip("Delete project")
                            .accessibility_label("Delete project")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.confirm_delete = Some(id.clone());
                                cx.notify();
                            }))
                    }),
            )
            .when(!p.description.trim().is_empty(), |d| {
                d.child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .child(p.description.clone()),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Created {}", jobs::when(p.created_at))),
            )
            .into_any_element()
    }
}

impl Render for ProjectsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let archived_count = self
            .items
            .iter()
            .filter(|p| matches!(p.state, RebornProjectState::Archived))
            .count();
        let actions = h_flex()
            .gap_1()
            .when(archived_count > 0, |this| {
                let label = if self.show_archived {
                    "Hide archived".to_string()
                } else {
                    format!("Show archived ({archived_count})")
                };
                this.child(
                    Button::new("toggle-archived")
                        .label(label)
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_archived = !this.show_archived;
                            cx.notify();
                        })),
                )
            })
            .child(
                refresh_button("refresh-projects")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );

        let form = card(cx)
            .child(div().text_sm().font_semibold().child("New project"))
            .child(Textarea::new(&self.name))
            .child(Textarea::new(&self.description))
            .child(
                h_flex().justify_end().child(
                    Button::new("create-project")
                        .label("Create")
                        .primary()
                        .small()
                        .loading(self.creating)
                        .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                ),
            );

        let show_archived = self.show_archived;
        let rows: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, p)| show_archived || !matches!(p.state, RebornProjectState::Archived))
            .map(|(ix, p)| self.render_row(ix, p, cx))
            .collect();

        let body = v_flex()
            .child(header("Projects", actions))
            .when_some(self.error.as_ref(), |this, e| this.child(error_line(e, cx)))
            .child(form)
            .when(self.loaded && rows.is_empty(), |this| {
                this.child(empty_state("No projects yet.", cx))
            })
            .child(v_flex().pt_4().gap_3().children(rows));
        page_body("projects-page", body)
    }
}
