//! The app's own settings, one topic per sidebar page (Zeron's layout):
//! General (the desktop app), Appearance, Permissions (what tools may do
//! without asking) and Trace Commons. Models and provider logins live on
//! the Providers page.

mod appearance;
mod traces;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::SettingsToolPermissionState;
use ironclaw_gpui_client::wire::ToolPermission;

use super::{
    PageEvent, error_line, group_header, header, page_body, refresh_button, rows_card, setting_row,
};
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

/// Which settings page this view is showing.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    General,
    Appearance,
    Permissions,
    Traces,
}

pub struct SettingsView {
    backend: Backend,
    section: Section,
    auto_approve: Option<bool>,
    tools: Vec<(String, ToolPermission)>,
    traces: Entity<traces::TracePanel>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// The Appearance select whose menu is open.
    pub(super) appearance_menu: Option<&'static str>,
}

impl EventEmitter<PageEvent> for SettingsView {}

impl SettingsView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend: backend.clone(),
            section: Section::General,
            auto_approve: None,
            tools: Vec::new(),
            traces: cx.new(|cx| traces::TracePanel::new(backend.clone(), cx)),
            error: None,
            notice: None,
            appearance_menu: None,
        };
        this.refresh(cx);
        this
    }

    pub fn set_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            cx.notify();
        }
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
        let login = Switch::new("open-at-login")
            .checked(crate::login_item::is_enabled())
            .disabled(!bundled)
            .accessibility_label("Open at login")
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                match crate::login_item::set_enabled(*checked) {
                    Ok(()) => this.error = None,
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            }));
        let status_bar = Switch::new("status-bar")
            .checked(!crate::design::prefs::Prefs::of(cx).status_bar_hidden)
            .accessibility_label("Show the status bar")
            .on_click(|_, _, cx| crate::workspace::toggle_status_bar(cx));
        rows_card(
            [
                setting_row(
                    "Open at login",
                    Some(
                        if bundled {
                            "Starts IronClaw Desktop when you log in to your Mac."
                        } else {
                            "Available when IronClaw runs from IronClaw.app (tools/ironclaw_gpui/bundle.sh)."
                        }
                        .into(),
                    ),
                    login,
                    cx,
                ),
                setting_row(
                    "Status bar",
                    Some("Connection, model and token use along the bottom of the window.".into()),
                    status_bar,
                    cx,
                ),
            ],
            cx,
        )
    }

    fn render_tools(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let auto = self.auto_approve.unwrap_or(false);
        let switch = Switch::new("auto-approve")
            .checked(auto)
            .disabled(self.auto_approve.is_none())
            .accessibility_label("Approve tools automatically")
            .on_click(
                cx.listener(|this, checked: &bool, _, cx| this.set_auto_approve(*checked, cx)),
            );
        let toggle = rows_card(
            [setting_row(
                "Approve tools automatically",
                Some(
                    if auto {
                        "On: shell commands, file writes and network calls run without asking you. Tools set to Ask below still ask."
                    } else {
                        "Off: IronClaw asks before shell commands, file writes and network calls, unless a tool is set to Allow."
                    }
                    .into(),
                ),
                switch,
                cx,
            )],
            cx,
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
            let control = h_flex()
                .gap_2()
                .items_center()
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
                .child(segments);
            setting_row(
                tool.name.clone(),
                (!tool.description.is_empty()).then(|| tool.description.clone().into()),
                control,
                cx,
            )
        });

        v_flex().child(toggle).when(!self.tools.is_empty(), |d| {
            d.child(group_header(
                "Per tool",
                Some("Default follows the switch above. Allow and Ask override it for one tool."),
                cx,
            ))
            .child(rows_card(rows.collect::<Vec<_>>(), cx))
        })
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = refresh_button("refresh-settings")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let title = match self.section {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Permissions => "Permissions",
            Section::Traces => "Trace Commons",
        };
        let body = v_flex()
            .child(header(title, actions))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.clone(), |d, n| {
                d.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(cx.theme().success)
                        .child(n),
                )
            });
        let body = match self.section {
            Section::General => body
                .child(group_header("Desktop app", None, cx))
                .child(self.render_desktop(cx)),
            Section::Appearance => body.child(appearance::render(self.appearance_menu, cx)),
            Section::Permissions => body.child(self.render_tools(cx)),
            Section::Traces => body
                .child(group_header(
                    "Contributions",
                    Some("Traces you contributed and the credits they earned."),
                    cx,
                ))
                .child(rows_card(
                    [div().p_4().child(self.traces.clone()).into_any_element()],
                    cx,
                )),
        };
        page_body("settings-page", body)
    }
}
