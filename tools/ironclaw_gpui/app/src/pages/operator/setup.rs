//! First-run setup: what is configured. Providers and models are added and
//! switched on the Providers page.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::{
    RebornOperatorSetupResponse, RebornOperatorSetupStatus, RebornOperatorSetupStepStatus,
};

use super::{diagnostic_row, muted, notice_line, section_title};
use crate::jobs;
use crate::pages::error_line;
use crate::ui::*;

pub struct SetupPanel {
    backend: Backend,
    setup: Option<RebornOperatorSetupResponse>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl SetupPanel {
    pub fn new(backend: Backend, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            setup: None,
            error: None,
            notice: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.operator_setup().await },
            |this, result, cx| {
                match result {
                    Ok(setup) => {
                        this.setup = Some(setup);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_state(&self, cx: &Context<Self>) -> AnyElement {
        let Some(setup) = self.setup.as_ref() else {
            return Spinner::new().into_any_element();
        };
        let theme = cx.theme();
        let status = match setup.status {
            RebornOperatorSetupStatus::Complete => Tag::success().small().child("Complete"),
            RebornOperatorSetupStatus::Incomplete => Tag::warning().small().child("Incomplete"),
            RebornOperatorSetupStatus::Unsupported => Tag::secondary().small().child("Unsupported"),
        };
        let active = match (&setup.active_provider_id, &setup.active_model) {
            (Some(p), Some(m)) => format!("Active model: {p} / {m}"),
            (Some(p), None) => format!("Active provider: {p}"),
            _ => "No active model".to_string(),
        };
        let steps = setup.steps.iter().map(|step| {
            let (icon, color) = match step.status {
                RebornOperatorSetupStepStatus::Complete => (IconName::CircleCheck, theme.success),
                RebornOperatorSetupStepStatus::Required => (IconName::CircleAlert, theme.warning),
                RebornOperatorSetupStepStatus::Unsupported => {
                    (IconName::Dash, theme.muted_foreground)
                }
            };
            h_flex()
                .gap_2()
                .items_start()
                .py_1()
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
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(step.name.replace('_', " ")),
                        )
                        .child(muted(step.message.clone(), cx)),
                )
        });
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(active))
                            .child(muted(setup.message.clone(), cx)),
                    )
                    .child(status),
            )
            .children(steps)
            .children(setup.diagnostics.iter().map(|d| diagnostic_row(d, cx)))
            .into_any_element()
    }
}

impl Render for SetupPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.as_ref(), |d, n| d.child(notice_line(n, cx)))
            .child(section_title("Setup state"))
            .child(self.render_state(cx))
            .child(div().pt_3().child(muted(
                "Add, remove or switch providers in Settings > Providers.",
                cx,
            )))
    }
}
