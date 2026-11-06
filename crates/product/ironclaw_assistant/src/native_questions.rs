//! `ask_user` for IronClaw's own agent loop: a structured question the model
//! asks and the person answers in a form, like the CLI runtimes'
//! `AskUserQuestion`.
//!
//! It rides the existing external-tool gate. For a WebUI run the run's
//! external-tool catalog is decorated ([`offer_ask_user`]) so the model is
//! offered `ask_user`; calling it parks the run as `BlockedExternalTool` with
//! the call recorded in the catalog. The projection draws the parked call as a
//! question form ([`pending_question`]), and gate resolution submits the
//! person's answer (or dismissal) as the call's output and resumes the run
//! ([`tool_output`]); the loop then writes that output as the tool result,
//! which is the durable record of the answer.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::agent_plan::{
    QUESTION_DECLINED_MESSAGE, QuestionAnswer, QuestionField, QuestionFieldKind, QuestionForm,
    QuestionOption,
};
use ironclaw_host_api::turn::{TurnOriginKind, TurnRunId};
use ironclaw_turns::{
    ExternalToolCatalog, ExternalToolCatalogError, ExternalToolSpec, ExternalToolSpecError,
    PendingExternalCall,
};
use serde_json::{Value, json};

/// The tool name the model calls.
pub const ASK_USER_TOOL_NAME: &str = "ask_user";
/// Most questions one call asks.
const ASK_USER_MAX_QUESTIONS: usize = 4;

const ASK_USER_DESCRIPTION: &str = include_str!("../prompts/ask_user.md");

fn ask_user_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": ASK_USER_MAX_QUESTIONS,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "The question, as the person reads it" },
                        "header": { "type": "string", "description": "A short tag, e.g. 'Database'" },
                        "options": {
                            "type": "array",
                            "maxItems": 8,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string" },
                                    "description": { "type": "string" }
                                },
                                "required": ["label"],
                                "additionalProperties": false
                            }
                        },
                        "multi_select": { "type": "boolean" },
                        "allow_free_text": { "type": "boolean" }
                    },
                    "required": ["question"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["questions"],
        "additionalProperties": false
    })
}

/// The `ask_user` tool as an external-tool spec.
pub fn ask_user_spec() -> Result<ExternalToolSpec, ExternalToolSpecError> {
    ExternalToolSpec::new(
        ASK_USER_TOOL_NAME,
        ASK_USER_DESCRIPTION.trim(),
        ask_user_schema(),
    )
}

/// The run's catalog, offering `ask_user` when the run came from the WebUI
/// (the desktop app draws the form); every other origin gets the catalog
/// unchanged, so a channel or API run is never offered a tool nobody can
/// answer.
pub fn offer_ask_user(
    catalog: Arc<dyn ExternalToolCatalog>,
    origin: Option<TurnOriginKind>,
) -> Arc<dyn ExternalToolCatalog> {
    match (origin, ask_user_spec()) {
        (Some(TurnOriginKind::WebUi), Ok(spec)) => Arc::new(AskUserToolCatalog {
            inner: catalog,
            spec,
        }),
        (_, Err(error)) => {
            tracing::debug!(%error, "ask_user spec rejected; not offered");
            catalog
        }
        _ => catalog,
    }
}

/// Appends `ask_user` to a run's caller-declared tools; everything else is
/// the inner catalog's.
struct AskUserToolCatalog {
    inner: Arc<dyn ExternalToolCatalog>,
    spec: ExternalToolSpec,
}

#[async_trait]
impl ExternalToolCatalog for AskUserToolCatalog {
    async fn register(
        &self,
        run_id: TurnRunId,
        specs: Vec<ExternalToolSpec>,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner.register(run_id, specs).await
    }

    async fn specs(
        &self,
        run_id: TurnRunId,
    ) -> Result<Vec<ExternalToolSpec>, ExternalToolCatalogError> {
        let mut specs = self.inner.specs(run_id).await?;
        // A caller's own `ask_user` wins.
        if !specs.iter().any(|s| s.name() == self.spec.name()) {
            specs.push(self.spec.clone());
        }
        Ok(specs)
    }

    async fn bind_call(
        &self,
        run_id: TurnRunId,
        input_ref: String,
        call_id: String,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner.bind_call(run_id, input_ref, call_id).await
    }

    async fn call_id_for_input_ref(
        &self,
        run_id: TurnRunId,
        input_ref: &str,
    ) -> Result<Option<String>, ExternalToolCatalogError> {
        self.inner.call_id_for_input_ref(run_id, input_ref).await
    }

    async fn submit_output(
        &self,
        run_id: TurnRunId,
        call_id: String,
        output: Value,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner.submit_output(run_id, call_id, output).await
    }

    async fn submit_output_for_pending_call(
        &self,
        run_id: TurnRunId,
        call_id: String,
        output: Value,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner
            .submit_output_for_pending_call(run_id, call_id, output)
            .await
    }

    async fn output(
        &self,
        run_id: TurnRunId,
        call_id: &str,
    ) -> Result<Option<Value>, ExternalToolCatalogError> {
        self.inner.output(run_id, call_id).await
    }

    async fn output_for_input_ref(
        &self,
        run_id: TurnRunId,
        input_ref: &str,
    ) -> Result<Option<Value>, ExternalToolCatalogError> {
        self.inner.output_for_input_ref(run_id, input_ref).await
    }

    async fn take_output(
        &self,
        run_id: TurnRunId,
        call_id: &str,
    ) -> Result<Option<Value>, ExternalToolCatalogError> {
        self.inner.take_output(run_id, call_id).await
    }

    async fn take_output_for_input_ref(
        &self,
        run_id: TurnRunId,
        input_ref: &str,
    ) -> Result<Option<Value>, ExternalToolCatalogError> {
        self.inner
            .take_output_for_input_ref(run_id, input_ref)
            .await
    }

    async fn complete_call_for_input_ref(
        &self,
        run_id: TurnRunId,
        input_ref: &str,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner
            .complete_call_for_input_ref(run_id, input_ref)
            .await
    }

    async fn record_pending_call(
        &self,
        run_id: TurnRunId,
        call: PendingExternalCall,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner.record_pending_call(run_id, call).await
    }

    async fn pending_calls(
        &self,
        run_id: TurnRunId,
    ) -> Result<Vec<PendingExternalCall>, ExternalToolCatalogError> {
        self.inner.pending_calls(run_id).await
    }

    async fn clear_pending_call(
        &self,
        run_id: TurnRunId,
        call_id: &str,
    ) -> Result<(), ExternalToolCatalogError> {
        self.inner.clear_pending_call(run_id, call_id).await
    }

    async fn clear(&self, run_id: TurnRunId) -> Result<(), ExternalToolCatalogError> {
        self.inner.clear(run_id).await
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An `ask_user` call's arguments as a form. Questions are keyed `q1`, `q2`,
/// ...; one without options takes free text. `None` when the arguments do
/// not make a valid form (the model then reads the run's refusal).
pub fn form_from_arguments(arguments: &Value) -> Option<QuestionForm> {
    let questions = arguments.get("questions")?.as_array()?;
    if questions.is_empty() || questions.len() > ASK_USER_MAX_QUESTIONS {
        return None;
    }
    let fields = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let options: Vec<QuestionOption> = q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    Some(QuestionOption {
                        label: text(o, "label")?,
                        description: text(o, "description"),
                        value: None,
                    })
                })
                .collect();
            let kind = if options.is_empty() {
                QuestionFieldKind::Text {
                    multiline: true,
                    default: None,
                    min_length: None,
                    max_length: None,
                }
            } else {
                QuestionFieldKind::Choice {
                    options,
                    multi: q.get("multi_select").and_then(Value::as_bool) == Some(true),
                    allow_other: q.get("allow_free_text").and_then(Value::as_bool) != Some(false),
                    min_items: None,
                    max_items: None,
                }
            };
            Some(QuestionField {
                id: format!("q{}", i + 1),
                label: text(q, "question")?,
                header: text(q, "header"),
                description: None,
                required: true,
                kind,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let form = QuestionForm {
        source: None,
        message: None,
        fields,
    };
    form.validate().ok().map(|()| form)
}

/// The run's parked `ask_user` call, if it is parked on one: its call id
/// and form.
pub async fn pending_question(
    catalog: &dyn ExternalToolCatalog,
    run_id: TurnRunId,
) -> Result<Option<(String, QuestionForm)>, ExternalToolCatalogError> {
    Ok(catalog
        .pending_calls(run_id)
        .await?
        .into_iter()
        .filter(|call| call.name() == ASK_USER_TOOL_NAME)
        .find_map(|call| {
            form_from_arguments(call.arguments()).map(|form| (call.call_id().to_string(), form))
        }))
}

/// What the model reads as the `ask_user` result: the answers (by question
/// and as one summary line each), or that the person declined.
pub fn tool_output(form: &QuestionForm, answer: Option<&QuestionAnswer>) -> Value {
    match answer {
        Some(answer) => {
            let by_question: serde_json::Map<String, Value> = form
                .fields
                .iter()
                .filter_map(|f| {
                    let value = serde_json::to_value(answer.values.get(&f.id)?).ok()?; // silent-ok: every answer variant serializes
                    Some((f.label.clone(), value))
                })
                .collect();
            json!({
                "answered": true,
                "answers": by_question,
                "summary": form.answer_summary(answer),
            })
        }
        None => json!({"answered": false, "message": QUESTION_DECLINED_MESSAGE}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::agent_plan::QuestionValue;
    use ironclaw_turns::InMemoryExternalToolCatalog;

    fn call() -> Value {
        json!({"questions": [
            {"question": "Which database?", "header": "DB",
             "options": [{"label": "Postgres", "description": "Shared"}, {"label": "SQLite"}]},
            {"question": "Anything else?"}
        ]})
    }

    #[test]
    fn the_spec_is_a_valid_external_tool() {
        let spec = ask_user_spec().unwrap();
        assert_eq!(spec.name(), ASK_USER_TOOL_NAME);
        assert!(spec.description().contains("structured question"));
    }

    #[tokio::test]
    async fn only_webui_runs_are_offered_ask_user() {
        let run = TurnRunId::new();
        let inner: Arc<dyn ExternalToolCatalog> = Arc::new(InMemoryExternalToolCatalog::new());
        let names = |catalog: Arc<dyn ExternalToolCatalog>| async move {
            catalog
                .specs(run)
                .await
                .unwrap()
                .iter()
                .map(|s| s.name().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(offer_ask_user(
                Arc::clone(&inner),
                Some(TurnOriginKind::WebUi)
            ))
            .await,
            vec![ASK_USER_TOOL_NAME]
        );
        for origin in [
            None,
            Some(TurnOriginKind::Inbound),
            Some(TurnOriginKind::ScheduledTrigger),
        ] {
            assert!(
                names(offer_ask_user(Arc::clone(&inner), origin))
                    .await
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn a_parked_call_reads_back_as_a_form_and_answers_as_output() {
        let run = TurnRunId::new();
        let catalog = offer_ask_user(
            Arc::new(InMemoryExternalToolCatalog::new()),
            Some(TurnOriginKind::WebUi),
        );
        assert!(
            pending_question(catalog.as_ref(), run)
                .await
                .unwrap()
                .is_none()
        );
        catalog
            .record_pending_call(
                run,
                PendingExternalCall::new("call_9", ASK_USER_TOOL_NAME, call()),
            )
            .await
            .unwrap();
        let (call_id, form) = pending_question(catalog.as_ref(), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call_id, "call_9");
        assert_eq!(form.fields.len(), 2);
        assert!(matches!(
            &form.fields[0].kind,
            QuestionFieldKind::Choice {
                allow_other: true,
                multi: false,
                ..
            }
        ));
        assert!(matches!(
            form.fields[1].kind,
            QuestionFieldKind::Text { .. }
        ));

        let mut answer = QuestionAnswer::default();
        answer
            .values
            .insert("q1".into(), QuestionValue::Choices(vec!["SQLite".into()]));
        answer
            .values
            .insert("q2".into(), QuestionValue::Text("no".into()));
        form.check_answer(&answer).unwrap();
        let output = tool_output(&form, Some(&answer));
        assert_eq!(output["answered"], true);
        assert_eq!(output["answers"]["Which database?"], json!(["SQLite"]));
        assert!(
            output["summary"]
                .as_str()
                .unwrap()
                .contains("Anything else?: no")
        );
        assert_eq!(tool_output(&form, None)["answered"], false);
    }

    #[test]
    fn bad_arguments_make_no_form() {
        assert!(form_from_arguments(&json!({})).is_none());
        assert!(form_from_arguments(&json!({"questions": []})).is_none());
        assert!(form_from_arguments(&json!({"questions": [{"header": "x"}]})).is_none());
        let five: Vec<Value> = (0..5)
            .map(|i| json!({"question": format!("{i}?")}))
            .collect();
        assert!(form_from_arguments(&json!({"questions": five})).is_none());
    }
}
