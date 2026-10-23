//! First-run setup: what is configured, and a form that adds a provider and
//! makes it the active model (`operator.run_setup`).

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::operator::SetupInput;
use ironclaw_gpui_client::product_wire::{
    RebornOperatorSetupResponse, RebornOperatorSetupStatus, RebornOperatorSetupStepStatus,
};

use super::{confirm_row, diagnostic_row, muted, notice_line, section_title};
use crate::jobs;
use crate::pages::error_line;
use crate::ui::*;

struct Form {
    provider_id: Entity<InputState>,
    adapter: Entity<InputState>,
    base_url: Entity<InputState>,
    model: Entity<InputState>,
    api_key: Entity<InputState>,
}

pub struct SetupPanel {
    backend: Backend,
    setup: Option<RebornOperatorSetupResponse>,
    form: Form,
    confirm: Option<SetupInput>,
    applying: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl SetupPanel {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut field = |placeholder: &'static str, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
            })
        };
        let form = Form {
            provider_id: field("Provider id, e.g. openrouter", false),
            adapter: field(
                "Adapter, e.g. open_ai_completions (new provider only)",
                false,
            ),
            base_url: field("Base URL (optional)", false),
            model: field("Model (optional)", false),
            api_key: field("API key (optional)", true),
        };
        let mut this = Self {
            backend,
            setup: None,
            form,
            confirm: None,
            applying: false,
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

    fn read_form(&self, cx: &App) -> SetupInput {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        SetupInput {
            provider_id: text(&self.form.provider_id),
            adapter: text(&self.form.adapter),
            base_url: text(&self.form.base_url),
            model: text(&self.form.model),
            api_key: text(&self.form.api_key),
        }
    }

    fn ask(&mut self, cx: &mut Context<Self>) {
        let input = self.read_form(cx);
        if input.provider_id.trim().is_empty() {
            self.error = Some("Enter a provider id.".into());
        } else {
            self.error = None;
            self.confirm = Some(input);
        }
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.confirm.take() else {
            return;
        };
        // The request owns the key now; clear it from the form.
        self.form
            .api_key
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.applying = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.run_operator_setup(&input).await },
            |this, result, cx| {
                this.applying = false;
                match result {
                    // Show the setup state the gateway read back.
                    Ok(setup) => {
                        this.notice = Some(
                            match (&setup.active_provider_id, &setup.active_model) {
                                (Some(p), Some(m)) => format!("Now using {p} / {m}"),
                                (Some(p), None) => format!("Now using {p}"),
                                _ => setup.message.clone(),
                            }
                            .into(),
                        );
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

    fn render_form(&self, cx: &Context<Self>) -> impl IntoElement {
        let confirm = self.confirm.as_ref().map(|input| {
            let what = if input.adapter.trim().is_empty() {
                format!(
                    "Switch IronClaw to provider \"{}\"? New chats use it right away.",
                    input.provider_id.trim()
                )
            } else {
                format!(
                    "Add or update provider \"{}\" and make it the active model? New chats use it right away.",
                    input.provider_id.trim()
                )
            };
            confirm_row(
                what,
                Button::new("confirm-setup")
                    .label("Apply")
                    .danger()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                Button::new("cancel-setup")
                    .label("Cancel")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm = None;
                        cx.notify();
                    })),
                cx,
            )
        });
        card(cx)
            .child(muted(
                "Provider and model only: switch to a provider IronClaw already knows. Add an adapter (and a key if it needs one) to register a new provider. Settings > Model covers the everyday switch.",
                cx,
            ))
            .child(Input::new(&self.form.provider_id))
            .child(Input::new(&self.form.adapter))
            .child(Input::new(&self.form.base_url))
            .child(Input::new(&self.form.model))
            .child(Input::new(&self.form.api_key))
            .child(
                h_flex().justify_end().child(
                    Button::new("apply-setup")
                        .label("Apply setup")
                        .primary()
                        .small()
                        .loading(self.applying)
                        .disabled(self.applying || self.confirm.is_some())
                        .on_click(cx.listener(|this, _, _, cx| this.ask(cx))),
                ),
            )
            .children(confirm)
    }
}

impl Render for SetupPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.as_ref(), |d, n| d.child(notice_line(n, cx)))
            .child(section_title("Setup state"))
            .child(self.render_state(cx))
            .child(section_title("Provider setup"))
            .child(self.render_form(cx))
    }
}
