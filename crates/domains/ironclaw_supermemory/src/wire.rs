//! Drift-tolerant decoding of Supermemory responses.
//!
//! Responses are read as `serde_json::Value` and picked apart field by field:
//! unknown fields are ignored, a missing or mistyped optional field becomes
//! `None`, camelCase and snake_case spellings are both accepted, and a list
//! may arrive bare or wrapped in `{"results": [...]}`. Only a response that is
//! not JSON at all is an error.

use serde_json::Value;

use crate::error::SupermemoryError;

/// One memory hit from `/v4/search` or the profile's `searchResults`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WireMemory {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) similarity: Option<f64>,
    pub(crate) updated_at: Option<String>,
}

/// `/v4/profile` response.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WireProfile {
    pub(crate) static_facts: Vec<String>,
    pub(crate) dynamic_facts: Vec<String>,
    pub(crate) search: Vec<WireMemory>,
}

/// `/v3/documents` add response, or `/v3/documents/{id}` read-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireDocument {
    pub(crate) id: String,
    pub(crate) status: Option<String>,
}

pub(crate) fn parse_body(body: &[u8]) -> Result<Value, SupermemoryError> {
    serde_json::from_slice(body).map_err(|error| SupermemoryError::Decode {
        reason: error.to_string(),
    })
}

fn field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| value.get(*name).filter(|found| !found.is_null()))
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    field(value, names)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn number_field(value: &Value, names: &[&str]) -> Option<f64> {
    match field(value, names)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn list(value: Option<&Value>) -> &[Value] {
    match value {
        Some(Value::Array(items)) => items,
        Some(object @ Value::Object(_)) => match object.get("results") {
            Some(Value::Array(items)) => items,
            _ => &[],
        },
        _ => &[],
    }
}

fn memory(item: &Value) -> Option<WireMemory> {
    let text = string_field(item, &["memory", "chunk", "content"])?;
    Some(WireMemory {
        id: string_field(item, &["id"]).unwrap_or_default(),
        text,
        similarity: number_field(item, &["similarity", "score"]),
        updated_at: string_field(item, &["updatedAt", "updated_at"]),
    })
}

pub(crate) fn search_results(value: &Value) -> Vec<WireMemory> {
    list(Some(value)).iter().filter_map(memory).collect()
}

fn facts(value: Option<&Value>) -> Vec<String> {
    list(value)
        .iter()
        .filter_map(|fact| match fact {
            Value::String(text) => Some(text.trim().to_string()),
            other => string_field(other, &["memory", "content", "text"]),
        })
        .filter(|text| !text.is_empty())
        .collect()
}

pub(crate) fn profile(value: &Value) -> WireProfile {
    let profile = field(value, &["profile"]);
    WireProfile {
        static_facts: facts(profile.and_then(|profile| field(profile, &["static"]))),
        dynamic_facts: facts(profile.and_then(|profile| field(profile, &["dynamic"]))),
        search: list(field(value, &["searchResults", "search_results"]))
            .iter()
            .filter_map(memory)
            .collect(),
    }
}

pub(crate) fn document(value: &Value) -> Result<WireDocument, SupermemoryError> {
    let id = string_field(value, &["id", "documentId", "document_id"]).ok_or_else(|| {
        SupermemoryError::Decode {
            reason: "document response carries no id".to_string(),
        }
    })?;
    Ok(WireDocument {
        id,
        status: string_field(value, &["status"]),
    })
}
