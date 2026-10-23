//! Deployment credentials extensions declare for the operator (Slack bot
//! token, Telegram bot, Google OAuth client). One form per group; a blank
//! secret keeps the stored value.

use ironclaw_gpui_client::operator::AdminConfigGroup;
use ironclaw_gpui_client::{Backend, ClientError};

use super::muted;
use crate::jobs;
use crate::pages::{empty_state, error_line};
use crate::ui::*;

/// One group with an input per field, in field order.
struct GroupForm {
    group: AdminConfigGroup,
    inputs: Vec<Entity<InputState>>,
    saving: bool,
    saved: bool,
}

pub struct ExtensionConfigPanel {
    backend: Backend,
    groups: Vec<GroupForm>,
    loaded: bool,
    error: Option<SharedString>,
}

impl ExtensionConfigPanel {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            groups: Vec::new(),
            loaded: false,
            error: None,
        };
        this.load(window, cx);
        this
    }

    /// The panel shell refreshes without a window; inputs need one, so the
    /// reload goes through the next frame.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            if let Some(window) = cx.active_window() {
                // silent-ok: the window closed between the click and this frame.
                let _ = window.update(cx, |_, window, cx| {
                    let _ = this.update(cx, |p, cx| p.load(window, cx));
                });
            }
        });
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        jobs::run_in(
            &self.backend,
            window,
            cx,
            |c| async move { c.extension_configuration().await },
            |this, result, window, cx| {
                match result {
                    Ok(list) => {
                        this.groups = list
                            .groups
                            .into_iter()
                            .map(|group| form(group, window, cx))
                            .collect();
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn save(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.groups.get_mut(ix) else {
            return;
        };
        let values: Vec<(String, String)> = g
            .group
            .fields
            .iter()
            .zip(&g.inputs)
            .map(|(f, input)| (f.handle.clone(), input.read(cx).value().trim().to_string()))
            .collect();
        // Clear typed secrets; the request owns them now.
        for (f, input) in g.group.fields.iter().zip(&g.inputs) {
            if f.secret {
                input.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        g.saving = true;
        g.saved = false;
        let (id, revision) = (g.group.group_id.clone(), g.group.revision);
        cx.notify();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move {
                c.replace_extension_configuration(&id, revision, values)
                    .await?;
                // Read back what the gateway stored.
                c.extension_configuration().await.map(|l| (id, l))
            },
            move |this, result, window, cx| {
                match result {
                    Ok((id, list)) => {
                        this.groups = list
                            .groups
                            .into_iter()
                            .map(|group| {
                                let saved = group.group_id == id;
                                let mut f = form(group, window, cx);
                                f.saved = saved;
                                f
                            })
                            .collect();
                        this.error = None;
                    }
                    Err(e) => {
                        if let Some(g) = this.groups.get_mut(ix) {
                            g.saving = false;
                        }
                        let conflict = matches!(e, ClientError::Api { status: 409, .. });
                        let msg = if conflict {
                            "Someone saved this group in the meantime. Refresh and try again."
                                .to_string()
                        } else {
                            e.to_string()
                        };
                        this.error = Some(msg.into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn render_group(&self, ix: usize, g: &GroupForm, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let used_by = g
            .group
            .used_by
            .iter()
            .map(|u| {
                if u.installed {
                    format!("{} (installed)", u.display_name)
                } else {
                    u.display_name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_semibold()
                            .child(g.group.display_name.clone()),
                    )
                    .child(if g.group.complete {
                        Tag::success().small().child("Configured")
                    } else {
                        Tag::warning().small().child("Needs values")
                    }),
            )
            .when(!g.group.description.is_empty(), |d| {
                d.child(muted(g.group.description.clone(), cx))
            })
            .when(!used_by.is_empty(), |d| {
                d.child(muted(format!("Used by: {used_by}"), cx))
            })
            .children(g.group.fields.iter().zip(&g.inputs).map(|(f, input)| {
                v_flex()
                    .gap_0p5()
                    .child(div().text_xs().font_medium().child(format!(
                        "{}{}",
                        f.label,
                        if f.required { " *" } else { "" }
                    )))
                    .child(Input::new(input).small())
                    .when(!f.description.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(f.description.clone()),
                        )
                    })
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .items_center()
                    .when(g.saved, |d| d.child(muted("Saved.", cx)))
                    .child(
                        Button::new(("save-ext-config", ix))
                            .label("Save")
                            .primary()
                            .small()
                            .loading(g.saving)
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.save(ix, window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn form(
    group: AdminConfigGroup,
    window: &mut Window,
    cx: &mut Context<ExtensionConfigPanel>,
) -> GroupForm {
    let inputs = group
        .fields
        .iter()
        .map(|f| {
            let (secret, provided, value) = (f.secret, f.provided, f.value.clone());
            cx.new(|cx| {
                let state = InputState::new(window, cx).masked(secret);
                match (secret, provided, value) {
                    (true, true, _) => state.placeholder("Saved. Leave blank to keep it."),
                    (false, _, Some(v)) => state.default_value(v),
                    _ => state,
                }
            })
        })
        .collect();
    GroupForm {
        group,
        inputs,
        saving: false,
        saved: false,
    }
}

impl Render for ExtensionConfigPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups: Vec<AnyElement> = self
            .groups
            .iter()
            .enumerate()
            .map(|(ix, g)| self.render_group(ix, g, cx))
            .collect();
        v_flex()
            .gap_3()
            .child(muted(
                "Credentials extensions need from the operator before anyone can connect them. \
                 Each person still connects their own account on the Extensions page.",
                cx,
            ))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when(self.loaded && groups.is_empty(), |d| {
                d.child(empty_state(
                    "No extension declares deployment settings.",
                    cx,
                ))
            })
            .children(groups)
    }
}
