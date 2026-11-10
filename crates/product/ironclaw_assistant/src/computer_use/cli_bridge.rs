//! Computer use for CLI runs (Claude, Codex) on the bridge's loopback MCP
//! route, beside the orchestrator tools and `history_search`.
//!
//! A CLI agent runs `claude -p` with `--strict-mcp-config`, so it never gets
//! a computer-use server of its own; without these tools a CLI thread has no
//! computer use even when this Mac's engine is ready. Each tool reaches the
//! same per-thread session the API-model agent uses, so the live view, Stop
//! and take over work the same way.
//!
//! Approval is the bridge's: every call already waits on the CLI's
//! PreToolUse gate for the thread's owner (or its "Full access" profile)
//! before it reaches this provider, exactly like the CLI's own tools.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::CliBridge;
use ironclaw_cli_providers::bridge_tools::BridgeToolProvider;
use ironclaw_host_api::ids::InvocationId;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_RESULT_FRAME, COMPUTER_USE_TOOL_NAME_PREFIX, ComputerUseSession,
    ComputerUseSessions,
};
use serde_json::{Value, json};

/// Offer this host's computer-use tools to CLI runs on `bridge`'s MCP route.
pub fn register_computer_use_bridge_tools<S: ComputerUseSessions + 'static>(
    bridge: &CliBridge,
    sessions: Arc<S>,
) {
    bridge.add_tool_provider(Arc::new(ComputerUseBridgeTools { sessions }));
}

struct ComputerUseBridgeTools<S> {
    sessions: Arc<S>,
}

impl<S: ComputerUseSessions> ComputerUseBridgeTools<S> {
    /// The engine's tool name behind an MCP name, when it is one of ours.
    fn engine_tool<'a>(&self, name: &'a str) -> Option<&'a str> {
        let tool = name.strip_prefix(COMPUTER_USE_TOOL_NAME_PREFIX)?;
        self.sessions
            .tools()
            .iter()
            .any(|spec| spec.name == tool)
            .then_some(tool)
    }
}

fn thread_scope(thread: &ThreadRecord) -> ResourceScope {
    ResourceScope {
        tenant_id: thread.scope.tenant_id.clone(),
        user_id: thread.scope.user_id.clone(),
        agent_id: thread.scope.agent_id.clone(),
        project_id: thread.scope.project_id.clone(),
        mission_id: None,
        thread_id: Some(thread.thread_id.clone()),
        invocation_id: InvocationId::new(),
    }
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[async_trait]
impl<S: ComputerUseSessions + 'static> BridgeToolProvider for ComputerUseBridgeTools<S> {
    fn tools(&self) -> Vec<Value> {
        self.sessions
            .tools()
            .iter()
            .map(|spec| {
                json!({
                    "name": format!("{COMPUTER_USE_TOOL_NAME_PREFIX}{}", spec.name),
                    "description": spec.description,
                    "inputSchema": spec.input_schema,
                })
            })
            .collect()
    }

    fn handles(&self, name: &str) -> bool {
        self.engine_tool(name).is_some()
    }

    async fn call(&self, thread: &ThreadRecord, name: &str, arguments: &Value) -> Value {
        let Some(tool) = self.engine_tool(name) else {
            return text_result(format!("{name} is not a computer-use tool"), true);
        };
        if !arguments.is_object() {
            return text_result("computer-use arguments must be an object".into(), true);
        }
        let scope = thread_scope(thread);
        let session = ComputerUseSession::from_scope(&scope);
        let mut output = self
            .sessions
            .call_tool(scope, tool, arguments.clone())
            .await;
        // The screenshot number is host bookkeeping; the picture itself goes
        // to the agent as an image block.
        let frame = output
            .as_object_mut()
            .and_then(|fields| fields.remove(COMPUTER_USE_RESULT_FRAME))
            .and_then(|seq| seq.as_u64());
        let ok = output.get("ok").and_then(Value::as_bool) == Some(true);
        let text = match output.get("result").and_then(Value::as_str) {
            Some(result) if ok => result.to_string(),
            _ => output.to_string(),
        };
        let mut content = vec![json!({"type": "text", "text": text})];
        if let (Some(seq), Some(session)) = (frame, &session) {
            let result_ref = format!("cli-bridge:{}", uuid::Uuid::new_v4());
            self.sessions.bind_result_frame(session, seq, &result_ref);
            if let Some(image) = self.sessions.result_image(session, &result_ref) {
                content.push(json!({
                    "type": "image",
                    "data": image.data,
                    "mimeType": image.mime_type,
                }));
            }
        }
        json!({"content": content, "isError": !ok})
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use ironclaw_cli_providers::bridge::{SessionGrant, serve_mcp};
    use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
    use ironclaw_orchestrator::engine::Engine;
    use ironclaw_orchestrator::model::{RuntimeId, ThreadScope};
    use ironclaw_product_contracts::computer_use::{
        ComputerUseControlAction, ComputerUseFrameRequest, ComputerUseImage, ComputerUseToolSpec,
    };

    use super::*;

    /// Records each call's scope and input; answers like the engine, with a
    /// screenshot number, and keeps bound frames like the session does.
    #[derive(Default)]
    struct RecordingSessions {
        tools: Vec<ComputerUseToolSpec>,
        calls: Mutex<Vec<(ResourceScope, String, Value)>>,
        bound: Mutex<Vec<(ComputerUseSession, u64, String)>>,
    }

    #[async_trait]
    impl ComputerUseSessions for RecordingSessions {
        fn tools(&self) -> &[ComputerUseToolSpec] {
            &self.tools
        }
        async fn call_tool(&self, scope: ResourceScope, tool: &str, input: Value) -> Value {
            self.calls
                .lock()
                .unwrap()
                .push((scope, tool.to_string(), input));
            if tool == "click" {
                json!({"ok": false, "code": "human_has_control", "error": "You have control."})
            } else {
                json!({"ok": true, "result": "Finder window tree", COMPUTER_USE_RESULT_FRAME: 3})
            }
        }
        fn bind_result_frame(&self, session: &ComputerUseSession, seq: u64, result_ref: &str) {
            self.bound
                .lock()
                .unwrap()
                .push((session.clone(), seq, result_ref.to_string()));
        }
        fn result_image(
            &self,
            session: &ComputerUseSession,
            result_ref: &str,
        ) -> Option<ComputerUseImage> {
            let bound = self.bound.lock().unwrap();
            bound
                .iter()
                .any(|(s, seq, r)| s == session && *seq == 3 && r == result_ref)
                .then(|| ComputerUseImage {
                    mime_type: "image/jpeg".into(),
                    data: "/9j/4A==".into(),
                })
        }
        fn session_view(&self, _: &ComputerUseSession, _: Option<u64>) -> Value {
            Value::Null
        }
        fn viewer_state(&self, _: &TenantId, _: &UserId, _: Option<&str>) -> Value {
            Value::Null
        }
        async fn window_frame(&self, _: &ComputerUseSession, _: &ComputerUseFrameRequest) -> Value {
            Value::Null
        }
        async fn control(
            &self,
            _: &ComputerUseSession,
            _: ComputerUseControlAction,
            _: Option<&str>,
        ) -> Value {
            Value::Null
        }
        async fn thread_stopped(&self, _: &TenantId, _: &ThreadId) {}
        async fn turn_ended(&self, _: &TenantId, _: &ThreadId) {}
    }

    fn spec(name: &str) -> ComputerUseToolSpec {
        ComputerUseToolSpec {
            name: name.into(),
            description: format!("{name} desc"),
            input_schema: json!({"type": "object", "properties": {"app": {"type": "string"}}}),
        }
    }

    /// A CLI thread (Claude `-p`, `--strict-mcp-config`) gets this host's
    /// computer use on its MCP route: listed beside the orchestrator tools,
    /// run in the session of its own thread, the screenshot handed back as
    /// an image block, a refusal as a tool error.
    #[tokio::test]
    async fn cli_runs_reach_the_thread_computer_use_session_over_the_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let thread = ThreadId::new("cli-cu").unwrap();
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
                RuntimeId::new("claude-cli-2").unwrap(),
            ))
            .await
            .unwrap();
        let sessions = Arc::new(RecordingSessions {
            tools: vec![spec("get_app_state"), spec("click")],
            ..RecordingSessions::default()
        });
        let bridge = CliBridge::new(Duration::from_secs(10));
        register_computer_use_bridge_tools(&bridge, Arc::clone(&sessions));
        let token = bridge.mint_token(
            SessionGrant {
                thread_id: thread.clone(),
                run_id: "r".into(),
            },
            Duration::from_secs(60),
        );
        let auth = format!("Bearer {token}");
        let send = |body: Value| {
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

        let list = send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"computer_use_get_app_state"), "{names:?}");
        assert!(names.contains(&"computer_use_click"), "{names:?}");
        assert!(
            names.iter().any(|n| n.starts_with("t3_")),
            "orchestrator tools stay listed: {names:?}"
        );

        let state = send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "computer_use_get_app_state", "arguments": {"app": "Finder"}}}))
        .await;
        assert_eq!(state["result"]["isError"], false, "{state}");
        assert_eq!(
            state["result"]["content"],
            json!([
                {"type": "text", "text": "Finder window tree"},
                {"type": "image", "data": "/9j/4A==", "mimeType": "image/jpeg"}
            ])
        );
        {
            let calls = sessions.calls.lock().unwrap();
            let (scope, tool, input) = &calls[0];
            assert_eq!(tool, "get_app_state");
            assert_eq!(input, &json!({"app": "Finder"}));
            assert_eq!(scope.thread_id.as_ref(), Some(&thread));
            assert_eq!(scope.user_id.as_str(), "u");
            assert_eq!(scope.tenant_id.as_str(), "t");
        }

        let click = send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "computer_use_click", "arguments": {"app": "Finder"}}}))
        .await;
        assert_eq!(click["result"]["isError"], true, "{click}");
        let text = click["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("human_has_control"), "{text}");
        assert_eq!(click["result"]["content"].as_array().unwrap().len(), 1);

        // A name the engine does not serve is not ours.
        let unknown = send(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "computer_use_format_disk", "arguments": {}}}))
        .await;
        assert_ne!(unknown["result"]["isError"], false);
        assert_eq!(sessions.calls.lock().unwrap().len(), 2);
    }
}
