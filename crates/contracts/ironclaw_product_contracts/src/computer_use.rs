//! Computer use across the product boundary: the session view and control
//! descriptors behind the WebUI routes, and the [`ComputerUseSessions`] port.
//!
//! The port is declared here because its caller and its implementor sit on
//! opposite sides of a boundary product may not cross: `ironclaw_assistant`
//! (approval-gated tools, the Stop hook, the routes) calls it, and
//! `ironclaw_computer_use` (a `runtimes` crate over the MCP lane, which
//! product must not name) implements it. A session belongs to one (tenant,
//! user, thread), so a caller only ever reads or controls its own.

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_host_api::resource::ResourceScope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};

/// Capability ids are `computer_use.<tool>`; model tool names
/// `computer_use_<tool>`.
pub const COMPUTER_USE_CAPABILITY_PREFIX: &str = "computer_use.";
pub const COMPUTER_USE_TOOL_NAME_PREFIX: &str = "computer_use_";

/// One computer-use session's owner and thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComputerUseSession {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub thread_id: ThreadId,
}

impl ComputerUseSession {
    /// `None` for a scope without a thread: computer use is per thread.
    pub fn from_scope(scope: &ResourceScope) -> Option<Self> {
        Some(Self {
            tenant_id: scope.tenant_id.clone(),
            user_id: scope.user_id.clone(),
            thread_id: scope.thread_id.clone()?,
        })
    }
}

/// A server tool as its `tools/list` defines it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputerUseToolSpec {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Implemented by `ironclaw_computer_use::ComputerUse`. Every method answers
/// with a document instead of failing: a refused or failed action is
/// model-visible, never a host error.
#[async_trait]
pub trait ComputerUseSessions: Send + Sync {
    fn tools(&self) -> &[ComputerUseToolSpec];
    /// Run one already-approved tool call in the session of `scope`'s
    /// thread. Returns the model-visible document.
    async fn call_tool(&self, scope: ResourceScope, tool: &str, input: Value) -> Value;
    /// The `computer_use.session` view item for `session`.
    fn session_view(&self, session: &ComputerUseSession, after_frame: Option<u64>) -> Value;
    /// The `computer_use.control` command output.
    async fn control(
        &self,
        session: &ComputerUseSession,
        action: ComputerUseControlAction,
    ) -> Value;
    /// The thread's Stop fired: end its sessions and kill their processes.
    async fn thread_stopped(&self, tenant_id: &TenantId, thread_id: &ThreadId);
    /// A turn on the thread ended normally.
    async fn turn_ended(&self, tenant_id: &TenantId, thread_id: &ThreadId);
}

/// `GET /api/webchat/v2/threads/{thread_id}/computer-use`. `after_frame`
/// skips the frame body when the caller already has that sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseSessionRequest {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_frame: Option<u64>,
}

/// Output: `{"enabled", "session": {state, processRunning, ...}, "frame":
/// {seq, capturedAtMs, mimeType, data, tool, app?, window?} | null}`.
pub const COMPUTER_USE_SESSION_VIEW: ProductView<ComputerUseSessionRequest, Value> =
    ProductView::unpaginated("computer_use.session");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUseControlAction {
    /// Abort the running action, end the session, refuse actions briefly.
    Stop,
    /// The user takes the desktop; the agent's actions are refused.
    TakeOver,
    /// Give the desktop back to the agent.
    HandBack,
}

/// `POST /api/webchat/v2/threads/{thread_id}/computer-use`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseControlRequest {
    pub thread_id: String,
    pub action: ComputerUseControlAction,
}

pub const COMPUTER_USE_CONTROL_COMMAND_ID: &str = "computer_use.control";
pub const COMPUTER_USE_CONTROL_COMMAND: ProductSurfaceCommandDescriptor<
    ComputerUseControlRequest,
    Value,
> = ProductSurfaceCommandDescriptor::new(COMPUTER_USE_CONTROL_COMMAND_ID);
