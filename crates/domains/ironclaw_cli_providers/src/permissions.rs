//! The one place a CLI agent's tool-permission request is decided.
//!
//! The user keeps every tool call behind an approval. With the host's
//! [`CliBridge`], a request waits as a gate in the thread until its owner
//! answers it (and is recorded with the answer); anything but an explicit
//! approval is a denial. Without a bridge there is nobody to ask, so every
//! request is denied. Nothing is ever auto-approved or remembered.

use ironclaw_host_api::ids::ThreadId;
use serde_json::Value;

use crate::bridge::{CliBridge, ToolDecision};

/// A tool call an agent wants to run, as the drivers can describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Runtime id (`antigravity`, `muse-code`).
    pub runtime: String,
    /// Native session the request belongs to.
    pub session_id: String,
    /// Tool or action name, as the agent labels it.
    pub tool: String,
    /// Short human-readable subject (command, path, host), if any.
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    Deny,
}

/// Decide one permission request of `thread`'s running turn `run_id`.
pub async fn decide(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    run_id: &str,
    request: &PermissionRequest,
) -> PermissionDecision {
    let Some(bridge) = bridge else {
        tracing::debug!(
            runtime = %request.runtime,
            tool = %request.tool,
            "cli agent permission request denied (no approval bridge)"
        );
        return PermissionDecision::Deny;
    };
    let input = request.subject.clone().map_or(Value::Null, Value::String);
    match bridge
        .request_approval(thread, run_id, &request.tool, &input)
        .await
    {
        ToolDecision::Allow => PermissionDecision::AllowOnce,
        ToolDecision::Deny(_) => PermissionDecision::Deny,
    }
}
