//! Operator → Setup: what is configured. Providers and models are added
//! and switched on the Providers page (GPUI `pages/operator/setup.rs`).

use crate::pages::common::controls::{muted, outlined_tag, spinner};
use crate::pages::common::ts::{body_semibold, sm_medium};
use crate::pages::common::{Tone, card, error_line, glyph};
use zlo_client::product_wire::{
    RebornOperatorSetupResponse, RebornOperatorSetupStatus, RebornOperatorSetupStepStatus,
};
use rune::{El, col, row, text};
use rune_kit::Kit;

use super::{diagnostic_row, notice_line, section_title};
use crate::icons;
use crate::jobs;
use crate::state::State;

#[derive(Default)]
pub struct SetupPanel {
    pub setup: Option<RebornOperatorSetupResponse>,
    pub error: Option<String>,
    pub notice: Option<String>,
}

fn st(s: &mut State) -> Option<&mut SetupPanel> {
    s.pages.operator.setup.as_mut()
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "operator",
        |c| async move { c.operator_setup().await },
        |s, result, _| {
            let Some(p) = st(s) else { return };
            match result {
                Ok(setup) => {
                    p.setup = Some(setup);
                    p.error = None;
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn render_state(s: &State, k: &Kit, panel: &SetupPanel) -> El {
    let p = &s.palette;
    let Some(setup) = panel.setup.as_ref() else {
        return spinner(k);
    };
    let status = match setup.status {
        RebornOperatorSetupStatus::Complete => outlined_tag(p, Tone::Success, "Complete"),
        RebornOperatorSetupStatus::Incomplete => outlined_tag(p, Tone::Warning, "Incomplete"),
        RebornOperatorSetupStatus::Unsupported => outlined_tag(p, Tone::Secondary, "Unsupported"),
    };
    let active = match (&setup.active_provider_id, &setup.active_model) {
        (Some(pid), Some(m)) => format!("Active model: {pid} / {m}"),
        (Some(pid), None) => format!("Active provider: {pid}"),
        _ => "No active model".to_string(),
    };
    let steps = setup.steps.iter().map(|step| {
        let (i, color) = match step.status {
            RebornOperatorSetupStepStatus::Complete => (&icons::CIRCLE_CHECK, p.success),
            RebornOperatorSetupStepStatus::Required => (&icons::CIRCLE_ALERT, p.warning),
            RebornOperatorSetupStepStatus::Unsupported => (&icons::DASH, p.text_muted),
        };
        row()
            .key(format!("step-{}", step.name))
            .w_full()
            .gap(8.)
            .items_start()
            .py(4.)
            .child(col().pt(2.).child(glyph(i, 14., color)))
            .child(
                col()
                    .grow(1.)
                    .min_w(0.)
                    .child(
                        text(step.name.replace('_', " "))
                            .font(sm_medium())
                            .color(p.text),
                    )
                    .child(muted(p, step.message.clone())),
            )
    });
    card(p)
        .key("setup-state")
        .child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    col()
                        .grow(1.)
                        .min_w(0.)
                        .child(text(active).font(body_semibold()).color(p.text))
                        .child(muted(p, setup.message.clone())),
                )
                .child(status),
        )
        .children(steps)
        .children(setup.diagnostics.iter().map(|d| diagnostic_row(s, d)))
}

pub fn view(s: &State, k: &Kit, panel: &SetupPanel) -> El {
    let p = &s.palette;
    let mut c = col().w_full();
    if let Some(e) = &panel.error {
        c = c.child(error_line(p, e));
    }
    if let Some(n) = &panel.notice {
        c = c.child(notice_line(s, n));
    }
    c.child(section_title(s, "Setup state"))
        .child(render_state(s, k, panel))
        .child(col().pt(12.).child(muted(
            p,
            "Add, remove or switch providers in Settings > Providers.",
        )))
}
