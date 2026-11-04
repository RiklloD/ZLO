//! A CLI thread through the orchestrating surface: its stream shows the
//! pending tool call, `gate.resolve` from the owner answers it, and nobody
//! else reaches either.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, ToolDecision};
use ironclaw_host_api::ids::{ActivityId, AgentId, CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord};
use ironclaw_orchestrator::runtime::{
    CreateThread, CreatedThread, HistoryMessage, HistoryRole, RuntimeCapabilities, RuntimeError,
    SteerHow, SteerOutcome, TurnAdmission, TurnInput, TurnRuntime,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::json;

use super::surface::{OrchestratedProductSurface, scope_of};
use crate::reborn_services::RESOLVE_GATE_COMMAND_ID;

/// Stands for IronClaw's own surface: anything reaching it was not a CLI thread.
struct Inner;

#[async_trait]
impl ProductSurface for Inner {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn query(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Ok(ProductSurfaceStreamResponse {
            events: Vec::new(),
            next_cursor: None,
            subscription: None,
        })
    }
}

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

#[tokio::test]
async fn cli_thread_gate_streams_and_resolves_for_its_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-thread").unwrap();
    let owner = caller("owner");
    engine
        .store()
        .ensure_thread(&ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("claude-cli").unwrap(),
        ))
        .await
        .unwrap();
    let bridge = CliBridge::new(Duration::from_secs(10));
    let surface =
        OrchestratedProductSurface::new(Arc::new(Inner), engine, Some(Arc::clone(&bridge)));

    let run = uuid::Uuid::new_v4().to_string();
    bridge.turn_started(&thread, &run);
    let waiter = {
        let (bridge, thread, run) = (Arc::clone(&bridge), thread.clone(), run.clone());
        tokio::spawn(async move {
            bridge
                .request_approval(&thread, &run, "Bash", &json!({"command": "ls"}))
                .await
        })
    };
    while bridge.snapshot(&thread).is_some_and(|s| s.gates.is_empty()) {
        tokio::task::yield_now().await;
    }

    let stream = |who: ProductSurfaceCaller| {
        surface.stream_events(
            who,
            ProductSurfaceStreamRequest {
                stream_id: Some(thread.to_string()),
                after_cursor: None,
            },
        )
    };
    // Another user's stream never reaches the bridge.
    let foreign = stream(caller("intruder")).await.unwrap();
    assert!(foreign.events.is_empty() && foreign.subscription.is_none());
    let mine = stream(owner.clone()).await.unwrap();
    assert!(mine.subscription.is_some());
    let items = &mine.events[0]["payload"]["projection_update"]["state"]["items"];
    let gate = items
        .as_array()
        .unwrap()
        .iter()
        .find_map(|i| i.get("gate"))
        .unwrap_or_else(|| panic!("no gate in {items}"));
    assert_eq!(gate["headline"], "Allow Bash?");
    assert_eq!(gate["allow_always"], false);
    let gate_ref = gate["gate_ref"].as_str().unwrap().to_string();

    let resolve = |who: ProductSurfaceCaller| {
        surface.invoke(
            who,
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(RESOLVE_GATE_COMMAND_ID).unwrap(),
                input: json!({"thread_id": thread.as_str(), "run_id": run,
                              "gate_ref": gate_ref, "resolution": "approved"}),
                activity_id: ActivityId::new(),
            },
        )
    };
    // The intruder falls through to the inner surface, which knows no such gate.
    assert!(resolve(caller("intruder")).await.is_err());
    let answered = resolve(owner).await.unwrap();
    assert_eq!(answered.output["outcome"], "resumed");
    assert_eq!(waiter.await.unwrap(), ToolDecision::Allow);
}

/// A CLI runtime whose session holds one reply.
struct OneReply;

#[async_trait]
impl TurnRuntime for OneReply {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("claude-cli").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    async fn create_thread(&self, _: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Err(RuntimeError::Unsupported("turns"))
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        _: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Err(RuntimeError::Unsupported("steer"))
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _: &ThreadRecord,
        _: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        Ok(vec![HistoryMessage {
            role: HistoryRole::Assistant,
            text: "Forty-two.".into(),
            tool_calls: Vec::new(),
            run_id: None,
            hidden: false,
            at: None,
        }])
    }
}

/// A fork or delegated child the orchestrator created never reached the
/// session thread store, so the inner timeline answers 404. The surface
/// still serves its timeline, in the gateway's shape, to its owner only.
#[tokio::test]
async fn orchestrator_made_cli_thread_serves_a_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-fork").unwrap();
    let owner = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("agent").unwrap()),
        None,
    );
    let mut record = ThreadRecord::new(
        thread.clone(),
        scope_of(&owner),
        RuntimeId::new("claude-cli").unwrap(),
    );
    record.title = Some("Side chat".into());
    engine.store().ensure_thread(&record).await.unwrap();
    engine.register_runtime(Arc::new(OneReply));
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine, None);

    let timeline = |who: ProductSurfaceCaller| {
        surface.query(
            who,
            ProductSurfaceQueryRequest {
                view_id: crate::reborn_services::TIMELINE_VIEW.id.to_string(),
                input: json!({"thread_id": thread.as_str(), "limit": 50}),
                cursor: None,
                limit: None,
            },
        )
    };
    let page = timeline(owner).await.unwrap();
    let response: crate::reborn_services::RebornTimelineResponse =
        serde_json::from_value(page.items[0].clone()).unwrap();
    assert_eq!(response.thread.thread_id.as_str(), "cli-fork");
    assert_eq!(response.thread.title.as_deref(), Some("Side chat"));
    assert_eq!(response.messages.len(), 1);
    assert_eq!(response.messages[0].content.as_deref(), Some("Forty-two."));
    assert!(response.next_cursor.is_none());
    // Anyone else still falls through to the inner 404.
    assert!(timeline(caller("intruder")).await.is_err());
}

/// Live command output, the turn diff and a review reach a CLI thread's
/// stream as typed `cli_detail` items: the snapshot at connect carries
/// them, and later changes arrive on the subscription.
#[tokio::test]
async fn cli_thread_stream_carries_command_output_diff_and_review() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("codex-thread").unwrap();
    let owner = caller("owner");
    engine
        .store()
        .ensure_thread(&ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("codex").unwrap(),
        ))
        .await
        .unwrap();
    let bridge = CliBridge::new(Duration::from_secs(10));
    let surface =
        OrchestratedProductSurface::new(Arc::new(Inner), engine, Some(Arc::clone(&bridge)));

    let run = uuid::Uuid::new_v4().to_string();
    bridge.turn_started(&thread, &run);
    bridge.tool_observed(&thread, &run, "shell", Some("cargo test"), "c1");
    bridge.tool_output(&thread, &run, "c1", "running 3 tests\n");
    bridge.turn_diff(&thread, &run, "diff --git a/x b/x\n");

    let mut stream = surface
        .stream_events(
            owner,
            ProductSurfaceStreamRequest {
                stream_id: Some(thread.to_string()),
                after_cursor: None,
            },
        )
        .await
        .unwrap();
    let items = stream.events[0]["payload"]["projection_update"]["state"]["items"]
        .as_array()
        .unwrap()
        .clone();
    let details: Vec<&serde_json::Value> =
        items.iter().filter_map(|i| i.get("cli_detail")).collect();
    let output = details
        .iter()
        .find(|d| d["kind"] == "tool_output")
        .unwrap_or_else(|| panic!("no output in {items:?}"));
    assert_eq!(output["body"], "running 3 tests\n");
    assert!(output["invocation_id"].is_string());
    assert!(
        details
            .iter()
            .any(|d| d["kind"] == "turn_diff" && d["body"] == "diff --git a/x b/x\n")
    );

    bridge.review(&thread, &run, "- [P1] Bug — /w/a.rs:1-2\n  body");
    let subscription = stream.subscription.take().unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = &next.events[0]["payload"]["projection_update"]["state"]["items"][0];
    assert_eq!(frame["cli_detail"]["kind"], "review");
    assert!(
        frame["cli_detail"]["body"]
            .as_str()
            .unwrap()
            .contains("/w/a.rs:1-2")
    );
}

/// `orchestrator.thread_action` reaches the runtime of the caller's own
/// thread only; a runtime without the action answers a correctable error.
#[tokio::test]
async fn thread_action_reaches_the_owners_runtime_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-thread").unwrap();
    let owner = caller("owner");
    engine
        .store()
        .ensure_thread(&ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("claude-cli").unwrap(),
        ))
        .await
        .unwrap();
    engine.register_runtime(Arc::new(OneReply));
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine, None);
    let act = |who: ProductSurfaceCaller| {
        surface.invoke(
            who,
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(
                    ironclaw_product_contracts::orchestration::ORCHESTRATOR_THREAD_ACTION_COMMAND_ID,
                )
                .unwrap(),
                input: json!({"thread_id": thread.as_str(), "action": {"action": "compact"}}),
                activity_id: ActivityId::new(),
            },
        )
    };
    assert!(act(caller("intruder")).await.is_err());
    let answer = act(owner).await.unwrap();
    assert_eq!(answer.output["ok"], false, "{}", answer.output);
    assert_eq!(answer.output["error"], "unsupported");
}
