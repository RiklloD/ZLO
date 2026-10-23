//! Operator config keys: list with source and precedence, validate, and edit
//! one key as JSON (read fresh first, confirm before saving).

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::{
    RebornOperatorConfigEntry, RebornOperatorConfigListResponse,
    RebornOperatorConfigValidateResponse,
};

use super::{confirm_row, diagnostic_row, muted, notice_line, section_title};
use crate::jobs;
use crate::pages::{empty_state, error_line};
use crate::ui::*;

const VALUE_PREVIEW_CHARS: usize = 120;

struct Editor {
    key: String,
    input: Entity<InputState>,
    confirming: bool,
    saving: bool,
}

pub struct ConfigPanel {
    backend: Backend,
    list: Option<RebornOperatorConfigListResponse>,
    validation: Option<RebornOperatorConfigValidateResponse>,
    validating: bool,
    filter: Entity<InputState>,
    editor: Option<Editor>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    _filter_events: Subscription,
}

impl ConfigPanel {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter keys"));
        let filter_events = cx.subscribe(&filter, |_, _, event, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        let mut this = Self {
            backend,
            list: None,
            validation: None,
            validating: false,
            filter,
            editor: None,
            error: None,
            notice: None,
            _filter_events: filter_events,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.operator_config().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        this.list = Some(list);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn validate(&mut self, cx: &mut Context<Self>) {
        self.validating = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.validate_operator_config(Vec::new()).await },
            |this, result, cx| {
                this.validating = false;
                match result {
                    Ok(v) => this.validation = Some(v),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// Read the key fresh (`get_config_key`) so the editor starts from the
    /// stored value, not a stale list row.
    fn edit(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = None;
        let job_key = key.clone();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move { c.operator_config_key(&job_key).await },
            move |this, result, window, cx| {
                match result {
                    Ok(resp) => {
                        let text = if resp.entry.redacted {
                            String::new()
                        } else {
                            resp.entry.value.to_string()
                        };
                        let input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(
                                    "JSON value, e.g. true or {\"state\": \"ask_each_time\"}",
                                )
                                .default_value(text)
                        });
                        input.update(cx, |s, cx| s.focus(window, cx));
                        this.replace_row(resp.entry);
                        this.editor = Some(Editor {
                            key,
                            input,
                            confirming: false,
                            saving: false,
                        });
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn ask_save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let raw = editor.input.read(cx).value().to_string();
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(_) => {
                editor.confirming = true;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Not valid JSON: {e}").into()),
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let raw = editor.input.read(cx).value().to_string();
        let value = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => value,
            Err(e) => {
                self.error = Some(format!("Not valid JSON: {e}").into());
                cx.notify();
                return;
            }
        };
        editor.confirming = false;
        editor.saving = true;
        let key = editor.key.clone();
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_operator_config_key(&key, value).await },
            |this, result, cx| {
                match result {
                    // The response is the gateway's read-back of the stored key.
                    Ok(resp) => {
                        this.notice = Some(format!("Saved {}", resp.entry.key).into());
                        this.replace_row(resp.entry);
                        this.editor = None;
                        this.error = None;
                    }
                    Err(e) => {
                        if let Some(editor) = this.editor.as_mut() {
                            editor.saving = false;
                        }
                        this.error = Some(e.to_string().into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn replace_row(&mut self, entry: RebornOperatorConfigEntry) {
        if let Some(row) = self
            .list
            .as_mut()
            .and_then(|l| l.entries.iter_mut().find(|e| e.key == entry.key))
        {
            *row = entry;
        }
    }

    fn render_row(
        &self,
        ix: usize,
        entry: &RebornOperatorConfigEntry,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let editing = self.editor.as_ref().filter(|e| e.key == entry.key);
        let value = if entry.redacted {
            "(secret, hidden)".to_string()
        } else {
            preview(&entry.value)
        };
        let key = entry.key.clone();
        v_flex()
            .gap_1()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_medium().truncate().child(entry.key.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(value),
                            ),
                    )
                    .child(Tag::secondary().small().child(entry.source.clone()))
                    .when(!entry.mutable, |d| d.child(Tag::secondary().small().child("Read-only")))
                    .when(entry.mutable && editing.is_none(), |d| {
                        d.child(
                            Button::new(("edit-key", ix))
                                .label("Edit")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit(key.clone(), window, cx)
                                })),
                        )
                    }),
            )
            .when_some(editing, |d, editor| {
                d.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&editor.input)))
                        .child(
                            Button::new(("save-key", ix))
                                .label("Save")
                                .primary()
                                .small()
                                .loading(editor.saving)
                                .disabled(editor.saving || editor.confirming)
                                .on_click(cx.listener(|this, _, _, cx| this.ask_save(cx))),
                        )
                        .child(
                            Button::new(("cancel-edit", ix))
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.editor = None;
                                    cx.notify();
                                })),
                        ),
                )
                .when(editor.confirming, |d| {
                    d.child(confirm_row(
                        format!(
                            "Change {}? This is an operator setting and applies to every chat and automation.",
                            editor.key
                        ),
                        Button::new(("confirm-save", ix))
                            .label("Change")
                            .danger()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                        Button::new(("cancel-save", ix))
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = this.editor.as_mut() {
                                    editor.confirming = false;
                                }
                                cx.notify();
                            })),
                        cx,
                    ))
                })
            })
            .into_any_element()
    }

    fn render_validation(&self, cx: &Context<Self>) -> impl IntoElement {
        let result = self.validation.as_ref().map(|v| {
            v_flex()
                .child(if v.valid {
                    Tag::success().small().child("Valid")
                } else {
                    Tag::danger().small().child("Problems found")
                })
                .children(v.diagnostics.iter().map(|d| diagnostic_row(d, cx)))
        });
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().font_semibold().child("Validate"))
                            .child(muted("Checks every key against the effective config.", cx)),
                    )
                    .child(
                        Button::new("validate-config")
                            .label("Validate")
                            .outline()
                            .small()
                            .loading(self.validating)
                            .disabled(self.validating)
                            .on_click(cx.listener(|this, _, _, cx| this.validate(cx))),
                    ),
            )
            .children(result)
    }
}

/// One-line JSON preview, cut on a char boundary.
fn preview(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= VALUE_PREVIEW_CHARS {
        return text;
    }
    let cut: String = text.chars().take(VALUE_PREVIEW_CHARS).collect();
    format!("{cut}…")
}

impl Render for ConfigPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let needle = self.filter.read(cx).value().trim().to_lowercase();
        let (rows, precedence, diagnostics): (Vec<AnyElement>, String, Vec<AnyElement>) = match self
            .list
            .as_ref()
        {
            Some(list) => (
                list.entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| needle.is_empty() || e.key.to_lowercase().contains(&needle))
                    .map(|(ix, e)| self.render_row(ix, e, cx))
                    .collect(),
                list.precedence.join(" > "),
                list.diagnostics
                    .iter()
                    .map(|d| diagnostic_row(d, cx).into_any_element())
                    .collect(),
            ),
            None => (Vec::new(), String::new(), Vec::new()),
        };
        let loaded = self.list.is_some();
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.as_ref(), |d, n| d.child(notice_line(n, cx)))
            .child(self.render_validation(cx))
            .child(section_title("Keys"))
            .when(!precedence.is_empty(), |d| {
                d.child(muted(format!("Precedence: {precedence}"), cx).pb_2())
            })
            .children(diagnostics)
            .child(Input::new(&self.filter))
            .when(!loaded, |d| d.child(Spinner::new()))
            .when(loaded && rows.is_empty(), |d| {
                d.child(empty_state("No matching keys.", cx))
            })
            .child(v_flex().children(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::preview;

    #[test]
    fn preview_cuts_long_values_on_char_boundaries() {
        assert_eq!(preview(&serde_json::json!(true)), "true");
        let long = serde_json::Value::String("é".repeat(200));
        let out = preview(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 121);
    }
}
