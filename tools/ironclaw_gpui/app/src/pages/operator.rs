//! Operator: gateway readiness, the OS service, first-run setup, config keys,
//! the log ring, and the run inspector. Every route behind this page is
//! operator-gated; the local single-user token has that role.

mod config;
mod inspector;
mod logs;
mod setup;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::{
    RebornOperatorCommandPlaneResponse, RebornOperatorConfigDiagnostic,
    RebornOperatorConfigDiagnosticSeverity, RebornOperatorServiceLifecycleAction,
    RebornOperatorStatusCheck, RebornOperatorStatusState, RebornServiceLifecycleResponse,
    RebornServiceLifecycleState,
};

use self::config::ConfigPanel;
use self::inspector::InspectorPanel;
use self::logs::LogsPanel;
use self::setup::SetupPanel;
use super::{PageEvent, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Status,
    Setup,
    Config,
    Logs,
    Inspector,
}

const TABS: [(Tab, &str); 5] = [
    (Tab::Status, "Status"),
    (Tab::Setup, "Setup"),
    (Tab::Config, "Config"),
    (Tab::Logs, "Logs"),
    (Tab::Inspector, "Inspector"),
];

/// Page shell: a tab row over panels that are built on first visit.
pub struct OperatorView {
    backend: Backend,
    tab: Tab,
    status: Entity<StatusPanel>,
    setup: Option<Entity<SetupPanel>>,
    config: Option<Entity<ConfigPanel>>,
    logs: Option<Entity<LogsPanel>>,
    inspector: Option<Entity<InspectorPanel>>,
    _events: Vec<Subscription>,
}

impl EventEmitter<PageEvent> for OperatorView {}

impl OperatorView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let status = cx.new(|cx| StatusPanel::new(backend.clone(), cx));
        Self {
            backend,
            tab: Tab::Status,
            status,
            setup: None,
            config: None,
            logs: None,
            inspector: None,
            _events: Vec::new(),
        }
    }

    /// Reload the visible tab only; the others reload when shown.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.tab {
            Tab::Status => self.status.update(cx, |p, cx| p.refresh(cx)),
            Tab::Setup => update(&self.setup, cx, |p, cx| p.refresh(cx)),
            Tab::Config => update(&self.config, cx, |p, cx| p.refresh(cx)),
            Tab::Logs => update(&self.logs, cx, |p, cx| p.refresh(cx)),
            Tab::Inspector => update(&self.inspector, cx, |p, cx| p.refresh(cx)),
        }
    }

    fn show(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let fresh = match tab {
            Tab::Status => false,
            Tab::Setup => build(&mut self.setup, || {
                cx.new(|cx| SetupPanel::new(backend, window, cx))
            }),
            Tab::Config => build(&mut self.config, || {
                cx.new(|cx| ConfigPanel::new(backend, window, cx))
            }),
            Tab::Logs => build(&mut self.logs, || {
                cx.new(|cx| LogsPanel::new(backend, window, cx))
            }),
            Tab::Inspector => {
                let fresh = build(&mut self.inspector, || {
                    cx.new(|cx| InspectorPanel::new(backend, cx))
                });
                if fresh && let Some(panel) = self.inspector.clone() {
                    // The inspector can open a run's conversation.
                    self._events
                        .push(cx.subscribe(&panel, |_, _, e: &PageEvent, cx| {
                            if let PageEvent::OpenThread(id) = e {
                                cx.emit(PageEvent::OpenThread(id.clone()));
                            }
                        }));
                }
                fresh
            }
        };
        self.tab = tab;
        if !fresh {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn panel(&self) -> Option<AnyElement> {
        match self.tab {
            Tab::Status => Some(self.status.clone().into_any_element()),
            Tab::Setup => self.setup.clone().map(|v| v.into_any_element()),
            Tab::Config => self.config.clone().map(|v| v.into_any_element()),
            Tab::Logs => self.logs.clone().map(|v| v.into_any_element()),
            Tab::Inspector => self.inspector.clone().map(|v| v.into_any_element()),
        }
    }
}

/// Build a panel into `slot` if it is empty. Returns true when it was built
/// (a new panel loads itself, so the caller skips the refresh).
fn build<V>(slot: &mut Option<Entity<V>>, make: impl FnOnce() -> Entity<V>) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(make());
    true
}

fn update<V: 'static>(
    slot: &Option<Entity<V>>,
    cx: &mut Context<OperatorView>,
    f: impl FnOnce(&mut V, &mut Context<V>),
) {
    if let Some(panel) = slot {
        panel.update(cx, f);
    }
}

impl Render for OperatorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = refresh_button("refresh-operator")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let tabs = h_flex()
            .gap_1()
            .pb_4()
            .children(TABS.iter().enumerate().map(|(ix, (tab, label))| {
                let tab = *tab;
                Button::new(("operator-tab", ix))
                    .label(*label)
                    .small()
                    .when(self.tab == tab, |b| b.primary())
                    .when(self.tab != tab, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, window, cx| this.show(tab, window, cx)))
            }));
        let body = v_flex()
            .child(header("Operator", actions))
            .child(tabs)
            .children(self.panel());
        page_body("operator-page", body)
    }
}

// --- shared widgets ---

pub(super) fn section_title(text: impl Into<SharedString>) -> impl IntoElement {
    div().pt_5().pb_2().font_semibold().child(text.into())
}

pub(super) fn muted(text: impl Into<SharedString>, cx: &App) -> gpui_kit::Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
}

pub(super) fn notice_line(text: &SharedString, cx: &App) -> impl IntoElement {
    div()
        .pb_3()
        .text_sm()
        .text_color(cx.theme().success)
        .whitespace_normal()
        .child(text.clone())
}

/// A danger-styled confirm row: what will happen, then confirm or cancel.
pub(super) fn confirm_row(
    message: impl Into<SharedString>,
    confirm: Button,
    cancel: Button,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .p_2()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().danger)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .whitespace_normal()
                .child(message.into()),
        )
        .child(confirm)
        .child(cancel)
}

pub(super) fn diagnostic_row(d: &RebornOperatorConfigDiagnostic, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let (icon, color) = match d.severity {
        RebornOperatorConfigDiagnosticSeverity::Error => (IconName::CircleX, theme.danger),
        RebornOperatorConfigDiagnosticSeverity::Warning => (IconName::TriangleAlert, theme.warning),
        RebornOperatorConfigDiagnosticSeverity::Info => (IconName::Info, theme.muted_foreground),
    };
    h_flex()
        .gap_2()
        .items_start()
        .py_1p5()
        .child(
            div()
                .pt(px(2.))
                .text_color(color)
                .child(Icon::new(icon).small()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().whitespace_normal().child(d.message.clone()))
                .child(muted(format!("{}  ·  {}", d.key, d.reason_code), cx))
                .when(!d.remediation.is_empty(), |this| {
                    this.child(muted(d.remediation.clone(), cx))
                }),
        )
}

// --- status tab ---

/// Readiness checks, doctor diagnostics, and the OS service.
pub struct StatusPanel {
    backend: Backend,
    status: Option<RebornOperatorCommandPlaneResponse>,
    diagnostics: Option<RebornOperatorCommandPlaneResponse>,
    service: Option<RebornServiceLifecycleResponse>,
    running_diagnostics: bool,
    confirm_stop: bool,
    stopping: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl StatusPanel {
    fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            status: None,
            diagnostics: None,
            service: None,
            running_diagnostics: false,
            confirm_stop: false,
            stopping: false,
            error: None,
            notice: None,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.operator_status().await },
            |this, result, cx| {
                match result {
                    Ok(status) => {
                        this.status = Some(status);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
        self.service_action(RebornOperatorServiceLifecycleAction::Status, cx);
    }

    fn run_diagnostics(&mut self, cx: &mut Context<Self>) {
        self.running_diagnostics = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.operator_diagnostics().await },
            |this, result, cx| {
                this.running_diagnostics = false;
                match result {
                    Ok(d) => this.diagnostics = Some(d),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn service_action(
        &mut self,
        action: RebornOperatorServiceLifecycleAction,
        cx: &mut Context<Self>,
    ) {
        let stop = action == RebornOperatorServiceLifecycleAction::Stop;
        if stop {
            self.stopping = true;
            self.confirm_stop = false;
            cx.notify();
        }
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.operator_service(action).await },
            move |this, result, cx| {
                this.stopping = false;
                match result {
                    Ok(resp) => {
                        if stop {
                            this.notice = Some(resp.message.clone().into());
                        }
                        this.service = resp.service_lifecycle;
                    }
                    // The gateway may die before it answers a stop.
                    Err(ironclaw_gpui_client::ClientError::Network(_)) if stop => {
                        this.notice = Some(
                            "Stop sent. The gateway closed the connection while shutting down."
                                .into(),
                        );
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_readiness(&self, cx: &Context<Self>) -> AnyElement {
        let Some(report) = self
            .status
            .as_ref()
            .and_then(|s| s.operator_status.as_ref())
        else {
            return match self.status.as_ref() {
                Some(s) => muted(s.message.clone(), cx).into_any_element(),
                None => Spinner::new().into_any_element(),
            };
        };
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().font_semibold().child("Overall"))
                    .child(state_tag(report.overall))
                    .child(muted(
                        format!("checked {}", jobs::ago(report.generated_at)),
                        cx,
                    )),
            )
            .children(report.checks.iter().map(|c| check_row(c, cx)))
            .into_any_element()
    }

    fn render_service(&self, cx: &Context<Self>) -> impl IntoElement {
        let base_url = self.backend.client().connection().base_url.clone();
        let reported = self.service.as_ref().map(|s| {
            let disagrees = s.state != RebornServiceLifecycleState::Running;
            v_flex()
                .gap_0p5()
                .child(muted(
                    format!("Service manager reports: {} ({})", service_label(s.state), s.message),
                    cx,
                ))
                .when(disagrees, |d| {
                    d.child(muted(
                        "This gateway is answering, so it runs. It may run outside the service manager (ironclaw serve), or the gateway misread launchctl (it parses `launchctl list <label>` as a table, which macOS prints as a dictionary).",
                        cx,
                    ))
                })
                .when_some(s.remediation.clone(), |d, r| d.child(muted(r, cx)))
        });
        let stop = Button::new("stop-service")
            .icon(IconName::Square)
            .label("Stop service")
            .outline()
            .small()
            .loading(self.stopping)
            .disabled(self.stopping)
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm_stop = true;
                cx.notify();
            }));
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child("IronClaw service"))
                            .child(muted(format!("Gateway answering at {base_url}"), cx)),
                    )
                    .child(Tag::success().small().child("Running"))
                    .child(
                        Button::new("service-status")
                            .label("Check")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.service_action(RebornOperatorServiceLifecycleAction::Status, cx)
                            })),
                    )
                    .child(stop),
            )
            .children(reported)
            .when(self.confirm_stop, |d| {
                d.child(confirm_row(
                    "Stop IronClaw? Chats and automations stop, and this app loses its connection. Start it again in a terminal with `ironclaw service start`.",
                    Button::new("confirm-stop")
                        .label("Stop")
                        .danger()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.service_action(RebornOperatorServiceLifecycleAction::Stop, cx)
                        })),
                    Button::new("cancel-stop")
                        .label("Cancel")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_stop = false;
                            cx.notify();
                        })),
                    cx,
                ))
            })
    }

    fn render_diagnostics(&self, cx: &Context<Self>) -> impl IntoElement {
        let run = Button::new("run-diagnostics")
            .label(if self.diagnostics.is_some() {
                "Run again"
            } else {
                "Run diagnostics"
            })
            .outline()
            .small()
            .loading(self.running_diagnostics)
            .disabled(self.running_diagnostics)
            .on_click(cx.listener(|this, _, _, cx| this.run_diagnostics(cx)));
        let result = self.diagnostics.as_ref().map(|d| {
            let rows: Vec<AnyElement> = d
                .diagnostics
                .iter()
                .map(|x| diagnostic_row(x, cx).into_any_element())
                .collect();
            v_flex()
                .child(muted(d.message.clone(), cx))
                .when(rows.is_empty(), |v| v.child(muted("No findings.", cx)))
                .children(rows)
        });
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().font_semibold().child("Doctor"))
                            .child(muted(
                                "Checks setup and readiness and explains what to fix.",
                                cx,
                            )),
                    )
                    .child(run),
            )
            .children(result)
    }
}

fn check_row(c: &RebornOperatorStatusCheck, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_start()
        .py_1()
        .border_t_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_medium().child(c.id.replace('_', " ")))
                .child(muted(c.summary.clone(), cx))
                .when_some(c.remediation.clone(), |d, r| d.child(muted(r, cx))),
        )
        .child(state_tag(c.status))
}

fn state_tag(state: RebornOperatorStatusState) -> Tag {
    match state {
        RebornOperatorStatusState::Ready => Tag::success().small().child("Ready"),
        RebornOperatorStatusState::Degraded => Tag::warning().small().child("Degraded"),
        RebornOperatorStatusState::Blocked => Tag::danger().small().child("Blocked"),
        RebornOperatorStatusState::NotConfigured => Tag::warning().small().child("Not configured"),
        RebornOperatorStatusState::Unsupported => Tag::secondary().small().child("No probe"),
    }
}

fn service_label(state: RebornServiceLifecycleState) -> &'static str {
    match state {
        RebornServiceLifecycleState::Installed => "installed",
        RebornServiceLifecycleState::Running => "running",
        RebornServiceLifecycleState::Stopped => "stopped",
        RebornServiceLifecycleState::Unsupported => "unsupported on this system",
        RebornServiceLifecycleState::Failed => "failed",
        RebornServiceLifecycleState::Unknown => "unknown",
    }
}

impl Render for StatusPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.as_ref(), |d, n| d.child(notice_line(n, cx)))
            .child(section_title("Readiness"))
            .child(self.render_readiness(cx))
            .child(section_title("Service"))
            .child(self.render_service(cx))
            .child(section_title("Diagnostics"))
            .child(self.render_diagnostics(cx))
    }
}
