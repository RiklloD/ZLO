//! Host tools beside the orchestrator's on the loopback MCP route.
//!
//! The route lists and calls `ironclaw_orchestrator::tools::TOOLS`; a host
//! can add more (history search) through one [`BridgeToolProvider`], set
//! once. Calls run as the session's thread, exactly like the orchestrator
//! tools: the provider receives the thread record and answers an MCP tool
//! result. Additive: without a provider the route is unchanged.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::ThreadRecord;
use serde_json::{Value, json};

use crate::bridge::{CliBridge, SessionGrant, handle_mcp};

#[async_trait]
pub trait BridgeToolProvider: Send + Sync {
    /// `{name, description, inputSchema}` entries for `tools/list`.
    fn tools(&self) -> Vec<Value>;
    fn handles(&self, name: &str) -> bool;
    /// The tool result `{content, isError}` for a call as `thread`.
    async fn call(&self, thread: &ThreadRecord, name: &str, arguments: &Value) -> Value;
}

/// [`handle_mcp`] plus the bridge's extra tools, when it has any.
pub(crate) async fn handle_with_tools(
    bridge: &CliBridge,
    engine: &Engine,
    grant: &SessionGrant,
    message: &Value,
) -> Option<Value> {
    let Some(provider) = bridge.tool_provider() else {
        return handle_mcp(engine, grant, message).await;
    };
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let name = message["params"]["name"].as_str().unwrap_or("");
    if method == "tools/call" && provider.handles(name) {
        let id = message.get("id").cloned()?;
        let result = match engine.store().thread(&grant.thread_id).await {
            Ok(Some(thread)) => {
                let arguments = match message["params"].get("arguments") {
                    Some(Value::Null) | None => json!({}),
                    Some(args) => args.clone(),
                };
                json!({"jsonrpc": "2.0", "id": id, "result": provider.call(&thread, name, &arguments).await})
            }
            Ok(None) => json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "thread not found"}}),
            Err(error) => {
                tracing::debug!(%error, "cli bridge: thread lookup failed");
                json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32603, "message": "thread unavailable"}})
            }
        };
        return Some(result);
    }
    let mut answer = handle_mcp(engine, grant, message).await?;
    if method == "tools/list"
        && let Some(tools) = answer["result"]["tools"].as_array_mut()
    {
        tools.extend(provider.tools());
    }
    Some(answer)
}

impl CliBridge {
    /// Add host tools to the MCP route (first call wins).
    pub fn set_tool_provider(&self, provider: Arc<dyn BridgeToolProvider>) {
        let _ = self.tool_provider.set(provider); // silent-ok: wired once per process
    }

    pub(crate) fn tool_provider(&self) -> Option<&Arc<dyn BridgeToolProvider>> {
        self.tool_provider.get()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
    use ironclaw_orchestrator::model::{RuntimeId, ThreadScope};

    use super::*;
    use crate::bridge::{DEFAULT_APPROVAL_TIMEOUT, SESSION_TOKEN_TTL, serve_mcp};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, Value)>>);

    #[async_trait]
    impl BridgeToolProvider for Recorder {
        fn tools(&self) -> Vec<Value> {
            vec![json!({"name": "history_search", "description": "d", "inputSchema": {}})]
        }
        fn handles(&self, name: &str) -> bool {
            name == "history_search"
        }
        async fn call(&self, thread: &ThreadRecord, _name: &str, arguments: &Value) -> Value {
            self.0
                .lock()
                .unwrap()
                .push((thread.thread_id.as_str().to_string(), arguments.clone()));
            json!({"content": [{"type": "text", "text": "ok"}], "isError": false})
        }
    }

    #[tokio::test]
    async fn host_tools_are_listed_and_called_as_the_session_thread() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let thread = ThreadId::new("cli-1").unwrap();
        engine
            .store()
            .ensure_thread(&ThreadRecord::new(
                thread.clone(),
                ThreadScope {
                    tenant_id: TenantId::new("t").unwrap(),
                    user_id: UserId::new("u").unwrap(),
                    agent_id: None,
                    project_id: None,
                },
                RuntimeId::new("claude-cli").unwrap(),
            ))
            .await
            .unwrap();
        let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
        let token = bridge.mint_token(
            SessionGrant {
                thread_id: thread.clone(),
                run_id: "r".into(),
            },
            SESSION_TOKEN_TTL,
        );
        let auth = format!("Bearer {token}");
        let call = |body: Value| {
            let (bridge, engine, auth) = (Arc::clone(&bridge), Arc::clone(&engine), auth.clone());
            async move {
                serve_mcp(
                    &bridge,
                    &engine,
                    true,
                    Some(&auth),
                    body.to_string().as_bytes(),
                )
                .await
                .unwrap()
                .unwrap()
            }
        };
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        // Without a provider the route lists only the orchestrator tools.
        let before = call(list.clone()).await;
        let names = |v: &Value| -> Vec<String> {
            v["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(!names(&before).contains(&"history_search".to_string()));

        let recorder = Arc::new(Recorder::default());
        bridge.set_tool_provider(recorder.clone());
        let after = call(list).await;
        assert!(names(&after).contains(&"history_search".to_string()));
        assert_eq!(names(&after).len(), names(&before).len() + 1);

        let result = call(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "history_search", "arguments": {"query": "deploy"}}}))
        .await;
        assert_eq!(result["result"]["isError"], false);
        let calls = recorder.0.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![("cli-1".to_string(), json!({"query": "deploy"}))]
        );
    }
}
