//! The loopback MCP route over its real router.

use super::*;
use axum::body::Body;
use ironclaw_cli_providers::bridge::{SESSION_TOKEN_TTL, SessionGrant};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::ThreadScope;
use tower::ServiceExt;

async fn fixture(dir: &std::path::Path) -> (Arc<CliBridge>, Router, ThreadId) {
    let db = Arc::new(
        libsql::Builder::new_local(dir.join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-thread").unwrap();
    let scope = ThreadScope {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
        agent_id: None,
        project_id: None,
    };
    engine.ensure_thread(&thread, &scope, None).await.unwrap();
    let bridge = CliBridge::new(std::time::Duration::from_secs(5));
    let mount = cli_bridge_route_mount(
        CliBridgeRouteState::new(Arc::clone(&bridge), engine),
        SocketAddr::from(([127, 0, 0, 1], 4100)),
    )
    .unwrap();
    (bridge, mount.router, thread)
}

async fn post(
    router: &Router,
    peer: [u8; 4],
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::post(CLI_BRIDGE_MCP_PATH)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((peer, 50000))));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn mcp_route_serves_tools_to_a_live_token_on_loopback_only() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, router, thread) = fixture(dir.path()).await;
    assert_eq!(
        bridge.mcp_url(),
        Some("http://127.0.0.1:4100/api/cli-bridge/mcp")
    );
    let token = bridge.mint_token(
        SessionGrant {
            thread_id: thread,
            run_id: "r".into(),
        },
        SESSION_TOKEN_TTL,
    );
    let list = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});

    let (status, body) = post(&router, [127, 0, 0, 1], &token, list.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["tools"].as_array().unwrap().len(), 6);
    // A call runs as the token's thread.
    let call = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "t3_capabilities", "arguments": {}}});
    let (status, body) = post(&router, [127, 0, 0, 1], &token, call).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["isError"], false, "{body}");

    let (id, _) = token.split_once('.').unwrap();
    for bad in ["", "nope", &format!("{id}.wrong")] {
        let (status, _) = post(&router, [127, 0, 0, 1], bad, list.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "token {bad:?}");
    }
    let (status, _) = post(&router, [192, 168, 1, 7], &token, list.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    bridge.revoke_token(&token);
    let (status, _) = post(&router, [127, 0, 0, 1], &token, list).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
