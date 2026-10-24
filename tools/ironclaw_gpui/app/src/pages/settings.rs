//! Settings → General: appearance, the desktop app, Trace Commons and what
//! tools may do without asking. Models and provider logins live on the
//! Providers page.

mod appearance;
mod traces;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::SettingsToolPermissionState;
use ironclaw_gpui_client::wire::ToolPermission;

use super::{PageEvent, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

const AUTO_APPROVE_KEY: &str = "agent.auto_approve_tools";
const TOOL_KEY_PREFIX: &str = "tool.";

const STATES: [(SettingsToolPermissionState, &str); 4] = [
    (SettingsToolPermissionState::Default, "Default"),
    (SettingsToolPermissionState::AlwaysAllow, "Allow"),
    (SettingsToolPermissionState::AskEachTime, "Ask"),
    (SettingsToolPermissionState::Disabled, "Off"),
];

pub struct SettingsView {
    backend: Backend,
    auto_approve: Option<bool>,
    tools: Vec<(String, ToolPermission)>,
    traces: Entity<traces::TracePanel>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl EventEmitter<PageEvent> for SettingsView {}

impl SettingsView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend: backend.clone(),
            auto_approve: None,
            tools: Vec::new(),
            traces: cx.new(|cx| traces::TracePanel::new(backend.clone(), cx)),
            error: None,
            notice: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.traces.update(cx, |t, cx| t.refresh(cx));
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.tool_settings().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        let mut tools = Vec::new();
                        for entry in list.entries {
                            if entry.key == AUTO_APPROVE_KEY {
                                this.auto_approve = entry.value.as_bool();
                            } else if let Some(id) = entry.key.strip_prefix(TOOL_KEY_PREFIX) {
                                // silent-ok: a row the gateway shapes differently is skipped, not fatal.
                                if let Ok(tool) =
                                    serde_json::from_value::<ToolPermission>(entry.value)
                                {
                                    tools.push((id.to_string(), tool));
                                }
                            }
                        }
                        tools.sort_by(|a, b| a.0.cmp(&b.0));
                        this.tools = tools;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn fail(&mut self, e: impl ToString, cx: &mut Context<Self>) {
        self.error = Some(e.to_string().into());
        cx.notify();
    }

    fn set_auto_approve(&mut self, enabled: bool, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_auto_approve(enabled).await },
            |this, result, cx| match result {
                // Show the stored value the gateway read back, not the click.
                Ok(resp) => {
                    this.auto_approve = resp.entry.value.as_bool();
                    this.error = None;
                    // Tools without their own setting follow the switch.
                    this.refresh(cx);
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    fn set_tool(&mut self, id: String, state: SettingsToolPermissionState, cx: &mut Context<Self>) {
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_tool_permission(&job_id, state).await },
            move |this, result, cx| match result {
                Ok(resp) => {
                    if let Ok(tool) = serde_json::from_value::<ToolPermission>(resp.entry.value)
                        && let Some(row) = this.tools.iter_mut().find(|(k, _)| *k == id)
                    {
                        row.1 = tool;
                    }
                    this.error = None;
                    cx.notify();
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    fn render_desktop(&self, cx: &Context<Self>) -> impl IntoElement {
        let bundled = crate::login_item::bundle_path().is_some();
        card(cx).child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_semibold().child("Open at login"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .whitespace_normal()
                                .child(if bundled {
                                    "Starts IronClaw Desktop when you log in to your Mac."
                                } else {
                                    "Available when IronClaw runs from IronClaw.app (tools/ironclaw_gpui/bundle.sh)."
                                }),
                        ),
                )
                .child(
                    Switch::new("open-at-login")
                        .checked(crate::login_item::is_enabled())
                        .disabled(!bundled)
                        .accessibility_label("Open at login")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            match crate::login_item::set_enabled(*checked) {
                                Ok(()) => this.error = None,
                                Err(e) => this.error = Some(e.into()),
                            }
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_tools(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let auto = self.auto_approve.unwrap_or(false);
        let toggle = card(cx).child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_semibold().child("Approve tools automatically"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(if auto { theme.warning } else { theme.muted_foreground })
                                .whitespace_normal()
                                .child(if auto {
                                    "On: shell commands, file writes and network calls run without asking you. Tools set to Ask below still ask."
                                } else {
                                    "Off: IronClaw asks before shell commands, file writes and network calls, unless a tool is set to Allow."
                                }),
                        ),
                )
                .child(
                    Switch::new("auto-approve")
                        .checked(auto)
                        .disabled(self.auto_approve.is_none())
                        .accessibility_label("Approve tools automatically")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_auto_approve(*checked, cx)
                        })),
                ),
        );

        let rows = self.tools.iter().enumerate().map(|(ix, (id, tool))| {
            let segments = h_flex().gap_0p5().children(STATES.iter().enumerate().map(
                |(six, (state, label))| {
                    let selected = tool.state == *state;
                    let id = id.clone();
                    let state = *state;
                    Button::new(("tool-state", ix * 10 + six))
                        .label(*label)
                        .xsmall()
                        .when(selected, |b| b.primary())
                        .when(!selected, |b| b.ghost())
                        .disabled(tool.locked)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_tool(id.clone(), state, cx)),
                        )
                },
            ));
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
                        .child(div().text_sm().font_medium().child(tool.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(tool.description.clone()),
                        ),
                )
                .when(tool.locked, |d| {
                    d.child(Tag::secondary().small().child("Locked"))
                })
                .when(!tool.locked && !tool.has_override(), |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("follows switch"),
                    )
                })
                .child(segments)
        });

        v_flex()
            .gap_3()
            .child(toggle)
            .child(v_flex().children(rows))
    }
}

fn section_title(text: &'static str) -> impl IntoElement {
    div().pt_6().pb_2().font_semibold().child(text)
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = refresh_button("refresh-settings")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let body = v_flex()
            .child(header("Settings", actions))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.clone(), |d, n| {
                d.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(cx.theme().success)
                        .child(n),
                )
            })
            .child(section_title("Appearance"))
            .child(appearance::render(cx))
            .child(section_title("Desktop app"))
            .child(self.render_desktop(cx))
            .child(section_title("Trace Commons"))
            .child(self.traces.clone())
            .child(section_title("Tool permissions"))
            .child(self.render_tools(cx));
        page_body("settings-page", body)
    }
}
