//! An agent's working plan and the structured questions it asks the person.
//!
//! Every runtime that can plan or ask speaks a dialect of its own: the Claude
//! CLI's `TodoWrite` and `AskUserQuestion` tools, Codex's
//! `turn/plan/updated` notification, ACP `plan` session updates, MCP
//! elicitation requests, and IronClaw's own `builtin.plan_update` /
//! `builtin.ask_user` capabilities. Each driver folds its dialect into the
//! one shape here, so the product projection and the desktop app draw one
//! checklist and one form.
//!
//! Shape validation and bounding live with the types; vendor parsing stays
//! with each driver.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Timestamp;

/// Most entries a plan keeps; the rest are dropped.
pub const PLAN_MAX_ENTRIES: usize = 64;
/// Bound on one entry's text, in characters.
pub const PLAN_ENTRY_MAX_CHARS: usize = 500;
/// Most fields one question form carries.
pub const QUESTION_MAX_FIELDS: usize = 16;
/// Most options one choice field offers.
pub const QUESTION_MAX_OPTIONS: usize = 32;
/// Bound on question, label and option text, in characters.
pub const QUESTION_TEXT_MAX_CHARS: usize = 2_000;
/// Bound on one free-text answer, in characters.
pub const ANSWER_TEXT_MAX_CHARS: usize = 8_000;
/// Prefix of every question gate ref (`gate:question-<id>`): a gate
/// resolution for one carries answers, not an approval.
pub const QUESTION_GATE_REF_PREFIX: &str = "gate:question-";
/// What the agent reads when the person dismisses a question form.
pub const QUESTION_DECLINED_MESSAGE: &str = "The user declined to answer. Do not ask the same question again; continue with your best judgment or ask what they want instead.";

/// Where one plan entry stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    /// `inProgress` is Codex's spelling.
    #[serde(alias = "inProgress", alias = "in-progress", alias = "active")]
    InProgress,
    #[serde(alias = "done", alias = "complete")]
    Completed,
}

/// How much an entry matters (ACP carries it; others leave it out).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanEntryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PlanEntryPriority>,
    /// The step as it reads while it runs ("Running the tests"), when the
    /// agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

impl PlanEntry {
    pub fn new(content: impl Into<String>, status: PlanEntryStatus) -> Self {
        Self {
            content: content.into(),
            status,
            priority: None,
            active_form: None,
        }
    }

    pub fn with_priority(mut self, priority: Option<PlanEntryPriority>) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_active_form(mut self, active_form: Option<String>) -> Self {
        self.active_form = active_form;
        self
    }
}

/// An agent's whole plan as of `updated_at`. Every update replaces the
/// previous plan: the runtimes all send the full list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPlan {
    pub entries: Vec<PlanEntry>,
    /// Why the plan changed, when the agent said (Codex sends one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    pub updated_at: Timestamp,
}

impl AgentPlan {
    /// A bounded plan: blank entries are dropped, at most
    /// [`PLAN_MAX_ENTRIES`] are kept and every text is cut to
    /// [`PLAN_ENTRY_MAX_CHARS`].
    pub fn new(
        entries: impl IntoIterator<Item = PlanEntry>,
        explanation: Option<String>,
        updated_at: Timestamp,
    ) -> Self {
        let entries = entries
            .into_iter()
            .filter_map(|entry| {
                let content = bounded(entry.content.trim(), PLAN_ENTRY_MAX_CHARS);
                (!content.is_empty()).then(|| PlanEntry {
                    content,
                    active_form: entry
                        .active_form
                        .map(|a| bounded(a.trim(), PLAN_ENTRY_MAX_CHARS))
                        .filter(|a| !a.is_empty()),
                    ..entry
                })
            })
            .take(PLAN_MAX_ENTRIES)
            .collect();
        Self {
            entries,
            explanation: explanation
                .map(|e| bounded(e.trim(), PLAN_ENTRY_MAX_CHARS))
                .filter(|e| !e.is_empty()),
            updated_at,
        }
    }

    /// `(completed, total)`.
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .entries
            .iter()
            .filter(|e| e.status == PlanEntryStatus::Completed)
            .count();
        (done, self.entries.len())
    }

    /// The entry being worked on: the first in progress.
    pub fn current(&self) -> Option<&PlanEntry> {
        self.entries
            .iter()
            .find(|e| e.status == PlanEntryStatus::InProgress)
    }

    /// Every entry is done (an empty plan is not).
    pub fn is_complete(&self) -> bool {
        !self.entries.is_empty()
            && self
                .entries
                .iter()
                .all(|e| e.status == PlanEntryStatus::Completed)
    }

    /// Whether the shape holds the bounds [`Self::new`] applies; for plans
    /// that arrive over the wire.
    pub fn is_bounded(&self) -> bool {
        self.entries.len() <= PLAN_MAX_ENTRIES
            && self.entries.iter().all(|e| {
                within(&e.content, PLAN_ENTRY_MAX_CHARS)
                    && e.active_form
                        .as_deref()
                        .is_none_or(|a| within(a, PLAN_ENTRY_MAX_CHARS))
            })
            && self
                .explanation
                .as_deref()
                .is_none_or(|e| within(e, PLAN_ENTRY_MAX_CHARS))
    }
}

/// A form an agent puts to the person: one or more questions, answered
/// together. The run waits on it like on an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionForm {
    /// Who is asking, as the person should read it ("Claude", an MCP
    /// server's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The overall request, when there is one beyond the questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub fields: Vec<QuestionField>,
}

/// One question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionField {
    /// The key its answer goes under; unique in the form.
    pub id: String,
    /// The question as the person reads it.
    pub label: String,
    /// A short tag ("Auth method").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    pub kind: QuestionFieldKind,
}

/// What kind of answer a question takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionFieldKind {
    /// Pick one (or several, with `multi`) of `options`; with `allow_other`
    /// the person may type their own instead.
    Choice {
        options: Vec<QuestionOption>,
        #[serde(default)]
        multi: bool,
        #[serde(default)]
        allow_other: bool,
    },
    Text {
        #[serde(default)]
        multiline: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    Number {
        #[serde(default)]
        integer: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<serde_json::Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<serde_json::Number>,
    },
    Boolean {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
}

/// One offered answer to a choice question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The value the asker wants back when it differs from the label (an
    /// MCP enum value under its display name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl QuestionOption {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
            value: None,
        }
    }

    /// What goes back to the asker when this option is picked.
    pub fn wire_value(&self) -> &str {
        self.value.as_deref().unwrap_or(&self.label)
    }
}

/// One answered question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuestionValue {
    Bool(bool),
    Number(serde_json::Number),
    Text(String),
    /// Picked options by label (or the typed "Other" text), in order.
    Choices(Vec<String>),
}

/// The person's answers, by field id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub values: BTreeMap<String, QuestionValue>,
}

/// Why a form or an answer does not hold.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QuestionFormError {
    #[error("the form has no questions")]
    Empty,
    #[error("the form has too many questions")]
    TooManyFields,
    #[error("question `{0}` appears twice")]
    DuplicateField(String),
    #[error("question `{0}` is invalid: {1}")]
    InvalidField(String, &'static str),
    #[error("question `{0}` needs an answer")]
    MissingAnswer(String),
    #[error("the answer to `{0}` does not fit the question: {1}")]
    InvalidAnswer(String, &'static str),
    #[error("`{0}` is not a question in this form")]
    UnknownField(String),
}

impl QuestionForm {
    pub fn validate(&self) -> Result<(), QuestionFormError> {
        if self.fields.is_empty() {
            return Err(QuestionFormError::Empty);
        }
        if self.fields.len() > QUESTION_MAX_FIELDS {
            return Err(QuestionFormError::TooManyFields);
        }
        let texts = [self.source.as_deref(), self.message.as_deref()];
        if texts
            .iter()
            .flatten()
            .any(|t| !within(t, QUESTION_TEXT_MAX_CHARS))
        {
            return Err(QuestionFormError::InvalidField(
                String::new(),
                "text too long",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for field in &self.fields {
            if !seen.insert(field.id.as_str()) {
                return Err(QuestionFormError::DuplicateField(field.id.clone()));
            }
            field.validate()?;
        }
        Ok(())
    }

    pub fn field(&self, id: &str) -> Option<&QuestionField> {
        self.fields.iter().find(|f| f.id == id)
    }

    /// Whether `answer` fits: every required question answered, each value
    /// of the question's kind, choices among the options unless "Other" is
    /// allowed, and nothing answered that was not asked.
    pub fn check_answer(&self, answer: &QuestionAnswer) -> Result<(), QuestionFormError> {
        for id in answer.values.keys() {
            if self.field(id).is_none() {
                return Err(QuestionFormError::UnknownField(id.clone()));
            }
        }
        for field in &self.fields {
            match answer.values.get(&field.id) {
                Some(value) => field.check(value)?,
                None if field.required => {
                    return Err(QuestionFormError::MissingAnswer(field.id.clone()));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// The answers as the agent reads them: one `question: answer` line per
    /// question, unanswered ones marked.
    pub fn answer_summary(&self, answer: &QuestionAnswer) -> String {
        let mut out = String::from("The user answered:");
        for field in &self.fields {
            let shown = match answer.values.get(&field.id) {
                Some(QuestionValue::Bool(b)) => if *b { "yes" } else { "no" }.to_string(),
                Some(QuestionValue::Number(n)) => n.to_string(),
                Some(QuestionValue::Text(t)) => t.clone(),
                Some(QuestionValue::Choices(c)) => c.join(", "),
                None => "(no answer)".to_string(),
            };
            out.push_str(&format!("\n- {}: {}", field.label, shown));
        }
        out
    }
}

impl QuestionField {
    fn invalid(&self, why: &'static str) -> QuestionFormError {
        QuestionFormError::InvalidField(self.id.clone(), why)
    }

    fn validate(&self) -> Result<(), QuestionFormError> {
        if self.id.trim().is_empty() || !within(&self.id, QUESTION_TEXT_MAX_CHARS) {
            return Err(self.invalid("bad id"));
        }
        if self.label.trim().is_empty() {
            return Err(self.invalid("empty question"));
        }
        let texts = [
            Some(self.label.as_str()),
            self.header.as_deref(),
            self.description.as_deref(),
        ];
        if texts
            .iter()
            .flatten()
            .any(|t| !within(t, QUESTION_TEXT_MAX_CHARS))
        {
            return Err(self.invalid("text too long"));
        }
        if let QuestionFieldKind::Choice {
            options,
            allow_other,
            ..
        } = &self.kind
        {
            if options.len() > QUESTION_MAX_OPTIONS {
                return Err(self.invalid("too many options"));
            }
            if options.is_empty() && !allow_other {
                return Err(self.invalid("no options"));
            }
            if options.iter().any(|o| {
                o.label.trim().is_empty()
                    || !within(&o.label, QUESTION_TEXT_MAX_CHARS)
                    || o.description
                        .as_deref()
                        .is_some_and(|d| !within(d, QUESTION_TEXT_MAX_CHARS))
            }) {
                return Err(self.invalid("bad option"));
            }
        }
        Ok(())
    }

    fn check(&self, value: &QuestionValue) -> Result<(), QuestionFormError> {
        let wrong = |why| Err(QuestionFormError::InvalidAnswer(self.id.clone(), why));
        match (&self.kind, value) {
            (
                QuestionFieldKind::Choice {
                    options,
                    multi,
                    allow_other,
                },
                QuestionValue::Choices(picked),
            ) => {
                if picked.is_empty() && self.required {
                    return wrong("nothing picked");
                }
                if picked.len() > 1 && !multi {
                    return wrong("only one may be picked");
                }
                for choice in picked {
                    if !within(choice, ANSWER_TEXT_MAX_CHARS) {
                        return wrong("answer too long");
                    }
                    if !allow_other && !options.iter().any(|o| &o.label == choice) {
                        return wrong("not one of the options");
                    }
                }
                Ok(())
            }
            (QuestionFieldKind::Text { .. }, QuestionValue::Text(text)) => {
                if self.required && text.trim().is_empty() {
                    return wrong("empty");
                }
                if !within(text, ANSWER_TEXT_MAX_CHARS) {
                    return wrong("answer too long");
                }
                Ok(())
            }
            (
                QuestionFieldKind::Number {
                    integer,
                    minimum,
                    maximum,
                },
                QuestionValue::Number(n),
            ) => {
                if *integer && !(n.is_i64() || n.is_u64()) {
                    return wrong("not a whole number");
                }
                let v = n.as_f64().unwrap_or(f64::NAN);
                if minimum
                    .as_ref()
                    .and_then(|m| m.as_f64())
                    .is_some_and(|m| v < m)
                    || maximum
                        .as_ref()
                        .and_then(|m| m.as_f64())
                        .is_some_and(|m| v > m)
                {
                    return wrong("out of range");
                }
                Ok(())
            }
            (QuestionFieldKind::Boolean { .. }, QuestionValue::Bool(_)) => Ok(()),
            _ => wrong("wrong kind of answer"),
        }
    }
}

fn within(text: &str, max_chars: usize) -> bool {
    text.chars().count() <= max_chars
}

fn bounded(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn choice_form() -> QuestionForm {
        QuestionForm {
            source: Some("Claude".into()),
            message: None,
            fields: vec![QuestionField {
                id: "Which database?".into(),
                label: "Which database?".into(),
                header: Some("Database".into()),
                description: None,
                required: true,
                kind: QuestionFieldKind::Choice {
                    options: vec![
                        QuestionOption::new("Postgres"),
                        QuestionOption::new("SQLite"),
                    ],
                    multi: false,
                    allow_other: true,
                },
            }],
        }
    }

    #[test]
    fn plan_status_reads_every_runtime_spelling() {
        for (raw, want) in [
            ("pending", PlanEntryStatus::Pending),
            ("in_progress", PlanEntryStatus::InProgress),
            ("inProgress", PlanEntryStatus::InProgress),
            ("completed", PlanEntryStatus::Completed),
        ] {
            let got: PlanEntryStatus = serde_json::from_value(json!(raw)).unwrap();
            assert_eq!(got, want, "{raw}");
        }
        assert_eq!(
            serde_json::to_value(PlanEntryStatus::InProgress).unwrap(),
            json!("in_progress")
        );
    }

    #[test]
    fn a_plan_is_bounded_and_reports_progress() {
        let long = "x".repeat(PLAN_ENTRY_MAX_CHARS + 10);
        let entries = [
            PlanEntry::new("Read the code", PlanEntryStatus::Completed),
            PlanEntry::new("  ", PlanEntryStatus::Pending),
            PlanEntry::new(long, PlanEntryStatus::InProgress)
                .with_active_form(Some("Writing it".into())),
            PlanEntry::new("Ship", PlanEntryStatus::Pending),
        ];
        let plan = AgentPlan::new(entries, None, chrono::Utc::now());
        assert_eq!(plan.entries.len(), 3, "the blank entry is dropped");
        assert_eq!(plan.progress(), (1, 3));
        assert_eq!(
            plan.current().map(|e| e.content.chars().count()),
            Some(PLAN_ENTRY_MAX_CHARS)
        );
        assert!(plan.is_bounded());
        assert!(!plan.is_complete());
        let many = (0..PLAN_MAX_ENTRIES + 5)
            .map(|i| PlanEntry::new(format!("step {i}"), PlanEntryStatus::Completed));
        let plan = AgentPlan::new(many, None, chrono::Utc::now());
        assert_eq!(plan.entries.len(), PLAN_MAX_ENTRIES);
        assert!(plan.is_complete());
    }

    #[test]
    fn answers_are_checked_against_the_form() {
        let form = choice_form();
        form.validate().unwrap();
        let mut answer = QuestionAnswer::default();
        assert_eq!(
            form.check_answer(&answer),
            Err(QuestionFormError::MissingAnswer("Which database?".into()))
        );
        answer.values.insert(
            "Which database?".into(),
            QuestionValue::Choices(vec!["Postgres".into(), "SQLite".into()]),
        );
        assert!(form.check_answer(&answer).is_err(), "single choice");
        answer.values.insert(
            "Which database?".into(),
            QuestionValue::Choices(vec!["DuckDB".into()]),
        );
        form.check_answer(&answer).unwrap();
        assert_eq!(
            form.answer_summary(&answer),
            "The user answered:\n- Which database?: DuckDB"
        );
        answer
            .values
            .insert("Which database?".into(), QuestionValue::Bool(true));
        assert!(form.check_answer(&answer).is_err(), "wrong kind");
        answer.values.clear();
        answer
            .values
            .insert("nope".into(), QuestionValue::Text("x".into()));
        assert_eq!(
            form.check_answer(&answer),
            Err(QuestionFormError::UnknownField("nope".into()))
        );
    }

    #[test]
    fn forms_and_answers_round_trip_on_the_wire() {
        let form = choice_form();
        let wire = serde_json::to_value(&form).unwrap();
        assert_eq!(wire["fields"][0]["kind"]["type"], "choice");
        assert_eq!(serde_json::from_value::<QuestionForm>(wire).unwrap(), form);
        let answer: QuestionAnswer = serde_json::from_value(json!({"values": {
            "a": true, "b": 3, "c": "free text", "d": ["Postgres"]
        }}))
        .unwrap();
        assert_eq!(answer.values["a"], QuestionValue::Bool(true));
        assert!(matches!(answer.values["b"], QuestionValue::Number(_)));
        assert_eq!(answer.values["c"], QuestionValue::Text("free text".into()));
        assert_eq!(
            answer.values["d"],
            QuestionValue::Choices(vec!["Postgres".into()])
        );
    }

    #[test]
    fn number_answers_respect_integer_and_range() {
        let form = QuestionForm {
            source: None,
            message: None,
            fields: vec![QuestionField {
                id: "n".into(),
                label: "How many?".into(),
                header: None,
                description: None,
                required: true,
                kind: QuestionFieldKind::Number {
                    integer: true,
                    minimum: Some(1.into()),
                    maximum: Some(10.into()),
                },
            }],
        };
        let answer = |v: serde_json::Value| QuestionAnswer {
            values: [("n".to_string(), serde_json::from_value(v).unwrap())].into(),
        };
        form.check_answer(&answer(json!(5))).unwrap();
        assert!(form.check_answer(&answer(json!(0))).is_err());
        assert!(form.check_answer(&answer(json!(2.5))).is_err());
    }
}
