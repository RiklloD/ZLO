//! ZLO local memory for CLI runs (Claude Code, Codex, Antigravity) on the
//! loopback MCP route: `memory_tree`, `memory_read`, `memory_search` and
//! `memory_write`, so a CLI agent can keep the knowledge wiki (`wiki/`) the
//! same way the native agent does.
//!
//! Each call is answered by the Memory page surface itself, as the session
//! thread's owner (tenant, user, agent; no project, so every project shares
//! one memory, as the Memory page shows it). The page's input bounds, path
//! grammar, archive refusal, conflict-checked saves and prompt-safety scan
//! therefore apply unchanged. Saving over an existing document needs the
//! `content_hash` its read returned, so an agent never overwrites a page it
//! has not read.
//!
//! Mediation exception (`.claude/rules/tools.md`, "Direct access
//! exceptions"): like `history_search` on this route, the calls do not pass
//! through `CapabilityHost`. They act only on the caller's own memory
//! documents through the typed product facade the Memory page uses, with no
//! network, no secret and no other user's data, so no authorization,
//! approval, resource or runtime obligation is skipped.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::CliBridge;
use ironclaw_cli_providers::bridge_tools::BridgeToolProvider;
use ironclaw_host_api::ids::ActivityId;
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_product_contracts::memory_page::*;
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurface, ProductSurfaceCaller, ProductSurfaceError,
    ProductSurfaceErrorCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

const TREE: &str = "memory_tree";
const READ: &str = "memory_read";
const SEARCH: &str = "memory_search";
const WRITE: &str = "memory_write";
/// Most paths one `memory_tree` answer lists.
const MAX_TREE_PATHS: usize = 500;

const SCOPE_NOTE: &str = "ZLO local memory (shared with the user's other agents; the \
                          knowledge wiki lives under wiki/, see wiki/SCHEMA.md).";

pub(crate) fn register(bridge: &CliBridge, memory: Arc<dyn ProductSurface>) {
    bridge.add_tool_provider(Arc::new(MemoryBridgeTools { memory }));
}

struct MemoryBridgeTools {
    /// The Memory page surface.
    memory: Arc<dyn ProductSurface>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeArgs {
    #[serde(default)]
    prefix: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    content_hash: Option<String>,
}

fn result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn ok(value: impl serde::Serialize) -> Value {
    match serde_json::to_string(&value) {
        Ok(text) => result(text, false),
        Err(error) => {
            tracing::debug!(%error, "memory bridge output did not encode");
            result("memory tool failed".into(), true)
        }
    }
}

/// A model-correctable message for a surface error; never backend detail.
fn failure(tool: &str, error: &ProductSurfaceError) -> Value {
    let text = match (&error.code, error.field.as_deref()) {
        (ProductSurfaceErrorCode::NotFound, _) => format!("{tool}: no such document"),
        (_, Some("content")) => format!(
            "{tool}: content refused (empty, too large, or flagged by the memory safety scan)"
        ),
        (_, Some(field)) => format!("{tool}: invalid {field}"),
        _ if error.retryable => format!("{tool}: memory is busy; try again"),
        _ => format!("{tool}: memory is unavailable"),
    };
    result(text, true)
}

impl MemoryBridgeTools {
    fn bound(&self, thread: &ThreadRecord) -> BoundProductSurface {
        BoundProductSurface::new(
            Arc::clone(&self.memory),
            ProductSurfaceCaller::new(
                thread.scope.tenant_id.clone(),
                thread.scope.user_id.clone(),
                thread.scope.agent_id.clone(),
                None,
            ),
        )
    }

    async fn tree(&self, surface: &BoundProductSurface, args: TreeArgs) -> Value {
        match MEMORY_TREE_VIEW
            .query_on(surface, MemoryTreeRequest {}, None)
            .await
        {
            Ok(tree) if !tree.available => result(
                format!("{TREE}: this deployment's memory is not document-shaped"),
                true,
            ),
            Ok(tree) => {
                let prefix = args.prefix.unwrap_or_default();
                let paths: Vec<String> = tree
                    .documents
                    .into_iter()
                    .map(|entry| entry.path)
                    .filter(|path| path.starts_with(prefix.trim()) && !path.starts_with("archive/"))
                    .collect();
                let truncated = paths.len() > MAX_TREE_PATHS;
                ok(json!({
                    "paths": paths.into_iter().take(MAX_TREE_PATHS).collect::<Vec<_>>(),
                    "truncated": truncated,
                }))
            }
            Err(error) => failure(TREE, &error),
        }
    }

    async fn read(&self, surface: &BoundProductSurface, args: ReadArgs) -> Value {
        match MEMORY_DOCUMENT_VIEW
            .query_on(surface, MemoryDocumentRequest { path: args.path }, None)
            .await
        {
            Ok(document) => ok(json!({
                "path": document.path,
                "content": document.content,
                "content_hash": document.content_hash,
            })),
            Err(error) => failure(READ, &error),
        }
    }

    async fn search(&self, surface: &BoundProductSurface, args: SearchArgs) -> Value {
        match MEMORY_SEARCH_VIEW
            .query_on(
                surface,
                MemoryPageSearchRequest {
                    query: args.query,
                    limit: args.limit,
                },
                None,
            )
            .await
        {
            Ok(response) => ok(response.hits),
            Err(error) => failure(SEARCH, &error),
        }
    }

    async fn write(&self, surface: &BoundProductSurface, args: WriteArgs) -> Value {
        match MEMORY_WRITE_COMMAND
            .invoke_on(
                surface,
                MemoryWriteRequest {
                    path: args.path,
                    content: args.content,
                    expected_content_hash: args.content_hash,
                },
                ActivityId::new(),
            )
            .await
        {
            Ok(response) => match (response.outcome, response.document) {
                (MemoryPageWriteOutcome::Written, Some(document)) => ok(json!({
                    "saved": document.path,
                    "content_hash": document.content_hash,
                })),
                (MemoryPageWriteOutcome::Written, None) => result(
                    format!("{WRITE}: saved, but the read-back is missing; read it to check"),
                    true,
                ),
                (MemoryPageWriteOutcome::Conflict, _) => result(
                    format!(
                        "{WRITE}: not saved. The document exists or changed since you read it: \
                         call {READ} and pass its content_hash with the full new content."
                    ),
                    true,
                ),
            },
            Err(error) => failure(WRITE, &error),
        }
    }
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

#[async_trait]
impl BridgeToolProvider for MemoryBridgeTools {
    fn tools(&self) -> Vec<Value> {
        vec![
            json!({
                "name": TREE,
                "description": format!("List document paths in {SCOPE_NOTE}"),
                "inputSchema": schema(json!({
                    "prefix": {"type": "string", "description": "Only paths starting with this, e.g. wiki/."}
                }), &[]),
            }),
            json!({
                "name": READ,
                "description": format!("Read one document from {SCOPE_NOTE} Returns content and content_hash."),
                "inputSchema": schema(json!({
                    "path": {"type": "string", "description": "e.g. wiki/index.md"}
                }), &["path"]),
            }),
            json!({
                "name": SEARCH,
                "description": format!("Search {SCOPE_NOTE}"),
                "inputSchema": schema(json!({
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MEMORY_SEARCH_LIMIT}
                }), &["query"]),
            }),
            json!({
                "name": WRITE,
                "description": format!(
                    "Create or replace one document in {SCOPE_NOTE} To replace an existing \
                     document pass the content_hash from {READ}; without it the document must \
                     not exist yet. Content is the whole new document."
                ),
                "inputSchema": schema(json!({
                    "path": {"type": "string", "description": "e.g. wiki/concepts/vector-databases.md"},
                    "content": {"type": "string"},
                    "content_hash": {"type": "string"}
                }), &["path", "content"]),
            }),
        ]
    }

    fn handles(&self, name: &str) -> bool {
        matches!(name, TREE | READ | SEARCH | WRITE)
    }

    async fn call(&self, thread: &ThreadRecord, name: &str, arguments: &Value) -> Value {
        let surface = self.bound(thread);
        match name {
            TREE => match parse(arguments) {
                Ok(args) => self.tree(&surface, args).await,
                Err(error) => error,
            },
            READ => match parse(arguments) {
                Ok(args) => self.read(&surface, args).await,
                Err(error) => error,
            },
            SEARCH => match parse(arguments) {
                Ok(args) => self.search(&surface, args).await,
                Err(error) => error,
            },
            WRITE => match parse(arguments) {
                Ok(args) => self.write(&surface, args).await,
                Err(error) => error,
            },
            _ => result(format!("{name}: unknown tool"), true),
        }
    }
}

/// The tool's arguments, or the model-visible error for malformed ones.
fn parse<T: serde::de::DeserializeOwned>(arguments: &Value) -> Result<T, Value> {
    let arguments = match arguments {
        Value::Null => json!({}),
        other => other.clone(),
    };
    serde_json::from_value(arguments).map_err(|error| {
        // The message can quote document content; keep only its class.
        tracing::debug!(category = ?error.classify(), "memory bridge arguments rejected");
        result(
            format!("arguments are invalid ({:?})", error.classify()).to_lowercase(),
            true,
        )
    })
}

#[cfg(test)]
#[path = "memory_bridge_tests.rs"]
mod tests;
