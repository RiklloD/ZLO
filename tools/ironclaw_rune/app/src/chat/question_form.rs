//! Question forms (GPUI `chat/question_form.rs`): an agent's structured
//! question drawn as a card the person fills in, instead of a raw tool row.
//! One form shape covers every source (Claude's `AskUserQuestion`, Codex's
//! `requestUserInput`, MCP elicitation, Antigravity's questions): choices as
//! chips (one or several, plus "Other" where allowed), text and number
//! inputs, yes/no. Submit sends the answers through the gate route; Dismiss
//! tells the agent the person declined. Both wait for the stream to close
//! the form, like an approval.

use std::collections::HashMap;

use ironclaw_host_api::agent_plan::{
    QuestionAnswer, QuestionField, QuestionFieldKind, QuestionForm, QuestionValue,
};
use rune::{El, TextEdit, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, TextInput};
use zlo_client::transcript::{QuestionState, Row};

use super::{ChatState, chat_for, flow, glyphs, orphan_edit, set_error};
use crate::jobs;
use crate::state::State;

/// What the person has filled in so far, per open form.
#[derive(Default)]
pub(crate) struct Drafts {
    by_gate: HashMap<String, Draft>,
}

#[derive(Default)]
struct Draft {
    /// Picked option labels by field id.
    picks: HashMap<String, Vec<String>>,
    bools: HashMap<String, bool>,
    /// Text, number and "Other" inputs by field id.
    inputs: HashMap<String, TextEdit>,
    error: Option<String>,
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

/// The answers a draft makes: typed text comes in by field id. Empty fields
/// are left out (the form check decides whether that is allowed).
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

/// Make a draft (and its inputs) for every open form; drop drafts of forms
/// that closed. Runs whenever the rows change.
pub(crate) fn sync_drafts(c: &mut ChatState) {
    let open: Vec<(String, QuestionForm)> = c
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Question {
                gate_ref,
                form,
                state: QuestionState::Open | QuestionState::Sending(_),
                ..
            } => Some((gate_ref.clone(), form.clone())),
            _ => None,
        })
        .collect();
    c.questions
        .by_gate
        .retain(|gate, _| open.iter().any(|(g, _)| g == gate));
    for (gate, form) in open {
        if c.questions.by_gate.contains_key(&gate) {
            continue;
        }
        let mut draft = Draft::default();
        for field in form.fields.iter().filter(|f| takes_text(f)) {
            let default = match &field.kind {
                QuestionFieldKind::Text {
                    default: Some(d), ..
                } => d.clone(),
                _ => String::new(),
            };
            draft
                .inputs
                .insert(field.id.clone(), TextEdit::new(default));
        }
        c.questions.by_gate.insert(gate, draft);
    }
}

fn input_edit<'a>(s: &'a mut State, thread: &str, gate: &str, field: &str) -> &'a mut TextEdit {
    match chat_for(s, thread)
        .and_then(|c| c.questions.by_gate.get_mut(gate))
        .and_then(|d| d.inputs.get_mut(field))
    {
        Some(edit) => edit,
        None => orphan_edit(),
    }
}

fn toggle_pick(s: &mut State, thread: &str, gate: &str, field: &QuestionField, label: &str) {
    let multi = matches!(field.kind, QuestionFieldKind::Choice { multi: true, .. });
    let Some(draft) = chat_for(s, thread).and_then(|c| c.questions.by_gate.get_mut(gate)) else {
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
}

fn set_bool(s: &mut State, thread: &str, gate: &str, field: &str, value: bool) {
    if let Some(draft) = chat_for(s, thread).and_then(|c| c.questions.by_gate.get_mut(gate)) {
        draft.bools.insert(field.to_string(), value);
        draft.error = None;
    }
}

/// Send the form's answers (`answer`) or dismiss it.
fn send_question(
    s: &mut State,
    thread: &str,
    run: String,
    gate: String,
    form: &QuestionForm,
    answer: bool,
    tx: &mut Tx,
) {
    let Some(c) = chat_for(s, thread) else { return };
    let reply = if answer {
        let Some(draft) = c.questions.by_gate.get_mut(&gate) else {
            return;
        };
        let typed: HashMap<String, String> = draft
            .inputs
            .iter()
            .map(|(id, input)| (id.clone(), input.text.clone()))
            .collect();
        match answer_of(form, &draft.picks, &draft.bools, &typed) {
            Ok(reply) => Some(reply),
            Err(why) => {
                draft.error = Some(why);
                return;
            }
        }
    } else {
        None
    };
    c.transcript.mark_question_sending(&gate, answer);
    super::refresh_rows(s, thread, tx);
    let thread_id = thread.to_string();
    let req = thread_id.clone();
    let gate_ref = gate.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move { c.answer_question(&req, &run, &gate_ref, reply).await },
        move |s, result, tx| {
            if let Err(e) = result
                && let Some(c) = chat_for(s, &thread_id)
            {
                c.transcript.clear_question_sending(&gate);
                set_error(s, &thread_id, e.to_string());
                super::refresh_rows(s, &thread_id, tx);
            }
        },
    );
}

fn small_button(label: &str, variant: ButtonVariant) -> Button {
    Button::new(label.to_string())
        .variant(variant)
        .control_size(ControlSize::Regular)
}

/// The card for an open (or sending) question. Settled ones fold into their
/// activity group (see `flow`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_question(
    s: &State,
    chat: &ChatState,
    key: &str,
    run_id: &str,
    gate_ref: &str,
    form: &QuestionForm,
    state: QuestionState,
    k: &Kit,
) -> El {
    let p = &s.palette;
    let open = state == QuestionState::Open;
    let draft = chat.questions.by_gate.get(gate_ref);
    let body = flow::type_at(s, 13., 19., 400.);
    let mut card = col()
        .key(format!("{key}-card"))
        .role("group")
        .label("Question from the agent")
        .w_full()
        .gap(12.)
        .padding(12.)
        .radius(12.)
        .border(
            1.,
            if open {
                p.warning.alpha(0.45)
            } else {
                p.text.alpha(0.08)
            },
        )
        .fill(p.text.alpha(0.035))
        .transition(flow::rise(8.))
        .child(
            row()
                .gap(8.)
                .center_y()
                .child(icon(&glyphs::CIRCLE_QUESTION_MARK, 14.).color(if open {
                    p.warning
                } else {
                    p.text_muted
                }))
                .child(
                    text(match &form.source {
                        Some(source) => format!("{source} asks"),
                        None => "The agent asks".to_string(),
                    })
                    .font(flow::type_at(s, 13., 19., 500.))
                    .color(p.text_muted),
                ),
        );
    if let Some(message) = &form.message {
        card = card.child(
            text(message.clone())
                .font(body)
                .color(p.text)
                .wrap()
                .selectable(true),
        );
    }
    for (ix, field) in form.fields.iter().enumerate() {
        card = card.child(render_field(
            s, chat, key, ix, gate_ref, field, draft, open, k,
        ));
    }
    if let Some(error) = draft.and_then(|d| d.error.clone()) {
        card = card.child(
            text(error)
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.danger),
        );
    }
    let thread = chat.thread_id.clone();
    let actions = match state {
        QuestionState::Open => {
            let (run, gate) = (run_id.to_string(), gate_ref.to_string());
            let (run2, gate2, thread2) = (run.clone(), gate.clone(), thread.clone());
            let (form1, form2) = (form.clone(), form.clone());
            row()
                .gap(8.)
                .child(
                    small_button("Submit", ButtonVariant::Contrast)
                        .on_press_tx(move |s: &mut State, tx| {
                            send_question(s, &thread, run.clone(), gate.clone(), &form1, true, tx)
                        })
                        .build(k)
                        .key(format!("{key}-submit")),
                )
                .child(
                    small_button("Dismiss", ButtonVariant::Ghost)
                        .help("The agent reads that you declined to answer")
                        .on_press_tx(move |s: &mut State, tx| {
                            send_question(
                                s,
                                &thread2,
                                run2.clone(),
                                gate2.clone(),
                                &form2,
                                false,
                                tx,
                            )
                        })
                        .build(k)
                        .key(format!("{key}-dismiss")),
                )
        }
        QuestionState::Sending(answered) => row()
            .gap(8.)
            .center_y()
            .child(rune_kit::Spinner::new().size(12.).build(k))
            .child(
                text(if answered {
                    "Answer sent, waiting for the agent"
                } else {
                    "Dismissed, waiting for the agent"
                })
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.text_muted),
            ),
        QuestionState::Closed(_) => rune::el(),
    };
    card.child(actions)
}

#[allow(clippy::too_many_arguments)] // arch-exempt: too_many_args, one row of a form card, as in the GPUI app
fn render_field(
    s: &State,
    chat: &ChatState,
    key: &str,
    ix: usize,
    gate_ref: &str,
    field: &QuestionField,
    draft: Option<&Draft>,
    open: bool,
    k: &Kit,
) -> El {
    let p = &s.palette;
    let small = flow::type_at(s, 12., 16., 400.);
    let mut column = col().key(format!("{key}-f{ix}")).gap(6.);
    let mut title = row().gap(8.).center_y();
    if let Some(header) = &field.header {
        title = title.child(
            text(header.clone())
                .font(flow::type_at(s, 11., 14., 500.))
                .color(p.text_muted)
                .px(6.)
                .py(1.)
                .radius(4.)
                .fill(p.hover),
        );
    }
    let label = if field.required {
        format!("{} *", field.label)
    } else {
        field.label.clone()
    };
    title = title.child(
        text(label)
            .font(flow::type_at(s, 13., 19., 400.))
            .color(p.text)
            .wrap(),
    );
    column = column.child(title);
    if let Some(description) = &field.description {
        column = column.child(
            text(description.clone())
                .font(small)
                .color(p.text_muted)
                .wrap(),
        );
    }
    let thread = chat.thread_id.clone();
    match &field.kind {
        QuestionFieldKind::Choice { options, multi, .. } => {
            let picked = draft
                .and_then(|d| d.picks.get(&field.id))
                .cloned()
                .unwrap_or_default();
            let mut chips = row().flex_wrap().gap(6.);
            for (o, option) in options.iter().enumerate() {
                let selected = picked.contains(&option.label);
                let (gate, field_c, label, thread) = (
                    gate_ref.to_string(),
                    field.clone(),
                    option.label.clone(),
                    thread.clone(),
                );
                let mut chip = small_button(
                    &option.label,
                    if selected {
                        ButtonVariant::Contrast
                    } else {
                        ButtonVariant::Outline
                    },
                )
                .disabled(!open)
                .on_press(move |s: &mut State| toggle_pick(s, &thread, &gate, &field_c, &label));
                if let Some(description) = &option.description
                    && !description.trim().is_empty()
                {
                    chip = chip.help(description.clone());
                }
                chips = chips.child(chip.build(k).key(format!("{key}-f{ix}-o{o}")));
            }
            column = column.child(chips);
            if *multi {
                column = column.child(text("Pick any that apply").font(small).color(p.text_muted));
            }
        }
        QuestionFieldKind::Boolean { default } => {
            let value = draft
                .and_then(|d| d.bools.get(&field.id).copied())
                .or(*default);
            let mut choices = row().gap(6.);
            for (label, choice) in [("Yes", true), ("No", false)] {
                let (gate, id, thread) = (gate_ref.to_string(), field.id.clone(), thread.clone());
                let chosen = value == Some(choice);
                choices = choices.child(
                    small_button(
                        label,
                        if chosen {
                            ButtonVariant::Contrast
                        } else {
                            ButtonVariant::Outline
                        },
                    )
                    .disabled(!open)
                    .on_press(move |s: &mut State| set_bool(s, &thread, &gate, &id, choice))
                    .build(k)
                    .key(format!("{key}-f{ix}-{label}")),
                );
            }
            column = column.child(choices);
        }
        QuestionFieldKind::Text { .. } | QuestionFieldKind::Number { .. } => {}
    }
    if let Some(input) = draft.and_then(|d| d.inputs.get(&field.id)) {
        let placeholder = match field.kind {
            QuestionFieldKind::Choice { .. } => "Other…",
            QuestionFieldKind::Number { .. } => "A number",
            _ => "Your answer",
        };
        let (gate, id) = (gate_ref.to_string(), field.id.clone());
        column = column.child(
            TextInput::new(input)
                .placeholder(placeholder)
                .width(None)
                .disabled(!open)
                .on_edit(move |s: &mut State| input_edit(s, &thread, &gate, &id))
                .build(k)
                .key(format!("{key}-f{ix}-input")),
        );
    }
    column
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
