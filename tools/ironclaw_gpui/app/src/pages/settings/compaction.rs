//! "Compact idle Claude threads": the engine setting that has an idle Claude
//! CLI thread send `/compact` while its prompt cache is still warm. Each run
//! is listed with its cost on the Usage page.

use ironclaw_gpui_client::Backend;

use crate::jobs;
use crate::pages::{error_line, rows_card, setting_row};
use crate::ui::*;

pub struct CompactionPanel {
    backend: Backend,
    /// The stored value; `None` until the gateway answered.
    enabled: Option<bool>,
    saving: bool,
    error: Option<SharedString>,
}

impl CompactionPanel {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            enabled: None,
            saving: false,
            error: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.idle_compact().await },
            |this, result, cx| {
                match result {
                    Ok(enabled) => {
                        this.enabled = Some(enabled);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn set(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_idle_compact(enabled).await },
            |this, result, cx| {
                this.saving = false;
                // Show the value the engine stored, not the click.
                match result {
                    Ok(stored) => {
                        this.enabled = Some(stored);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }
}

impl Render for CompactionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.enabled.unwrap_or(false);
        let switch = Switch::new("idle-compact")
            .checked(on)
            .disabled(self.enabled.is_none() || self.saving)
            .accessibility_label("Compact idle Claude threads")
            .on_click(cx.listener(|this, checked: &bool, _, cx| this.set(*checked, cx)));
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .child(rows_card(
                [setting_row(
                    "Compact idle Claude threads",
                    Some(
                        "After 4.5 idle minutes, a Claude CLI thread with a large context sends /compact while the prompt cache is still warm, so your next message costs less. Each run shows on the Usage page."
                            .into(),
                    ),
                    switch,
                    cx,
                )],
                cx,
            ))
    }
}
