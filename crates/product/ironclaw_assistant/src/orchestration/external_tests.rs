//! A CLI thread through the orchestrating surface: its stream shows the
//! pending tool call, `gate.resolve` from the owner answers it, and nobody
//! else reaches either.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, ToolDecision};
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord};
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
