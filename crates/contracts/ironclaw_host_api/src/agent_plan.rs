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
/// Bound on a whole form, serialized: every string has its own bound, and
/// this caps what many bounded strings add up to.
pub const QUESTION_FORM_MAX_BYTES: usize = 64 * 1024;
/// Bound on a whole answer, serialized.
pub const QUESTION_ANSWER_MAX_BYTES: usize = 64 * 1024;
/// `output_kind` of a capability display preview whose `output_preview` is
/// an [`AgentPlan`] (`builtin.plan_update`'s result).
pub const AGENT_PLAN_OUTPUT_KIND: &str = "agent_plan";
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
        /// Fewest picks a multi-choice answer may have.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_items: Option<u32>,
        /// Most picks a multi-choice answer may have.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_items: Option<u32>,
    },
    Text {
        #[serde(default)]
        multiline: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        /// Length bounds of the answer, in characters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<u32>,
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
    #[error("the form or answer is too large")]
    TooLarge,
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
        if serialized_len(self) > QUESTION_FORM_MAX_BYTES {
            return Err(QuestionFormError::TooLarge);
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
        if serialized_len(answer) > QUESTION_ANSWER_MAX_BYTES {
            return Err(QuestionFormError::TooLarge);
        }
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
        match &self.kind {
            QuestionFieldKind::Choice {
                options,
                multi,
                allow_other,
                min_items,
                max_items,
            } => {
                if options.len() > QUESTION_MAX_OPTIONS {
                    return Err(self.invalid("too many options"));
                }
                if options.is_empty() && !allow_other {
                    return Err(self.invalid("no options"));
                }
                let short = |t: &Option<String>| {
                    t.as_deref()
                        .is_none_or(|t| within(t, QUESTION_TEXT_MAX_CHARS))
                };
                if options.iter().any(|o| {
                    o.label.trim().is_empty()
                        || !within(&o.label, QUESTION_TEXT_MAX_CHARS)
                        || !short(&o.description)
                        || !short(&o.value)
                }) {
                    return Err(self.invalid("bad option"));
                }
                // The label is what the person picks: it names one option.
                let mut labels = std::collections::BTreeSet::new();
                if !options.iter().all(|o| labels.insert(o.label.as_str())) {
                    return Err(self.invalid("two options share a label"));
                }
                let most = self.most_picks(options.len(), *multi, *allow_other);
                if (min_items.is_some() || max_items.is_some()) && !multi {
                    return Err(self.invalid("pick counts only apply to multiple choice"));
                }
                let (min, max) = (min_items.unwrap_or(0), max_items.unwrap_or(u32::MAX));
                if min > max || usize::try_from(min).unwrap_or(usize::MAX) > most {
                    return Err(self.invalid("no answer can meet the pick counts"));
                }
            }
            QuestionFieldKind::Text {
                default,
                min_length,
                max_length,
                ..
            } => {
                if default
                    .as_deref()
                    .is_some_and(|d| !within(d, ANSWER_TEXT_MAX_CHARS))
                {
                    return Err(self.invalid("default too long"));
                }
                let max_allowed = u32::try_from(ANSWER_TEXT_MAX_CHARS).unwrap_or(u32::MAX);
                let (min, max) = (min_length.unwrap_or(0), max_length.unwrap_or(max_allowed));
                if min > max || min > max_allowed {
                    return Err(self.invalid("no answer can meet the length bounds"));
                }
            }
            QuestionFieldKind::Number { .. } | QuestionFieldKind::Boolean { .. } => {}
        }
        Ok(())
    }

    /// Most picks an answer may hold: each option once, plus one "Other".
    fn most_picks(&self, options: usize, multi: bool, allow_other: bool) -> usize {
        if multi {
            options + usize::from(allow_other)
        } else {
            1
        }
    }

    fn check(&self, value: &QuestionValue) -> Result<(), QuestionFormError> {
        let wrong = |why| Err(QuestionFormError::InvalidAnswer(self.id.clone(), why));
        match (&self.kind, value) {
            (
                QuestionFieldKind::Choice {
                    options,
                    multi,
                    allow_other,
                    min_items,
                    max_items,
                },
                QuestionValue::Choices(picked),
            ) => {
                if picked.is_empty() && self.required {
                    return wrong("nothing picked");
                }
                if picked.len() > 1 && !multi {
                    return wrong("only one may be picked");
                }
                if picked.len() > self.most_picks(options.len(), *multi, *allow_other) {
                    return wrong("too many picks");
                }
                let count = u32::try_from(picked.len()).unwrap_or(u32::MAX);
                if min_items.is_some_and(|m| count < m) || max_items.is_some_and(|m| count > m) {
                    return wrong("wrong number of picks");
                }
                let mut seen = std::collections::BTreeSet::new();
                let mut others = 0;
                for choice in picked {
                    if !within(choice, ANSWER_TEXT_MAX_CHARS) {
                        return wrong("answer too long");
                    }
                    if !seen.insert(choice.as_str()) {
                        return wrong("picked twice");
                    }
                    if !options.iter().any(|o| &o.label == choice) {
                        others += 1;
                        if !allow_other {
                            return wrong("not one of the options");
                        }
                        if others > 1 {
                            return wrong("only one Other answer");
                        }
                    }
                }
                Ok(())
            }
            (
                QuestionFieldKind::Text {
                    min_length,
                    max_length,
                    ..
                },
                QuestionValue::Text(text),
            ) => {
                if self.required && text.trim().is_empty() {
                    return wrong("empty");
                }
                if !within(text, ANSWER_TEXT_MAX_CHARS) {
                    return wrong("answer too long");
                }
                let len = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
                // An optional field left empty is not held to its bounds.
                if !text.is_empty()
                    && (min_length.is_some_and(|m| len < m) || max_length.is_some_and(|m| len > m))
                {
                    return wrong("wrong length");
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

/// Serialized size; a value that cannot serialize counts as too large.
fn serialized_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
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
                    min_items: None,
                    max_items: None,
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

    fn one_choice(options: Vec<QuestionOption>, multi: bool, allow_other: bool) -> QuestionForm {
        QuestionForm {
            source: None,
            message: None,
            fields: vec![QuestionField {
                id: "q".into(),
                label: "Pick".into(),
                header: None,
                description: None,
                required: true,
                kind: QuestionFieldKind::Choice {
                    options,
                    multi,
                    allow_other,
                    min_items: None,
                    max_items: None,
                },
            }],
        }
    }

    // Review finding 4: untrusted forms must be bounded in every string and
    // in total, and multi-choice answers in count.
    #[test]
    fn oversized_values_defaults_and_forms_are_refused() {
        let mut option = QuestionOption::new("A");
        option.value = Some("v".repeat(QUESTION_TEXT_MAX_CHARS + 1));
        assert!(
            one_choice(vec![option], false, false).validate().is_err(),
            "option value"
        );

        let text = QuestionForm {
            source: None,
            message: None,
            fields: vec![QuestionField {
                id: "t".into(),
                label: "Text".into(),
                header: None,
                description: None,
                required: false,
                kind: QuestionFieldKind::Text {
                    multiline: false,
                    default: Some("d".repeat(ANSWER_TEXT_MAX_CHARS + 1)),
                    min_length: None,
                    max_length: None,
                },
            }],
        };
        assert!(text.validate().is_err(), "text default");

        // Every string within its own bound, the whole form over the total.
        let field = |i: usize| QuestionField {
            id: format!("f{i}"),
            label: "l".repeat(QUESTION_TEXT_MAX_CHARS),
            header: None,
            description: Some("d".repeat(QUESTION_TEXT_MAX_CHARS)),
            required: false,
            kind: QuestionFieldKind::Choice {
                options: (0..QUESTION_MAX_OPTIONS)
                    .map(|o| QuestionOption {
                        label: format!("{o}{}", "o".repeat(QUESTION_TEXT_MAX_CHARS - 3)),
                        description: None,
                        value: None,
                    })
                    .collect(),
                multi: true,
                allow_other: false,
                min_items: None,
                max_items: None,
            },
        };
        let huge = QuestionForm {
            source: None,
            message: None,
            fields: (0..QUESTION_MAX_FIELDS).map(field).collect(),
        };
        assert_eq!(
            huge.validate(),
            Err(QuestionFormError::TooLarge),
            "aggregate size"
        );
    }

    #[test]
    fn multi_choice_answers_are_capped_and_unique() {
        let form = one_choice(
            vec![QuestionOption::new("A"), QuestionOption::new("B")],
            true,
            true,
        );
        let pick = |c: &[&str]| QuestionAnswer {
            values: [(
                "q".to_string(),
                QuestionValue::Choices(c.iter().map(|s| s.to_string()).collect()),
            )]
            .into(),
        };
        form.check_answer(&pick(&["A", "B", "mine"])).unwrap();
        assert!(
            form.check_answer(&pick(&["A", "A"])).is_err(),
            "duplicate pick"
        );
        assert!(
            form.check_answer(&pick(&["A", "B", "x", "y"])).is_err(),
            "more picks than options plus one Other"
        );
    }

    // Review finding 5: a label is what the person picks, so two options
    // with one label would return the wrong value.
    #[test]
    fn duplicate_option_labels_are_refused() {
        let mut a = QuestionOption::new("Same");
        a.value = Some("a".into());
        let mut b = QuestionOption::new("Same");
        b.value = Some("b".into());
        assert!(one_choice(vec![a, b], false, false).validate().is_err());
    }

    // Review finding 6: length and cardinality constraints are enforced.
    #[test]
    fn text_length_and_choice_cardinality_are_enforced() {
        let text = QuestionForm {
            source: None,
            message: None,
            fields: vec![QuestionField {
                id: "t".into(),
                label: "Code".into(),
                header: None,
                description: None,
                required: true,
                kind: QuestionFieldKind::Text {
                    multiline: false,
                    default: None,
                    min_length: Some(3),
                    max_length: Some(5),
                },
            }],
        };
        let say = |t: &str| QuestionAnswer {
            values: [("t".to_string(), QuestionValue::Text(t.into()))].into(),
        };
        assert!(text.check_answer(&say("x")).is_err(), "under min_length");
        assert!(
            text.check_answer(&say("abcdef")).is_err(),
            "over max_length"
        );
        text.check_answer(&say("abcd")).unwrap();

        let mut multi = one_choice(
            vec![
                QuestionOption::new("A"),
                QuestionOption::new("B"),
                QuestionOption::new("C"),
            ],
            true,
            false,
        );
        if let QuestionFieldKind::Choice {
            min_items,
            max_items,
            ..
        } = &mut multi.fields[0].kind
        {
            *min_items = Some(2);
            *max_items = Some(2);
        }
        multi.validate().unwrap();
        let pick = |c: &[&str]| QuestionAnswer {
            values: [(
                "q".to_string(),
                QuestionValue::Choices(c.iter().map(|s| s.to_string()).collect()),
            )]
            .into(),
        };
        assert!(
            multi.check_answer(&pick(&["A"])).is_err(),
            "under min_items"
        );
        assert!(
            multi.check_answer(&pick(&["A", "B", "C"])).is_err(),
            "over max_items"
        );
        multi.check_answer(&pick(&["A", "C"])).unwrap();
        let mut bad = multi.clone();
        if let QuestionFieldKind::Choice { min_items, .. } = &mut bad.fields[0].kind {
            *min_items = Some(9);
        }
        assert!(bad.validate().is_err(), "min_items no answer can meet");
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
