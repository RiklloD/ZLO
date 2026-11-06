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

/// `/v3/documents` add response, `/v3/documents/{id}` read-back, or one row
/// of `/v3/documents/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WireDocument {
    pub(crate) id: String,
    pub(crate) status: Option<String>,
    pub(crate) custom_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) container_tags: Vec<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) updated_at: Option<String>,
}

/// `/v3/documents/list` response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WireDocumentPage {
    pub(crate) documents: Vec<WireDocument>,
    pub(crate) current_page: Option<u32>,
    pub(crate) total_pages: Option<u32>,
    pub(crate) total_items: Option<u64>,
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
    // A read-back may wrap the row as `{"document": {...}}`.
    let value = field(value, &["document"])
        .filter(|inner| inner.is_object())
        .unwrap_or(value);
    let id = string_field(value, &["id", "documentId", "document_id"]).ok_or_else(|| {
        SupermemoryError::Decode {
            reason: "document response carries no id".to_string(),
        }
    })?;
    let container_tags = match field(value, &["containerTags", "container_tags"]) {
        Some(Value::Array(tags)) => tags
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => string_field(value, &["containerTag", "container_tag"])
            .into_iter()
            .collect(),
    };
    Ok(WireDocument {
        id,
        status: string_field(value, &["status"]),
        custom_id: string_field(value, &["customId", "custom_id"]),
        title: string_field(value, &["title"]),
        summary: string_field(value, &["summary"]),
        content: string_field(value, &["content"]),
        container_tags,
        created_at: string_field(value, &["createdAt", "created_at"]),
        updated_at: string_field(value, &["updatedAt", "updated_at"]),
    })
}

fn u64_field(value: &Value, names: &[&str]) -> Option<u64> {
    number_field(value, names)
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| number as u64)
}

/// Rows under `memories` (the documented key), `documents`, or a bare list;
/// rows without an id are skipped.
pub(crate) fn document_page(value: &Value) -> WireDocumentPage {
    let rows = match field(value, &["memories", "documents"]) {
        Some(rows) => list(Some(rows)),
        None => list(Some(value)),
    };
    let pagination = field(value, &["pagination"]).unwrap_or(&Value::Null);
    let as_u32 = |number: Option<u64>| number.and_then(|number| u32::try_from(number).ok());
    WireDocumentPage {
        documents: rows.iter().filter_map(|row| document(row).ok()).collect(),
        current_page: as_u32(u64_field(
            pagination,
            &["currentPage", "current_page", "page"],
        )),
        total_pages: as_u32(u64_field(pagination, &["totalPages", "total_pages"])),
        total_items: u64_field(pagination, &["totalItems", "total_items"]),
    }
}
