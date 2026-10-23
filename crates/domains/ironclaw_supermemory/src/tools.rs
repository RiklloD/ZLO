//! The `supermemory.search` and `supermemory.write` model tools as plain
//! data: ids, schemas, input parsing and the JSON document each call returns.
//! The host adapter (`ironclaw_assistant::supermemory`) registers them and
//! adds the approval gate on write. Failures come back as `{ok: false, error,
//! message}` so the model can adapt instead of the run ending.

use serde_json::{Value, json};

use crate::binding::SupermemoryCaller;
use crate::error::SupermemoryError;
use crate::service::Supermemory;

pub const SUPERMEMORY_SEARCH_CAPABILITY_ID: &str = "supermemory.search";
pub const SUPERMEMORY_WRITE_CAPABILITY_ID: &str = "supermemory.write";
pub const SUPERMEMORY_SEARCH_TOOL_NAME: &str = "supermemory_search";
pub const SUPERMEMORY_WRITE_TOOL_NAME: &str = "supermemory_write";

pub const SUPERMEMORY_SEARCH_DESCRIPTION: &str = "Search long-term memory (Supermemory) by semantic similarity. Searches the primary container unless container names one of the readable containers.";
pub const SUPERMEMORY_WRITE_DESCRIPTION: &str = "Store a durable fact in long-term memory (Supermemory). Always writes to the primary container. Save lasting facts, preferences and decisions, not one-off task status.";

/// Local memory tools hidden while Supermemory is the caller's memory.
const LOCAL_MEMORY_CAPABILITY_PREFIX: &str = "ironclaw.memory.";

pub fn hides_local_memory_capability(capability_id: &str) -> bool {
    capability_id.starts_with(LOCAL_MEMORY_CAPABILITY_PREFIX)
}

pub fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to search for."},
            "limit": {"type": "integer", "description": "Maximum results to return, 1 to 20 (default 5)."},
            "container": {"type": "string", "description": "Optional readable container to search instead of the primary one."}
        },
        "required": ["query"]
    })
}

pub fn write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "content": {"type": "string", "description": "The memory to store, as a self-contained statement."},
            "metadata": {"type": "object", "description": "Optional flat metadata (string, number, boolean or string-list values)."}
        },
        "required": ["content"]
    })
}

/// Shape check before dispatch: arguments must be an object with the
/// required string field.
pub fn validate_arguments(capability_id: &str, arguments: &Value) -> Result<(), SupermemoryError> {
    let required = match capability_id {
        SUPERMEMORY_SEARCH_CAPABILITY_ID => "query",
        SUPERMEMORY_WRITE_CAPABILITY_ID => "content",
        other => {
            return Err(SupermemoryError::InvalidInput {
                reason: format!("unknown supermemory tool {other}"),
            });
        }
    };
    let present = arguments
        .get(required)
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    if arguments.is_object() && present {
        Ok(())
    } else {
        Err(SupermemoryError::InvalidInput {
            reason: format!("{required} is required"),
        })
    }
}

fn failure(error: &SupermemoryError) -> Value {
    json!({"ok": false, "error": error.kind(), "message": error.to_string()})
}

/// Run `supermemory.search`.
pub async fn run_search(
    supermemory: &Supermemory,
    caller: &SupermemoryCaller,
    input: &Value,
) -> Value {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let container = input.get("container").and_then(Value::as_str);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|limit| usize::try_from(limit).ok());
    match supermemory.search(caller, query, container, limit).await {
        Ok(hits) => {
            let results: Vec<Value> = hits
                .iter()
                .map(|hit| {
                    let mut item = json!({
                        "id": hit.id,
                        "container": hit.container.as_str(),
                        "content": hit.content,
                    });
                    if let Some(similarity) = hit.similarity {
                        item["similarity"] = json!((similarity * 100.0).round() as i64);
                    }
                    if let Some(updated_at) = &hit.updated_at {
                        item["updated_at"] = json!(updated_at);
                    }
                    item
                })
                .collect();
            json!({"ok": true, "count": results.len(), "results": results})
        }
        Err(error) => failure(&error),
    }
}

/// Run `supermemory.write`. The caller has already passed the approval gate.
pub async fn run_write(
    supermemory: &Supermemory,
    caller: &SupermemoryCaller,
    input: &Value,
) -> Value {
    let content = input
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let metadata = input.get("metadata").and_then(Value::as_object);
    match supermemory.write(caller, content, metadata).await {
        Ok(evidence) => {
            let preview: String = content.trim().chars().take(80).collect();
            json!({
                "ok": true,
                "saved": true,
                "id": evidence.id,
                "container": evidence.container.as_str(),
                "status": evidence.status,
                "verified": evidence.verified,
                "preview": preview,
            })
        }
        Err(error) => failure(&error),
    }
}
