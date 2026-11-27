//! Host tools beside the orchestrator's on the loopback MCP route.
//!
//! The route lists and calls `ironclaw_orchestrator::tools::TOOLS`; a host
//! can add more (history search, computer use) through
//! [`BridgeToolProvider`]s. Calls run as the session's thread, exactly like
//! the orchestrator tools: the provider receives the thread record and
//! answers an MCP tool result. Additive: without a provider the route is
//! unchanged.
//!
//! A runtime may build its product surface more than once (the channel host
//! takes its own copy), and every build registers its providers again. A
//! provider that offers a name an earlier one offered replaces it, and
//! `tools/list` never repeats a name: Antigravity refuses to start a session
//! whose MCP servers list one tool name twice.

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
    let providers = bridge.tool_providers();
    if providers.is_empty() {
        return handle_mcp(engine, grant, message).await;
    }
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let name = message["params"]["name"].as_str().unwrap_or("");
    let handler = (method == "tools/call")
        .then(|| providers.iter().find(|provider| provider.handles(name)))
        .flatten();
    if let Some(provider) = handler {
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
        let mut seen: std::collections::HashSet<String> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect();
        for provider in &providers {
            for tool in provider.tools() {
                let fresh = tool["name"]
                    .as_str()
                    .is_some_and(|name| seen.insert(name.to_string()));
                if fresh {
                    tools.push(tool);
                }
            }
        }
    }
    Some(answer)
}

impl CliBridge {
    /// Add host tools to the MCP route. A call goes to the first provider
    /// that handles its name. A provider offering a name an earlier one
    /// offered replaces that one, so registering again (a second product
    /// surface over the same runtime) never lists a tool twice.
    pub fn add_tool_provider(&self, provider: Arc<dyn BridgeToolProvider>) {
        let names = tool_names(provider.as_ref());
        let mut providers = self
            .tool_providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        providers.retain(|existing| tool_names(existing.as_ref()).is_disjoint(&names));
        providers.push(provider);
    }

    pub(crate) fn tool_providers(&self) -> Vec<Arc<dyn BridgeToolProvider>> {
        self.tool_providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn tool_names(provider: &dyn BridgeToolProvider) -> std::collections::HashSet<String> {
    provider
        .tools()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

/// The server name ZLO's route has inside Codex.
pub const CODEX_ROUTE_NAME: &str = "zlo_bridge";

/// One turn's token for the loopback MCP route, revoked when the turn drops
/// it. For drivers that hand the route to a CLI per turn (Codex).
pub struct TurnMcpToken {
    bridge: Arc<CliBridge>,
    url: String,
    token: String,
}

impl TurnMcpToken {
    /// `None` without a bridge, or before the gateway reported its URL.
    pub fn mint(
        bridge: Option<&Arc<CliBridge>>,
        thread: &ironclaw_host_api::ids::ThreadId,
        run_id: &str,
    ) -> Option<Self> {
        let bridge = bridge?;
        let url = bridge.mcp_url()?.to_string();
        let token = bridge.mint_token(
            SessionGrant {
                thread_id: thread.clone(),
                run_id: run_id.to_string(),
            },
            crate::bridge::SESSION_TOKEN_TTL,
        );
        Some(Self {
            bridge: Arc::clone(bridge),
            url,
            token,
        })
    }

    /// Codex's `config` override for the route: one streamable-HTTP MCP
    /// server under `mcp_servers.<CODEX_ROUTE_NAME>`. Codex merges it into
    /// the person's own config field by field, so it takes a name nobody
    /// configures by hand (an `ironclaw` stdio server would keep its
    /// `command` beside this `url` and Codex would refuse the mix).
    pub fn codex_config(&self) -> Value {
        let mut config = serde_json::Map::new();
        config.insert(
            format!("mcp_servers.{CODEX_ROUTE_NAME}"),
            json!({
                "url": self.url,
                "http_headers": {"Authorization": format!("Bearer {}", self.token)},
            }),
        );
        Value::Object(config)
    }
}

impl Drop for TurnMcpToken {
    fn drop(&mut self) {
        self.bridge.revoke_token(&self.token);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
    use ironclaw_orchestrator::model::{RuntimeId, ThreadScope};

    use ironclaw_orchestrator::runtime::RuntimeError;

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

        // A second product surface over the same runtime registers its
        // providers again; the newest replaces the first and the name is
        // listed once (Antigravity refuses duplicate tool names).
        bridge.add_tool_provider(Arc::new(Recorder::default()));
        let recorder = Arc::new(Recorder::default());
        bridge.add_tool_provider(recorder.clone());
        let after = call(list).await;
        assert_eq!(
            names(&after)
                .iter()
                .filter(|name| *name == "history_search")
                .count(),
            1
        );
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

    /// A runtime whose threads never start.
    struct NoStart;

    #[async_trait]
    impl ironclaw_orchestrator::runtime::TurnRuntime for NoStart {
        fn id(&self) -> RuntimeId {
            RuntimeId::new("claude-cli").unwrap()
        }
        fn capabilities(&self) -> ironclaw_orchestrator::runtime::RuntimeCapabilities {
            Default::default()
        }
        async fn create_thread(
            &self,
            _request: ironclaw_orchestrator::runtime::CreateThread,
        ) -> Result<ironclaw_orchestrator::runtime::CreatedThread, RuntimeError> {
            Err(RuntimeError::Unavailable("down".into()))
        }
        async fn start_turn(
            &self,
            _thread: &ThreadRecord,
            _input: ironclaw_orchestrator::runtime::TurnInput,
        ) -> Result<ironclaw_orchestrator::runtime::TurnAdmission, RuntimeError> {
            Err(RuntimeError::Unavailable("down".into()))
        }
        async fn steer(
            &self,
            _thread: &ThreadRecord,
            _text: &str,
            _how: ironclaw_orchestrator::runtime::SteerHow,
        ) -> Result<ironclaw_orchestrator::runtime::SteerOutcome, RuntimeError> {
            Err(RuntimeError::Unavailable("down".into()))
        }
        async fn cancel(&self, _thread: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn history(
            &self,
            _thread: &ThreadRecord,
            _limit: usize,
        ) -> Result<Vec<ironclaw_orchestrator::runtime::HistoryMessage>, RuntimeError> {
            Ok(Vec::new())
        }
    }

    /// A CLI's `t3_delegate` over the loopback route records the CLI turn's
    /// run on the task (the parent transcript's anchor), and a child that
    /// could not start still names its task in the tool body.
    #[tokio::test]
    async fn a_cli_delegate_call_anchors_its_task_to_the_cli_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        // The child is recorded, then its runtime cannot make its thread.
        engine.register_runtime(Arc::new(NoStart));
        let thread = ThreadId::new("cli-anchor").unwrap();
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
        let grant = SessionGrant {
            thread_id: thread.clone(),
            run_id: "run-cli-7".into(),
        };
        let answer = handle_mcp(
            &engine,
            &grant,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "t3_delegate", "arguments": {"task": "look around"}}}),
        )
        .await
        .unwrap();
        let body: Value =
            serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let tasks = engine
            .store()
            .tasks_for_parent(&thread, false)
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1, "{body}");
        assert_eq!(tasks[0].parent_run_id.as_deref(), Some("run-cli-7"));
        assert_eq!(body["taskId"], tasks[0].id.as_str(), "{body}");
    }
}
