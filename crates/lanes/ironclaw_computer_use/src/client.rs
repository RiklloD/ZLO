//! The two seams [`ComputerUse`] implements:
//!
//! - the MCP lane's [`McpClient`]: a host-normalized `tools/call` request in,
//!   the model-visible document out. The request's `command`/`args`/`url`
//!   are ignored; the binary is host config only;
//! - the product port [`ComputerUseSessions`], whose tool calls go through
//!   that client seam.

use async_trait::async_trait;
use ironclaw_host_api::ids::{CapabilityId, ExtensionId, TenantId, ThreadId, UserId};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_mcp::{McpClient, McpClientError, McpClientOutput, McpClientRequest};
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CAPABILITY_PREFIX, ComputerUseControlAction, ComputerUseFrameRequest,
    ComputerUseSession, ComputerUseSessions, ComputerUseToolSpec,
};
use serde_json::{Value, json};

use crate::catalog::{computer_use_tools, tool_for_capability};
use crate::session::ComputerUse;

/// Transport name on the requests this crate builds.
pub const COMPUTER_USE_TRANSPORT: &str = "stdio";
/// Model-visible tool output cap (the tree text). Screenshots stay host-side
/// as session frames; the model port hands each result's own to vision models.
const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

#[async_trait]
impl McpClient for ComputerUse {
    async fn call_tool(
        &self,
        request: McpClientRequest,
    ) -> Result<McpClientOutput, McpClientError> {
        let Some(tool) = tool_for_capability(request.capability_id.as_str()) else {
            return Err(McpClientError::client("computer_use_unknown_tool"));
        };
        let Some(session) = ComputerUseSession::from_scope(&request.scope) else {
            return Err(McpClientError::client("computer_use_requires_thread"));
        };
        let limit = usize::try_from(request.max_output_bytes).unwrap_or(usize::MAX);
        let outcome = self
            .call_with_images(&session, &tool.name, request.input, limit, true)
            .await;
        Ok(McpClientOutput::json(outcome.to_json()))
    }
}

fn client_request(
    scope: ResourceScope,
    tool: &str,
    input: Value,
) -> Result<McpClientRequest, McpClientError> {
    let unknown = |error: ironclaw_host_api::error::HostApiError| {
        tracing::debug!(%error, "computer-use request id is invalid");
        McpClientError::client("computer_use_unknown_tool")
    };
    Ok(McpClientRequest {
        provider: ExtensionId::new("computer_use").map_err(unknown)?,
        capability_id: CapabilityId::new(format!("{COMPUTER_USE_CAPABILITY_PREFIX}{tool}"))
            .map_err(unknown)?,
        scope,
        transport: COMPUTER_USE_TRANSPORT.to_string(),
        command: None,
        args: Vec::new(),
        url: None,
        input,
        max_output_bytes: MAX_OUTPUT_BYTES,
    })
}

#[async_trait]
impl ComputerUseSessions for ComputerUse {
    fn tools(&self) -> &[ComputerUseToolSpec] {
        computer_use_tools()
    }

    async fn call_tool(&self, scope: ResourceScope, tool: &str, input: Value) -> Value {
        let result = match client_request(scope, tool, input) {
            Ok(request) => McpClient::call_tool(self, request).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(output) => output.output,
            Err(error) => json!({"ok": false, "code": error.stable_reason()}),
        }
    }

    fn bind_result_frame(&self, session: &ComputerUseSession, seq: u64, result_ref: &str) {
        ComputerUse::bind_result_frame(self, session, seq, result_ref);
    }

    fn session_view(&self, session: &ComputerUseSession, after_frame: Option<u64>) -> Value {
        json!({
            "enabled": true,
            "session": self.status(session),
            "frame": self.latest_frame(session, after_frame).as_deref(),
        })
    }

    fn viewer_state(&self, tenant_id: &TenantId, user_id: &UserId, viewer: Option<&str>) -> Value {
        ComputerUse::viewer_state(self, &(tenant_id.clone(), user_id.clone()), viewer)
    }

    async fn window_frame(
        &self,
        session: &ComputerUseSession,
        request: &ComputerUseFrameRequest,
    ) -> Value {
        ComputerUse::window_frame(
            self,
            session,
            request.window_id,
            request.width.unwrap_or(640),
            request.fps.unwrap_or(8.0),
            request.after_seq,
        )
        .await
    }

    async fn control(
        &self,
        session: &ComputerUseSession,
        action: ComputerUseControlAction,
        viewer: Option<&str>,
    ) -> Value {
        let status = match action {
            ComputerUseControlAction::Stop => self.stop(session).await,
            ComputerUseControlAction::TakeOver => {
                self.take_over_as(session, viewer.unwrap_or("route"))
            }
            ComputerUseControlAction::HandBack => self.hand_back(session),
        };
        json!({"ok": true, "enabled": true, "session": status})
    }

    async fn thread_stopped(&self, tenant_id: &TenantId, thread_id: &ThreadId) {
        self.stop_thread(tenant_id, thread_id).await;
    }

    async fn turn_ended(&self, tenant_id: &TenantId, thread_id: &ThreadId) {
        ComputerUse::turn_ended(self, tenant_id, thread_id).await;
    }
}
