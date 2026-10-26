//! The one place a CLI agent's tool-permission request is decided.
//!
//! The user keeps every tool call behind an approval. With the host's
//! [`CliBridge`], a request waits as a gate in the thread until its owner
//! answers it (and is recorded with the answer); anything but an explicit
//! approval is a denial. Without a bridge there is nobody to ask, so every
//! request is denied. Nothing is remembered; the only auto-approval is a
//! thread whose profile is on "Full access" (checked inside the bridge).

use ironclaw_host_api::ids::ThreadId;
use serde_json::Value;

use crate::bridge::{CallClaim, CliBridge, ToolDecision};

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
    /// The agent's own justification for the call, when its protocol has
    /// one; otherwise the bridge uses what the model said before it.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    Deny,
}

/// Match a request to its turn where the driver reads it, before spawning
/// the wait (see [`CliBridge::claim_call`]).
pub fn claim(bridge: Option<&CliBridge>, thread: &ThreadId, run_id: &str) -> CallClaim {
    bridge.map_or_else(CallClaim::default, |b| b.claim_call(thread, run_id))
}

/// Decide one permission request of `thread`'s running turn `run_id`,
/// claimed with [`claim`].
pub async fn decide(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    run_id: &str,
    request: &PermissionRequest,
    claim: CallClaim,
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
        .decide_call(
            thread,
            run_id,
            &request.tool,
            &input,
            request.reason.clone(),
            claim,
        )
        .await
    {
        ToolDecision::Allow => PermissionDecision::AllowOnce,
        ToolDecision::Deny(_) => PermissionDecision::Deny,
    }
}
