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
/// Key of a tool output's screenshot number, removed before the model reads
/// the output and handed to [`ComputerUseSessions::bind_result_frame`].
pub const COMPUTER_USE_RESULT_FRAME: &str = "frame";

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
    /// The `computer_use.state` view item: the live view of every session
    /// of this (tenant, user), newest first. `viewer` marks a polling viewer
    /// alive, which keeps its take over.
    fn viewer_state(&self, tenant_id: &TenantId, user_id: &UserId, viewer: Option<&str>) -> Value;
    /// The `computer_use.frame` view item: the newest picture of a window
    /// `session` used (waits briefly for one newer than `after_seq`).
    async fn window_frame(
        &self,
        session: &ComputerUseSession,
        request: &ComputerUseFrameRequest,
    ) -> Value;
    /// The `computer_use.control` command output.
    async fn control(
        &self,
        session: &ComputerUseSession,
        action: ComputerUseControlAction,
        viewer: Option<&str>,
    ) -> Value;
    /// Tie screenshot `seq` of `session` to the tool result that reported it,
    /// so the model sees that picture with that result.
    fn bind_result_frame(&self, _session: &ComputerUseSession, _seq: u64, _result_ref: &str) {}
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
    /// The viewer taking over; its take over is released when it stops
    /// polling the state view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
}

/// `GET /api/webchat/v2/computer-use?viewer=<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseStateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
}

/// Output: `{"now", "enabled", "human_has_control", "sessions": [{thread_id,
/// apps: [{pid, window_id, app, title, last_used, width?, height?, capture}],
/// inflight, label, action, status, ok, code, pointer, typing, last_activity,
/// ended, last_failure, human_has_control, stopping, watchable}]}`.
pub const COMPUTER_USE_STATE_VIEW: ProductView<ComputerUseStateRequest, Value> =
    ProductView::unpaginated("computer_use.state");

/// `GET /api/webchat/v2/threads/{thread_id}/computer-use/frame`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseFrameRequest {
    pub thread_id: String,
    pub window_id: u32,
    /// Pixel width of the picture (160..=1600, default 640).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Frames per second the viewer wants while it keeps asking (0.2..=10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    /// Answer only with a picture newer than this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<u64>,
}

/// Output: `{"ok": true, "frame": {seq, mimeType, data, width, height,
/// method, ageMs} | null}` or `{"ok": false, "code": "ended" |
/// "human_has_control" | "capture_failed", "detail"?}`.
pub const COMPUTER_USE_FRAME_VIEW: ProductView<ComputerUseFrameRequest, Value> =
    ProductView::unpaginated("computer_use.frame");

pub const COMPUTER_USE_CONTROL_COMMAND_ID: &str = "computer_use.control";
pub const COMPUTER_USE_CONTROL_COMMAND: ProductSurfaceCommandDescriptor<
    ComputerUseControlRequest,
    Value,
> = ProductSurfaceCommandDescriptor::new(COMPUTER_USE_CONTROL_COMMAND_ID);
