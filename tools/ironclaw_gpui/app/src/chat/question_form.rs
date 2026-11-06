//! Question forms: an agent's structured question drawn as a card the person
//! fills in, instead of a raw tool row. One form shape covers every source
//! (Claude's `AskUserQuestion`, Codex's `requestUserInput`, MCP elicitation,
//! Antigravity's questions): choices as chips (one or several, plus "Other"
//! where allowed), text and number inputs, yes/no. Submit sends the answers
//! through the gate route; Dismiss tells the agent the person declined. Both
//! wait for the stream to close the form, like an approval.

use std::collections::HashMap;

use ironclaw_gpui_client::transcript::QuestionState;
use ironclaw_host_api::agent_plan::{
    QuestionAnswer, QuestionField, QuestionFieldKind, QuestionForm, QuestionValue,
};

use super::{ChatView, flow};
use crate::assets::ExtraIcon;
use crate::design::Palette;
use crate::jobs::flatten;
use crate::ui::*;

/// What the person has filled in so far, per open form.
#[derive(Default)]
pub(super) struct Drafts {
    by_gate: HashMap<String, Draft>,
}

#[derive(Default)]
struct Draft {
    /// Picked option labels by field id.
    picks: HashMap<String, Vec<String>>,
    bools: HashMap<String, bool>,
    /// Text, number and "Other" inputs by field id.
    inputs: HashMap<String, Entity<InputState>>,
    error: Option<SharedString>,
}

/// Whether `field` takes typed text: a text or number field, or a choice
/// with "Other".
fn takes_text(field: &QuestionField) -> bool {
    matches!(
        field.kind,
        QuestionFieldKind::Text { .. }
            | QuestionFieldKind::Number { .. }
            | QuestionFieldKind::Choice {
                allow_other: true,
                ..
            }
    )
}

/// The answers a draft makes, without the window: typed text comes in by
/// field id. Empty fields are left out (the form check decides whether
/// that is allowed).
fn answer_of(
    form: &QuestionForm,
    picks: &HashMap<String, Vec<String>>,
    bools: &HashMap<String, bool>,
    typed: &HashMap<String, String>,
) -> Result<QuestionAnswer, String> {
    let mut answer = QuestionAnswer::default();
    for field in &form.fields {
        let text = typed
            .get(&field.id)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let value = match &field.kind {
            QuestionFieldKind::Choice { .. } => {
                let mut chosen = picks.get(&field.id).cloned().unwrap_or_default();
                chosen.extend(text);
                (!chosen.is_empty()).then_some(QuestionValue::Choices(chosen))
            }
            QuestionFieldKind::Text { .. } => text.map(QuestionValue::Text),
            QuestionFieldKind::Number { integer, .. } => match text {
                None => None,
                Some(raw) => {
                    let number = if *integer {
                        raw.parse::<i64>().ok().map(serde_json::Number::from)
                    } else {
                        raw.parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                    };
                    match number {
                        Some(n) => Some(QuestionValue::Number(n)),
                        None => return Err(format!("“{}” needs a number", field.label)),
                    }
                }
            },
            QuestionFieldKind::Boolean { default } => bools
                .get(&field.id)
                .copied()
                .or(*default)
                .map(QuestionValue::Bool),
        };
        if let Some(value) = value {
            answer.values.insert(field.id.clone(), value);
        }
    }
    form.check_answer(&answer).map_err(|e| e.to_string())?;
    Ok(answer)
}

impl ChatView {
    /// Make a draft (and its inputs) for every open form; drop drafts of
    /// forms that closed. Runs in `render`, which has the window inputs need.
    pub(super) fn sync_question_drafts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open: Vec<(String, QuestionForm)> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                ironclaw_gpui_client::transcript::Row::Question {
                    gate_ref,
                    form,
                    state: QuestionState::Open | QuestionState::Sending(_),
                    ..
                } => Some((gate_ref.clone(), form.clone())),
                _ => None,
            })
            .collect();
        self.questions
            .by_gate
            .retain(|gate, _| open.iter().any(|(g, _)| g == gate));
        for (gate, form) in open {
            if self.questions.by_gate.contains_key(&gate) {
                continue;
            }
            let mut draft = Draft::default();
            for field in form.fields.iter().filter(|f| takes_text(f)) {
                let placeholder = match field.kind {
                    QuestionFieldKind::Choice { .. } => "Other…",
                    QuestionFieldKind::Number { .. } => "A number",
                    _ => "Your answer",
                };
                let default = match &field.kind {
                    QuestionFieldKind::Text {
                        default: Some(d), ..
                    } => d.clone(),
                    _ => String::new(),
                };
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(default)
                });
                draft.inputs.insert(field.id.clone(), input);
            }
            self.questions.by_gate.insert(gate, draft);
        }
    }

    fn toggle_pick(
        &mut self,
        gate: &str,
        field: &QuestionField,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let multi = matches!(field.kind, QuestionFieldKind::Choice { multi: true, .. });
        let Some(draft) = self.questions.by_gate.get_mut(gate) else {
            return;
        };
        let picks = draft.picks.entry(field.id.clone()).or_default();
        match picks.iter().position(|p| p == label) {
            Some(at) => {
                picks.remove(at);
            }
            None if multi => picks.push(label.to_string()),
            None => *picks = vec![label.to_string()],
        }
        draft.error = None;
        cx.notify();
    }

    fn set_bool(&mut self, gate: &str, field: &str, value: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.questions.by_gate.get_mut(gate) {
            draft.bools.insert(field.to_string(), value);
            draft.error = None;
            cx.notify();
        }
    }

    /// Send the form's answers (`answer`) or dismiss it.
    fn send_question(
        &mut self,
        run: String,
        gate: String,
        form: &QuestionForm,
        answer: bool,
        cx: &mut Context<Self>,
    ) {
        let reply = if answer {
            let Some(draft) = self.questions.by_gate.get(&gate) else {
                return;
            };
            let typed: HashMap<String, String> = draft
                .inputs
                .iter()
                .map(|(id, input)| (id.clone(), input.read(cx).value().to_string()))
                .collect();
            match answer_of(form, &draft.picks, &draft.bools, &typed) {
                Ok(reply) => Some(reply),
                Err(why) => {
                    if let Some(draft) = self.questions.by_gate.get_mut(&gate) {
                        draft.error = Some(why.into());
                    }
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.transcript.mark_question_sending(&gate, answer);
        self.refresh_rows(cx);
        let thread_id = self.thread_id.clone();
        let gate_ref = gate.clone();
        let job = self.backend.spawn(move |c| async move {
            c.answer_question(&thread_id, &run, &gate_ref, reply).await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.transcript.clear_question_sending(&gate);
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    /// The card for an open (or sending) question. Settled ones fold into
    /// their activity group (see `flow`).
    pub(super) fn render_question(
        &self,
        key: &str,
        run_id: &str,
        gate_ref: &str,
        form: &QuestionForm,
        state: QuestionState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let open = state == QuestionState::Open;
        let draft = self.questions.by_gate.get(gate_ref);
        let mut card = v_flex()
            .id(SharedString::from(format!("{key}-card")))
            .role(Role::Group)
            .aria_label("Question from the agent")
            .w_full()
            .gap_3()
            .p(px(12.))
            .rounded(px(12.))
            .border_1()
            .border_color(if open {
                p.warning.opacity(0.45)
            } else {
                p.text.opacity(0.08)
            })
            .bg(p.text.opacity(0.035))
            .text_size(flow::ui(13.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        Icon::new(ExtraIcon::CircleQuestionMark)
                            .size(px(14.))
                            .text_color(if open { p.warning } else { p.text_muted }),
                    )
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(p.text_muted)
                            .child(match &form.source {
                                Some(source) => format!("{source} asks"),
                                None => "The agent asks".to_string(),
                            }),
                    ),
            );
        if let Some(message) = &form.message {
            card = card.child(div().text_color(p.text).child(message.clone()));
        }
        for (ix, field) in form.fields.iter().enumerate() {
            card = card.child(self.render_field(key, ix, gate_ref, field, draft, open, cx));
        }
        if let Some(error) = draft.and_then(|d| d.error.clone()) {
            card = card.child(div().text_xs().text_color(p.danger).child(error));
        }
        let actions = match state {
            QuestionState::Open => {
                let (run, gate) = (run_id.to_string(), gate_ref.to_string());
                let (run2, gate2) = (run.clone(), gate.clone());
                let (form1, form2) = (form.clone(), form.clone());
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("{key}-submit")))
                            .label("Submit")
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_question(run.clone(), gate.clone(), &form1, true, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{key}-dismiss")))
                            .label("Dismiss")
                            .small()
                            .ghost()
                            .tooltip("The agent reads that you declined to answer")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_question(run2.clone(), gate2.clone(), &form2, false, cx)
                            })),
                    )
                    .into_any_element()
            }
            QuestionState::Sending(answered) => h_flex()
                .gap_2()
                .text_xs()
                .text_color(p.text_muted)
                .child(Spinner::new().xsmall())
                .child(if answered {
                    "Answer sent, waiting for the agent"
                } else {
                    "Dismissed, waiting for the agent"
                })
                .into_any_element(),
            QuestionState::Closed(_) => div().into_any_element(),
        };
        card.child(actions).into_any_element()
    }

    #[allow(clippy::too_many_arguments)] // arch-exempt: too_many_args, one row of a form card; a FieldCtx bundle buys nothing at one call site, plan #plan-forms
    fn render_field(
        &self,
        key: &str,
        ix: usize,
        gate_ref: &str,
        field: &QuestionField,
        draft: Option<&Draft>,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let mut col = v_flex().gap_1p5();
        let mut title = h_flex().gap_2().items_baseline();
        if let Some(header) = &field.header {
            title = title.child(Tag::secondary().small().child(header.clone()));
        }
        title = title.child(
            div()
                .text_color(p.text)
                .child(field.label.clone())
                .when(field.required, |d| d.child(" *")),
        );
        col = col.child(title);
        if let Some(description) = &field.description {
            col = col.child(
                div()
                    .text_xs()
                    .text_color(p.text_muted)
                    .child(description.clone()),
            );
        }
        match &field.kind {
            QuestionFieldKind::Choice { options, multi, .. } => {
                let picked = draft
                    .and_then(|d| d.picks.get(&field.id))
                    .cloned()
                    .unwrap_or_default();
                let mut chips = h_flex().flex_wrap().gap_1p5();
                for (o, option) in options.iter().enumerate() {
                    let selected = picked.contains(&option.label);
                    let (gate, field_c, label) =
                        (gate_ref.to_string(), field.clone(), option.label.clone());
                    let mut chip = Button::new(SharedString::from(format!("{key}-f{ix}-o{o}")))
                        .label(option.label.clone())
                        .small()
                        .selected(selected)
                        .disabled(!open)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_pick(&gate, &field_c, &label, cx)
                        }));
                    chip = if selected {
                        chip.primary()
                    } else {
                        chip.outline()
                    };
                    if let Some(description) = &option.description
                        && !description.trim().is_empty()
                    {
                        chip = chip.tooltip(description.clone());
                    }
                    chips = chips.child(chip);
                }
                col = col.child(chips);
                if *multi {
                    col = col.child(
                        div()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child("Pick any that apply"),
                    );
                }
            }
            QuestionFieldKind::Boolean { default } => {
                let value = draft
                    .and_then(|d| d.bools.get(&field.id).copied())
                    .or(*default);
                let mut row = h_flex().gap_1p5();
                for (label, choice) in [("Yes", true), ("No", false)] {
                    let (gate, id) = (gate_ref.to_string(), field.id.clone());
                    let chosen = value == Some(choice);
                    let button =
                        Button::new(SharedString::from(format!("{key}-f{ix}-{label}")))
                            .label(label)
                            .small()
                            .selected(chosen)
                            .disabled(!open)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_bool(&gate, &id, choice, cx)
                            }));
                    row = row.child(if chosen {
                        button.primary()
                    } else {
                        button.outline()
                    });
                }
                col = col.child(row);
            }
            QuestionFieldKind::Text { .. } | QuestionFieldKind::Number { .. } => {}
        }
        if let Some(input) = draft.and_then(|d| d.inputs.get(&field.id)) {
            col = col.child(Input::new(input).small().disabled(!open));
        }
        col.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::agent_plan::QuestionOption;

    fn form() -> QuestionForm {
        QuestionForm {
            source: None,
            message: None,
            fields: vec![
                QuestionField {
                    id: "db".into(),
                    label: "Database?".into(),
                    header: None,
                    description: None,
                    required: true,
                    kind: QuestionFieldKind::Choice {
                        options: vec![QuestionOption::new("Postgres")],
                        multi: false,
                        allow_other: true,
                        min_items: None,
                        max_items: None,
                    },
                },
                QuestionField {
                    id: "n".into(),
                    label: "Seats".into(),
                    header: None,
                    description: None,
                    required: false,
                    kind: QuestionFieldKind::Number {
                        integer: true,
                        minimum: None,
                        maximum: None,
                    },
                },
                QuestionField {
                    id: "ok".into(),
                    label: "Agree?".into(),
                    header: None,
                    description: None,
                    required: false,
                    kind: QuestionFieldKind::Boolean {
                        default: Some(false),
                    },
                },
            ],
        }
    }

    #[test]
    fn a_draft_becomes_answers_the_form_accepts() {
        let form = form();
        let none: HashMap<String, Vec<String>> = HashMap::new();
        let no_bools: HashMap<String, bool> = HashMap::new();
        let no_text: HashMap<String, String> = HashMap::new();
        assert!(
            answer_of(&form, &none, &no_bools, &no_text).is_err(),
            "the required choice is empty"
        );
        let picks = HashMap::from([("db".to_string(), vec!["Postgres".to_string()])]);
        let typed = HashMap::from([("n".to_string(), " 3 ".to_string())]);
        let answer = answer_of(&form, &picks, &no_bools, &typed).unwrap();
        assert_eq!(
            answer.values["db"],
            QuestionValue::Choices(vec!["Postgres".into()])
        );
        assert_eq!(answer.values["n"], QuestionValue::Number(3.into()));
        assert_eq!(
            answer.values["ok"],
            QuestionValue::Bool(false),
            "default kept"
        );

        let other = HashMap::from([("db".to_string(), "DuckDB".to_string())]);
        let answer = answer_of(&form, &none, &no_bools, &other).unwrap();
        assert_eq!(
            answer.values["db"],
            QuestionValue::Choices(vec!["DuckDB".into()])
        );
        let bad = HashMap::from([
            ("db".to_string(), "x".to_string()),
            ("n".to_string(), "three".to_string()),
        ]);
        assert!(answer_of(&form, &none, &no_bools, &bad).is_err());
    }
}
