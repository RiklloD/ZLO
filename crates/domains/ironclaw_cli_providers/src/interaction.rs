//! Each CLI's plan and question dialects, folded into the shared
//! [`AgentPlan`] and [`QuestionForm`] shapes (and answers folded back). Pure:
//! the drivers feed wire JSON in and send what comes out.
//!
//! - Claude CLI: `TodoWrite` (`todos[{content,status,activeForm}]`) and
//!   `AskUserQuestion` (`questions[{question,header,options,multiSelect}]`);
//! - Codex app-server: `turn/plan/updated` (`plan[{step,status}]`) and
//!   `item/tool/requestUserInput` (`questions[{id,header,question,options,isOther}]`);
//! - ACP (Antigravity): `plan` session updates (`entries[{content,priority,status}]`)
//!   and its `interaction_*` permission requests, which are questions;
//! - MCP elicitation (`requestedSchema`, a flat JSON-schema object), from
//!   whichever client relays it.

use ironclaw_host_api::agent_plan::{
    AgentPlan, PlanEntry, PlanEntryPriority, PlanEntryStatus, QUESTION_DECLINED_MESSAGE,
    QuestionAnswer, QuestionField, QuestionFieldKind, QuestionForm, QuestionOption, QuestionValue,
};
use serde_json::{Map, Value, json};

/// The Claude CLI's checklist tool.
pub const CLAUDE_TODO_TOOL: &str = "TodoWrite";
/// The Claude CLI's question tool.
pub const CLAUDE_QUESTION_TOOL: &str = "AskUserQuestion";

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn status(v: &Value) -> PlanEntryStatus {
    v.get("status")
        .cloned()
        .and_then(|s| serde_json::from_value(s).ok()) // silent-ok: an unknown status reads as pending
        .unwrap_or(PlanEntryStatus::Pending)
}

fn now() -> ironclaw_host_api::Timestamp {
    chrono::Utc::now()
}

/// A `TodoWrite` call's input as a plan.
pub fn claude_todo_plan(input: &Value) -> Option<AgentPlan> {
    let todos = input.get("todos")?.as_array()?;
    let entries = todos.iter().filter_map(|t| {
        Some(PlanEntry::new(text(t, "content")?, status(t)).with_active_form(text(t, "activeForm")))
    });
    Some(AgentPlan::new(entries, None, now()))
}

/// Codex's `turn/plan/updated` params as a plan.
pub fn codex_plan(params: &Value) -> Option<AgentPlan> {
    let steps = params.get("plan")?.as_array()?;
    let entries = steps
        .iter()
        .filter_map(|s| Some(PlanEntry::new(text(s, "step")?, status(s))));
    Some(AgentPlan::new(entries, text(params, "explanation"), now()))
}

/// An ACP `plan` session update as a plan; `None` for other updates.
pub fn acp_plan(update: &Value) -> Option<AgentPlan> {
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("plan") {
        return None;
    }
    let entries = update.get("entries")?.as_array()?.iter().filter_map(|e| {
        let priority = e
            .get("priority")
            .cloned()
            .and_then(|p| serde_json::from_value::<PlanEntryPriority>(p).ok()); // silent-ok: priority is optional display data
        Some(PlanEntry::new(text(e, "content")?, status(e)).with_priority(priority))
    });
    Some(AgentPlan::new(entries, None, now()))
}

fn options(raw: Option<&Value>, label_key: &str) -> Vec<QuestionOption> {
    raw.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| {
            Some(QuestionOption {
                label: text(o, label_key)?,
                description: text(o, "description"),
                value: None,
            })
        })
        .collect()
}

/// An `AskUserQuestion` call's input as a form. Each question is keyed by
/// its own text (the CLI's answer convention); "Other" is always allowed,
/// as in the CLI's own picker.
pub fn claude_question_form(input: &Value) -> Option<QuestionForm> {
    let fields: Vec<QuestionField> = input
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|q| {
            let label = text(q, "question")?;
            Some(QuestionField {
                id: label.clone(),
                label,
                header: text(q, "header"),
                description: None,
                required: true,
                kind: QuestionFieldKind::Choice {
                    options: options(q.get("options"), "label"),
                    multi: q.get("multiSelect").and_then(Value::as_bool) == Some(true),
                    allow_other: true,
                    min_items: None,
                    max_items: None,
                },
            })
        })
        .collect();
    let form = QuestionForm {
        source: Some("Claude".into()),
        message: None,
        fields,
    };
    form.validate().ok().map(|()| form)
}

/// What the Claude CLI reads back for an answered or dismissed
/// `AskUserQuestion`. It arrives as the hook's decision reason, so it says
/// plainly that it is the answer, not a refusal.
pub fn claude_question_reply(form: &QuestionForm, answer: Option<&QuestionAnswer>) -> String {
    match answer {
        Some(answer) => format!(
            "Answered in ZLO's question form (this is the user's answer, not an error). {}",
            form.answer_summary(answer)
        ),
        None => QUESTION_DECLINED_MESSAGE.to_string(),
    }
}

/// Codex's `item/tool/requestUserInput` params as a form, keyed by each
/// question's `id`.
pub fn codex_question_form(params: &Value) -> Option<QuestionForm> {
    let fields: Vec<QuestionField> = params
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|q| {
            let id = text(q, "id")?;
            let options = options(q.get("options"), "label");
            let allow_other = q.get("isOther").and_then(Value::as_bool) == Some(true);
            let kind = if options.is_empty() {
                QuestionFieldKind::Text {
                    multiline: false,
                    default: None,
                    min_length: None,
                    max_length: None,
                }
            } else {
                QuestionFieldKind::Choice {
                    options,
                    multi: false,
                    allow_other,
                    min_items: None,
                    max_items: None,
                }
            };
            Some(QuestionField {
                label: text(q, "question").unwrap_or_else(|| id.clone()),
                id,
                header: text(q, "header"),
                description: None,
                required: true,
                kind,
            })
        })
        .collect();
    let form = QuestionForm {
        source: Some("Codex".into()),
        message: None,
        fields,
    };
    form.validate().ok().map(|()| form)
}

/// The `requestUserInput` response: every answer as a list of strings. A
/// dismissed form answers nothing.
pub fn codex_question_reply(answer: Option<&QuestionAnswer>) -> Value {
    let answers: Map<String, Value> = answer
        .map(|a| {
            a.values
                .iter()
                .map(|(id, v)| (id.clone(), json!({"answers": strings(v)})))
                .collect()
        })
        .unwrap_or_default();
    json!({"answers": answers})
}

fn strings(value: &QuestionValue) -> Vec<String> {
    match value {
        QuestionValue::Bool(b) => vec![b.to_string()],
        QuestionValue::Number(n) => vec![n.to_string()],
        QuestionValue::Text(t) => vec![t.clone()],
        QuestionValue::Choices(c) => c.clone(),
    }
}

/// An MCP elicitation's `requestedSchema` (a flat object schema) as a form.
/// `None` when the schema holds a property the protocol does not allow
/// (nested objects): the request is then declined, not half-asked.
pub fn elicitation_form(
    server: Option<String>,
    message: Option<String>,
    schema: &Value,
) -> Option<QuestionForm> {
    let props = schema.get("properties")?.as_object()?;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut fields = Vec::new();
    for (id, prop) in props {
        fields.push(QuestionField {
            id: id.clone(),
            label: text(prop, "title").unwrap_or_else(|| id.clone()),
            header: None,
            description: text(prop, "description"),
            required: required.contains(&id.as_str()),
            kind: elicitation_kind(prop)?,
        });
    }
    let form = QuestionForm {
        source: server,
        message,
        fields,
    };
    form.validate().ok().map(|()| form)
}

/// Options of a string enum: `enum` (+ legacy `enumNames`), or
/// `oneOf`/`anyOf` of `{const, title}`.
fn enum_options(prop: &Value) -> Option<Vec<QuestionOption>> {
    if let Some(values) = prop.get("enum").and_then(Value::as_array) {
        let names = prop.get("enumNames").and_then(Value::as_array);
        return Some(
            values
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let value = v.as_str()?.to_string();
                    let label = names
                        .and_then(|n| n.get(i))
                        .and_then(Value::as_str)
                        .map_or_else(|| value.clone(), str::to_string);
                    Some(QuestionOption {
                        label,
                        description: None,
                        value: Some(value),
                    })
                })
                .collect(),
        );
    }
    let list = prop
        .get("oneOf")
        .or_else(|| prop.get("anyOf"))?
        .as_array()?;
    Some(
        list.iter()
            .filter_map(|o| {
                let value = o.get("const")?.as_str()?.to_string();
                Some(QuestionOption {
                    label: text(o, "title").unwrap_or_else(|| value.clone()),
                    description: None,
                    value: Some(value),
                })
            })
            .collect(),
    )
}

/// A schema's count bound (`minLength`, `maxItems`, ...); `Err` when it is
/// there but not a count we can hold (then the request is declined).
fn count(prop: &Value, key: &str) -> Result<Option<u32>, ()> {
    match prop.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or(()),
    }
}

/// The field kind of one schema property. `None` — so the whole request is
/// declined rather than half-checked — for anything the form cannot draw or
/// enforce: nested objects, `pattern`, and `format` (email, uri, date, ...).
fn elicitation_kind(prop: &Value) -> Option<QuestionFieldKind> {
    if prop.get("pattern").is_some() || prop.get("format").is_some() {
        return None;
    }
    let number = |key| prop.get(key).and_then(Value::as_number).cloned();
    Some(match prop.get("type").and_then(Value::as_str)? {
        "string" => match enum_options(prop) {
            Some(options) => QuestionFieldKind::Choice {
                options,
                multi: false,
                allow_other: false,
                min_items: None,
                max_items: None,
            },
            None => QuestionFieldKind::Text {
                multiline: false,
                default: text(prop, "default"),
                min_length: count(prop, "minLength").ok()?,
                max_length: count(prop, "maxLength").ok()?,
            },
        },
        "array" => QuestionFieldKind::Choice {
            options: enum_options(prop.get("items")?)?,
            multi: true,
            allow_other: false,
            min_items: count(prop, "minItems").ok()?,
            max_items: count(prop, "maxItems").ok()?,
        },
        kind @ ("number" | "integer") => QuestionFieldKind::Number {
            integer: kind == "integer",
            minimum: number("minimum"),
            maximum: number("maximum"),
        },
        "boolean" => QuestionFieldKind::Boolean {
            default: prop.get("default").and_then(Value::as_bool),
        },
        _ => return None,
    })
}

/// The elicitation result: `accept` with the answers as the schema's
/// values (option values, not labels), or `decline`.
pub fn elicitation_reply(form: &QuestionForm, answer: Option<&QuestionAnswer>) -> Value {
    let Some(answer) = answer else {
        return json!({"action": "decline"});
    };
    let mut content = Map::new();
    for field in &form.fields {
        let Some(value) = answer.values.get(&field.id) else {
            continue;
        };
        let wire = match (&field.kind, value) {
            (QuestionFieldKind::Choice { options, multi, .. }, QuestionValue::Choices(picked)) => {
                let values: Vec<Value> = picked
                    .iter()
                    .map(|label| {
                        let found = options.iter().find(|o| &o.label == label);
                        Value::String(found.map_or(label.as_str(), |o| o.wire_value()).to_string())
                    })
                    .collect();
                if *multi {
                    Value::Array(values)
                } else {
                    values.into_iter().next().unwrap_or(Value::Null)
                }
            }
            (_, other) => serde_json::to_value(other).unwrap_or(Value::Null), // silent-ok: every answer variant serializes
        };
        content.insert(field.id.clone(), wire);
    }
    json!({"action": "accept", "content": content})
}

/// ACP option kinds that grant or refuse for good; never offered on a form.
const ACP_PERSISTENT_KINDS: [&str; 2] = ["allow_always", "reject_always"];

/// Whether an ACP tool call looks like a question rather than an action: a
/// question id, a kind that names no effect, and nothing to act on (no raw
/// input, locations, diffs or terminals).
fn acp_is_question(call: &Value) -> bool {
    let id = call.get("toolCallId").and_then(Value::as_str);
    let kind = call.get("kind").and_then(Value::as_str);
    let empty = |key: &str| {
        call.get(key)
            .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty))
    };
    let acts = call
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|c| {
            matches!(
                c.get("type").and_then(Value::as_str),
                Some("diff" | "terminal")
            )
        });
    id.is_some_and(|id| id.starts_with("interaction_"))
        && matches!(kind, None | Some("other" | "think"))
        && empty("rawInput")
        && empty("locations")
        && !acts
}

/// Antigravity asks the person with a permission request whose call id is
/// `interaction_*`: its options are the answers. One single-choice question.
/// `None` unless the request passes [`acp_is_question`] and offers no
/// persistent option: anything else is an approval, so a question card can
/// never grant a call (let alone for good) that an approval would show.
pub fn acp_interaction_form(params: &Value) -> Option<QuestionForm> {
    let call = params.get("toolCall")?;
    if !acp_is_question(call) {
        return None;
    }
    let options = params.get("options")?.as_array()?;
    if options.iter().any(|o| {
        o.get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| ACP_PERSISTENT_KINDS.contains(&k))
    }) {
        return None;
    }
    let label = text(call, "title").unwrap_or_else(|| "Antigravity asks".to_string());
    let form = QuestionForm {
        source: Some("Antigravity".into()),
        message: None,
        fields: vec![QuestionField {
            id: "answer".into(),
            label,
            header: None,
            description: None,
            required: true,
            kind: QuestionFieldKind::Choice {
                options: options
                    .iter()
                    .filter_map(|o| {
                        Some(QuestionOption {
                            label: text(o, "name")?,
                            description: None,
                            value: text(o, "optionId"),
                        })
                    })
                    .collect(),
                multi: false,
                allow_other: false,
                min_items: None,
                max_items: None,
            },
        }],
    };
    form.validate().ok().map(|()| form)
}

/// The ACP permission outcome for an answered interaction, or `cancelled`.
pub fn acp_interaction_reply(form: &QuestionForm, answer: Option<&QuestionAnswer>) -> Value {
    let picked = answer
        .and_then(|a| a.values.get("answer"))
        .and_then(|v| match v {
            QuestionValue::Choices(c) => c.first(),
            _ => None,
        });
    let option = picked.and_then(|label| {
        form.fields.first().and_then(|f| match &f.kind {
            QuestionFieldKind::Choice { options, .. } => options
                .iter()
                .find(|o| &o.label == label)
                .map(|o| o.wire_value().to_string()),
            _ => None,
        })
    });
    match option {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(pairs: &[(&str, QuestionValue)]) -> QuestionAnswer {
        QuestionAnswer {
            values: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn todo_write_becomes_a_plan() {
        // Recorded TodoWrite input shape (Claude CLI 2.1).
        let input = json!({"todos": [
            {"content": "Read the bridge", "status": "completed", "activeForm": "Reading the bridge"},
            {"content": "Add plan events", "status": "in_progress", "activeForm": "Adding plan events"},
            {"content": "Write tests", "status": "pending", "activeForm": "Writing tests"},
        ]});
        let plan = claude_todo_plan(&input).unwrap();
        assert_eq!(plan.progress(), (1, 3));
        let current = plan.current().unwrap();
        assert_eq!(current.content, "Add plan events");
        assert_eq!(current.active_form.as_deref(), Some("Adding plan events"));
        assert!(claude_todo_plan(&json!({"nope": 1})).is_none());
    }

    #[test]
    fn codex_and_acp_plans_fold_their_spellings() {
        let codex = codex_plan(&json!({
            "threadId": "t", "turnId": "u", "explanation": "Split the work",
            "plan": [{"step": "One", "status": "completed"}, {"step": "Two", "status": "inProgress"}]
        }))
        .unwrap();
        assert_eq!(codex.progress(), (1, 2));
        assert_eq!(codex.explanation.as_deref(), Some("Split the work"));
        assert_eq!(codex.current().unwrap().content, "Two");

        let acp = acp_plan(&json!({"sessionUpdate": "plan", "entries": [
            {"content": "Look", "priority": "high", "status": "in_progress"},
            {"content": "Fix", "priority": "low", "status": "pending"}
        ]}))
        .unwrap();
        assert_eq!(acp.entries[0].priority, Some(PlanEntryPriority::High));
        assert_eq!(acp.current().unwrap().content, "Look");
        assert!(acp_plan(&json!({"sessionUpdate": "agent_message_chunk"})).is_none());
    }

    #[test]
    fn ask_user_question_becomes_a_form_and_its_answer_a_reply() {
        // Recorded AskUserQuestion input shape (Claude CLI 2.1).
        let input = json!({"questions": [{
            "question": "Which library should we use?",
            "header": "Library",
            "multiSelect": false,
            "options": [
                {"label": "serde", "description": "The standard"},
                {"label": "miniserde", "description": "Smaller"}
            ]
        }]});
        let form = claude_question_form(&input).unwrap();
        let field = &form.fields[0];
        assert_eq!(field.header.as_deref(), Some("Library"));
        assert!(matches!(
            &field.kind,
            QuestionFieldKind::Choice { options, multi: false, allow_other: true, .. } if options.len() == 2
        ));
        let a = answer(&[(
            "Which library should we use?",
            QuestionValue::Choices(vec!["serde".into()]),
        )]);
        form.check_answer(&a).unwrap();
        let reply = claude_question_reply(&form, Some(&a));
        assert!(reply.contains("not an error"), "{reply}");
        assert!(
            reply.contains("Which library should we use?: serde"),
            "{reply}"
        );
        assert_eq!(
            claude_question_reply(&form, None),
            QUESTION_DECLINED_MESSAGE
        );
        assert!(claude_question_form(&json!({"questions": []})).is_none());
    }

    #[test]
    fn elicitation_schema_becomes_fields_and_answers_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "title": "Your name"},
                "plan": {"type": "string", "enum": ["free", "pro"], "enumNames": ["Free", "Pro"]},
                "seats": {"type": "integer", "minimum": 1, "maximum": 50},
                "agree": {"type": "boolean", "default": false},
                "tags": {"type": "array", "items": {"anyOf": [{"const": "a", "title": "Alpha"}, {"const": "b", "title": "Beta"}]}}
            },
            "required": ["name", "plan"]
        });
        let form = elicitation_form(Some("github".into()), Some("Set up".into()), &schema).unwrap();
        assert_eq!(form.fields.len(), 5);
        assert!(form.field("name").unwrap().required);
        assert!(!form.field("seats").unwrap().required);
        let a = answer(&[
            ("name", QuestionValue::Text("Ada".into())),
            ("plan", QuestionValue::Choices(vec!["Pro".into()])),
            ("seats", QuestionValue::Number(3.into())),
            ("agree", QuestionValue::Bool(true)),
            ("tags", QuestionValue::Choices(vec!["Beta".into()])),
        ]);
        form.check_answer(&a).unwrap();
        assert_eq!(
            elicitation_reply(&form, Some(&a)),
            json!({"action": "accept", "content": {
                "name": "Ada", "plan": "pro", "seats": 3, "agree": true, "tags": ["b"]
            }})
        );
        assert_eq!(elicitation_reply(&form, None), json!({"action": "decline"}));
        let nested = json!({"type": "object", "properties": {"o": {"type": "object"}}});
        assert!(elicitation_form(None, None, &nested).is_none());
    }

    // Review findings 4-6, through the provider parser: what the form cannot
    // bound or enforce is declined, and what it can is enforced.
    #[test]
    fn elicitation_constraints_are_enforced_or_the_request_declined() {
        let schema = json!({"type": "object", "required": ["code", "tags"], "properties": {
            "code": {"type": "string", "minLength": 10, "maxLength": 12},
            "tags": {"type": "array", "minItems": 1, "maxItems": 2,
                     "items": {"enum": ["a", "b", "c"]}}
        }});
        let form = elicitation_form(None, None, &schema).unwrap();
        let ask = |code: &str, tags: &[&str]| QuestionAnswer {
            values: [
                ("code".to_string(), QuestionValue::Text(code.into())),
                (
                    "tags".to_string(),
                    QuestionValue::Choices(tags.iter().map(|t| t.to_string()).collect()),
                ),
            ]
            .into(),
        };
        assert!(
            form.check_answer(&ask("x", &["a"])).is_err(),
            "minLength 10"
        );
        assert!(
            form.check_answer(&ask("0123456789", &[])).is_err(),
            "minItems 1"
        );
        assert!(
            form.check_answer(&ask("0123456789", &["a", "b", "c"]))
                .is_err(),
            "maxItems 2"
        );
        form.check_answer(&ask("0123456789", &["a", "c"])).unwrap();

        for prop in [
            json!({"type": "string", "pattern": "^[0-9]+$"}),
            json!({"type": "string", "format": "email"}),
            json!({"type": "string", "minLength": -1}),
            json!({"type": "string", "default": "d".repeat(9_000)}),
            json!({"type": "string", "enum": ["a", "b"], "enumNames": ["Same", "Same"]}),
            json!({"type": "string", "enum": ["a", "v".repeat(3_000)]}),
        ] {
            let schema = json!({"type": "object", "properties": {"f": prop.clone()}});
            assert!(elicitation_form(None, None, &schema).is_none(), "{prop}");
        }
    }

    #[test]
    fn codex_request_user_input_round_trips() {
        let params = json!({"threadId": "t", "turnId": "u", "itemId": "i", "questions": [
            {"id": "scope", "header": "Scope", "question": "How far?", "isOther": true,
             "options": [{"label": "Just this", "description": ""}, {"label": "Everything"}]},
            {"id": "name", "question": "Name it?"}
        ]});
        let form = codex_question_form(&params).unwrap();
        assert!(matches!(
            form.fields[1].kind,
            QuestionFieldKind::Text { .. }
        ));
        let a = answer(&[
            ("scope", QuestionValue::Choices(vec!["Everything".into()])),
            ("name", QuestionValue::Text("zlo".into())),
        ]);
        assert_eq!(
            codex_question_reply(Some(&a)),
            json!({"answers": {"scope": {"answers": ["Everything"]}, "name": {"answers": ["zlo"]}}})
        );
        assert_eq!(codex_question_reply(None), json!({"answers": {}}));
    }

    #[test]
    fn antigravity_interactions_select_the_picked_option() {
        let params = json!({
            "toolCall": {"toolCallId": "interaction_1", "title": "Proceed with the refactor?"},
            "options": [
                {"optionId": "opt-yes", "name": "Yes", "kind": "allow_once"},
                {"optionId": "opt-no", "name": "No", "kind": "reject_once"}
            ]
        });
        let form = acp_interaction_form(&params).unwrap();
        assert_eq!(form.fields[0].label, "Proceed with the refactor?");
        let a = answer(&[("answer", QuestionValue::Choices(vec!["No".into()]))]);
        assert_eq!(
            acp_interaction_reply(&form, Some(&a)),
            json!({"outcome": {"outcome": "selected", "optionId": "opt-no"}})
        );
        assert_eq!(
            acp_interaction_reply(&form, None),
            json!({"outcome": {"outcome": "cancelled"}})
        );
    }
}
