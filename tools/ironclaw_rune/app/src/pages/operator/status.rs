//! Operator → Status: readiness checks, doctor diagnostics, and the OS
//! service (GPUI `StatusPanel` in `pages/operator.rs`).

use crate::pages::common::controls::{
    Look, Size, button_sized, muted, outlined_tag, rule, spinner,
};
use crate::pages::common::ts::{BODY_SEMIBOLD, SM_MEDIUM};
use crate::pages::common::{Tone, card, error_line, loading};
use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::product_wire::{
    RebornOperatorCommandPlaneResponse, RebornOperatorServiceLifecycleAction,
    RebornOperatorStatusCheck, RebornOperatorStatusState, RebornServiceLifecycleResponse,
    RebornServiceLifecycleState,
};
use rune::{El, col, row, text};
use rune_kit::Kit;

use super::{confirm_row, diagnostic_row, notice_line, section_title};
use crate::icons;
use crate::jobs;
use crate::state::State;

#[derive(Default)]
pub struct StatusPanel {
    pub status: Option<RebornOperatorCommandPlaneResponse>,
    pub diagnostics: Option<RebornOperatorCommandPlaneResponse>,
    pub service: Option<RebornServiceLifecycleResponse>,
    pub running_diagnostics: bool,
    pub confirm_stop: bool,
    pub stopping: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
}

fn st(s: &mut State) -> &mut StatusPanel {
    &mut s.pages.operator.status
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "operator",
        |c| async move { c.operator_status().await },
        |s, result, _| {
            let p = st(s);
            match result {
                Ok(status) => {
                    p.status = Some(status);
                    p.error = None;
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
    service_action(s, RebornOperatorServiceLifecycleAction::Status);
}

fn run_diagnostics(s: &mut State) {
    st(s).running_diagnostics = true;
    jobs::run(
        s,
        "operator",
        |c| async move { c.operator_diagnostics().await },
        |s, result, _| {
            let p = st(s);
            p.running_diagnostics = false;
            match result {
                Ok(d) => p.diagnostics = Some(d),
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn service_action(s: &mut State, action: RebornOperatorServiceLifecycleAction) {
    let stop = action == RebornOperatorServiceLifecycleAction::Stop;
    if stop {
        let p = st(s);
        p.stopping = true;
        p.confirm_stop = false;
    }
    jobs::run(
        s,
        "operator",
        move |c| async move { c.operator_service(action).await },
        move |s, result, _| {
            let p = st(s);
            p.stopping = false;
            match result {
                Ok(resp) => {
                    if stop {
                        p.notice = Some(resp.message.clone());
                    }
                    p.service = resp.service_lifecycle;
                }
                // The gateway may die before it answers a stop.
                Err(ClientError::Network(_)) if stop => {
                    p.notice = Some(
                        "Stop sent. The gateway closed the connection while shutting down.".into(),
                    );
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn render_readiness(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let panel = &s.pages.operator.status;
    let Some(report) = panel
        .status
        .as_ref()
        .and_then(|x| x.operator_status.as_ref())
    else {
        return match panel.status.as_ref() {
            Some(x) => muted(p, x.message.clone()),
            None => spinner(k),
        };
    };
    card(p)
        .key("readiness")
        .child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(text("Overall").font(BODY_SEMIBOLD).color(p.text).grow(1.))
                .child(state_tag(s, report.overall))
                .child(muted(
                    p,
                    format!("checked {}", jobs::ago(report.generated_at)),
                )),
        )
        .children(report.checks.iter().map(|c| check_row(s, c)))
}

fn render_service(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let panel = &s.pages.operator.status;
    let base_url =
        s.rt.backend
            .as_ref()
            .map(|b| b.client().connection().base_url.to_string())
            .unwrap_or_else(|| "(not connected)".into());
    let stop = loading(
        button_sized("Stop service", Look::Outline, Size::Small)
            .icon(&icons::SQUARE)
            .on_press(|s: &mut State| st(s).confirm_stop = true),
        panel.stopping,
    );
    let mut c = card(p).key("service").child(
        row()
            .w_full()
            .gap(8.)
            .center_y()
            .child(
                col()
                    .grow(1.)
                    .min_w(0.)
                    .child(text("IronClaw service").font(BODY_SEMIBOLD).color(p.text))
                    .child(muted(p, format!("Gateway answering at {base_url}"))),
            )
            .child(outlined_tag(p, Tone::Success, "Running"))
            .child(
                button_sized("Check", Look::Ghost, Size::Small)
                    .on_press(|s: &mut State| {
                        service_action(s, RebornOperatorServiceLifecycleAction::Status)
                    })
                    .build(k)
                    .key("service-status"),
            )
            .child(stop.build(k).key("stop-service")),
    );
    if let Some(svc) = &panel.service {
        let mut reported = col().w_full().gap(2.).child(muted(
            p,
            format!(
                "Service manager reports: {} ({})",
                service_label(svc.state),
                svc.message
            ),
        ));
        if svc.state != RebornServiceLifecycleState::Running {
            reported = reported.child(muted(
                p,
                "This gateway is answering, so it runs, probably outside the service manager (`ironclaw serve` in a terminal).",
            ));
        }
        if let Some(r) = &svc.remediation {
            reported = reported.child(muted(p, r.clone()));
        }
        c = c.child(reported);
    }
    if panel.confirm_stop {
        c = c.child(
            confirm_row(
                s,
                k,
                "Stop IronClaw? Chats and automations stop, and this app loses its connection. \"Try again\" on the connect screen starts it back up.".into(),
                button_sized("Stop", Look::Danger, Size::Small).on_press(|s: &mut State| {
                    service_action(s, RebornOperatorServiceLifecycleAction::Stop)
                }),
                button_sized("Cancel", Look::Ghost, Size::Small)
                    .on_press(|s: &mut State| st(s).confirm_stop = false),
            )
            .key("confirm-stop"),
        );
    }
    c
}

fn render_diagnostics(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let panel = &s.pages.operator.status;
    let run = loading(
        button_sized(
            if panel.diagnostics.is_some() {
                "Run again"
            } else {
                "Run diagnostics"
            },
            Look::Outline,
            Size::Small,
        )
        .on_press(|s: &mut State| run_diagnostics(s)),
        panel.running_diagnostics,
    );
    let mut c = card(p).key("doctor").child(
        row()
            .w_full()
            .gap(8.)
            .center_y()
            .child(
                col()
                    .grow(1.)
                    .child(text("Doctor").font(BODY_SEMIBOLD).color(p.text))
                    .child(muted(
                        p,
                        "Checks setup and readiness and explains what to fix.",
                    )),
            )
            .child(run.build(k).key("run-diagnostics")),
    );
    if let Some(d) = &panel.diagnostics {
        let mut result = col().w_full().child(muted(p, d.message.clone()));
        if d.diagnostics.is_empty() {
            result = result.child(muted(p, "No findings."));
        }
        c = c.child(result.children(d.diagnostics.iter().map(|x| diagnostic_row(s, x))));
    }
    c
}

fn check_row(s: &State, c: &RebornOperatorStatusCheck) -> El {
    let p = &s.palette;
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .gap(2.)
        .child(text(c.id.replace('_', " ")).font(SM_MEDIUM).color(p.text))
        .child(muted(p, c.summary.clone()));
    if let Some(r) = &c.remediation {
        info = info.child(muted(p, r.clone()));
    }
    col()
        .key(format!("check-{}", c.id))
        .w_full()
        .child(rule(p))
        .child(
            row()
                .w_full()
                .gap(8.)
                .items_start()
                .py(4.)
                .child(info)
                .child(state_tag(s, c.status)),
        )
}

fn state_tag(s: &State, state: RebornOperatorStatusState) -> El {
    let p = &s.palette;
    match state {
        RebornOperatorStatusState::Ready => outlined_tag(p, Tone::Success, "Ready"),
        RebornOperatorStatusState::Degraded => outlined_tag(p, Tone::Warning, "Degraded"),
        RebornOperatorStatusState::Blocked => outlined_tag(p, Tone::Danger, "Blocked"),
        RebornOperatorStatusState::NotConfigured => {
            outlined_tag(p, Tone::Warning, "Not configured")
        }
        RebornOperatorStatusState::Unsupported => outlined_tag(p, Tone::Secondary, "No probe"),
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

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let panel = &s.pages.operator.status;
    let mut c = col().w_full();
    if let Some(e) = &panel.error {
        c = c.child(error_line(p, e));
    }
    if let Some(n) = &panel.notice {
        c = c.child(notice_line(s, n));
    }
    c.child(section_title(s, "Readiness"))
        .child(render_readiness(s, k))
        .child(section_title(s, "Service"))
        .child(render_service(s, k))
        .child(section_title(s, "Diagnostics"))
        .child(render_diagnostics(s, k))
}
