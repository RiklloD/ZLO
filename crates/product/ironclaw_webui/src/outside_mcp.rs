//! `POST /api/mcp` — this environment's orchestrator tools for outside
//! agents (Claude Code or Codex in a terminal, any MCP client) over MCP's
//! streamable-HTTP transport, answered as plain JSON.
//!
//! The agent authenticates with a paired device token (`zlo pair --scope
//! orchestration:read --scope orchestration:operate`); its scopes set what
//! it may do, exactly as a peer link's access does: read-only devices list
//! and read, operating devices also start and message threads — fenced to
//! threads they started — and a device with every scope acts like this
//! machine's own agents. Work it starts is marked with the device's name.
//! The calls run through the same fenced path as peer links
//! (`PeerToolHandler`), never around it.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ingress::BodyLimitPolicy;
use ironclaw_host_ingress::SplitRouteMount;
use ironclaw_product_contracts::environment_access::{DeviceId, DeviceScope, PeerLinkAccess};
use ironclaw_product_contracts::peer::{OUTSIDE_MCP_PATH, PeerCaller, PeerToolHandler};
use serde_json::{Value, json};

use crate::device_scopes::CallerGrant;
use crate::environment_access::{descriptor, operator_policy};

const MCP_BODY_LIMIT: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(512 * 1024).expect("512 KiB != 0"); // safety: const-evaluated, literal non-zero
const MCP_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(600).expect("600 != 0"); // safety: const-evaluated, literal non-zero
const PROTOCOL_VERSION: &str = "2025-06-18";
/// The parent-thread key a non-device bearer (the master token) acts as.
const OPERATOR_DEVICE: &str = "00000000000000000000000000000000";

/// Mount `POST /api/mcp` over `tools`.
pub fn outside_mcp_route_mount(tools: Arc<dyn PeerToolHandler>) -> SplitRouteMount {
    let protected = Router::new()
        .route(OUTSIDE_MCP_PATH, post(mcp))
        .with_state(tools);
    SplitRouteMount::new(
        protected,
        Router::new(),
        vec![descriptor(
            "webui.outside_mcp",
            NetworkMethod::Post,
            OUTSIDE_MCP_PATH,
            operator_policy(
                BodyLimitPolicy::Limited {
                    max_bytes: MCP_BODY_LIMIT,
                },
                MCP_MAX_REQUESTS,
            ),
        )],
    )
}

/// What an outside agent may do, from its device scopes.
fn access_of(grant: &CallerGrant) -> Option<(PeerLinkAccess, DeviceId, String)> {
    let Some(device) = grant.0.as_ref() else {
        let id = DeviceId::new(OPERATOR_DEVICE).ok()?;
        return Some((PeerLinkAccess::Full, id, "this machine's operator".into()));
    };
    // Peer links use their own route.
    if device.peer_link.is_some() {
        return None;
    }
    let access = if DeviceScope::ALL.iter().all(|s| device.allows(*s)) {
        PeerLinkAccess::Full
    } else if device.allows(DeviceScope::OrchestrationOperate) {
        PeerLinkAccess::Operate
    } else if device.allows(DeviceScope::OrchestrationRead) {
        PeerLinkAccess::ReadOnly
    } else {
        return None;
    };
    Some((access, device.device_id.clone(), device.label.clone()))
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
        .into_response()
}

async fn mcp(
    State(tools): State<Arc<dyn PeerToolHandler>>,
    grant: Option<Extension<CallerGrant>>,
    Json(request): Json<Value>,
) -> Response {
    let grant = grant.map(|Extension(g)| g).unwrap_or(CallerGrant(None));
    let Some((access, device_id, label)) = access_of(&grant) else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "scope_required", "scope": "orchestration:read",
                "message": "Pair this agent with at least orchestration:read (`zlo pair --scope orchestration:read`)."})),
        )
            .into_response();
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str().unwrap_or_default();
    // Notifications get no answer body.
    if request.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    match method {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "zlo", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "ZLO orchestrator tools: t3_capabilities lists providers and linked machines; \
                    t3_delegate starts work in a new thread; t3_task follows it. Pass thread ids explicitly.",
            }),
        ),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({"tools": tools.tool_catalog()})),
        "tools/call" => {
            let name = request["params"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if !tools.tool_catalog().iter().any(|t| t.name == name) {
                return rpc_error(id, -32602, "unknown tool");
            }
            let arguments = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let caller = PeerCaller {
                device_id,
                label,
                access,
                caller_key: "outside-agent".into(),
            };
            let answer = tools.call_peer_tool(caller, &name, arguments).await;
            let is_error = answer.get("ok") == Some(&Value::Bool(false));
            rpc_result(
                id,
                json!({
                    "content": [{"type": "text", "text": answer.to_string()}],
                    "structuredContent": answer,
                    "isError": is_error,
                }),
            )
        }
        _ => rpc_error(id, -32601, "method not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_product_contracts::environment_access::DeviceGrant;

    fn device(scopes: Vec<DeviceScope>) -> CallerGrant {
        CallerGrant(Some(DeviceGrant {
            device_id: DeviceId::new("0123456789abcdef0123456789abcdef").unwrap(),
            label: "terminal".into(),
            scopes,
            peer_link: None,
        }))
    }

    #[test]
    fn scopes_decide_what_an_outside_agent_may_do() {
        assert_eq!(
            access_of(&device(DeviceScope::all())).unwrap().0,
            PeerLinkAccess::Full
        );
        assert_eq!(
            access_of(&device(vec![
                DeviceScope::OrchestrationRead,
                DeviceScope::OrchestrationOperate
            ]))
            .unwrap()
            .0,
            PeerLinkAccess::Operate
        );
        assert_eq!(
            access_of(&device(vec![DeviceScope::OrchestrationRead]))
                .unwrap()
                .0,
            PeerLinkAccess::ReadOnly
        );
        assert!(access_of(&device(vec![DeviceScope::FilesystemRead])).is_none());
        assert_eq!(
            access_of(&CallerGrant(None)).unwrap().0,
            PeerLinkAccess::Full
        );
    }
}
