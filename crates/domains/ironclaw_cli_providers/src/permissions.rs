//! The one place a CLI agent's tool-permission request is decided.
//!
//! The user keeps every tool call behind an approval. With the host's
//! [`CliBridge`], a request waits as a gate in the thread until its owner
//! answers it (and is recorded with the answer); anything but an explicit
//! approval is a denial. Without a bridge there is nobody to ask, so every
//! request is denied. Nothing is remembered; the only auto-approval is a
//! thread whose profile is on "Full access" (checked inside the bridge).

use ironclaw_host_api::ids::ThreadId;
use ironclaw_host_api::turn::DenialFeedback;
use serde_json::Value;

use crate::bridge::{CallClaim, CliBridge, Receipt, ToolDecision};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    /// Not run. `feedback` is what the person said instead ("no, do X
    /// instead"), for drivers whose protocol cannot carry a denial message
    /// to hand to the agent another way (a steer). It never makes the call
    /// run.
    Deny {
        feedback: Option<DenialFeedback>,
    },
}

impl PermissionDecision {
    pub const DENY: Self = Self::Deny { feedback: None };
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
    decide_receipted(bridge, thread, run_id, request, claim)
        .await
        .0
}

/// [`decide`], plus the [`Receipt`] the driver reports on once its reply
/// reached the agent (a steer that declined the call waits on it).
pub async fn decide_receipted(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    run_id: &str,
    request: &PermissionRequest,
    claim: CallClaim,
) -> (PermissionDecision, Receipt) {
    let Some(bridge) = bridge else {
        tracing::debug!(
            runtime = %request.runtime,
            tool = %request.tool,
            "cli agent permission request denied (no approval bridge)"
        );
        return (PermissionDecision::DENY, Receipt::none());
    };
    let input = request.subject.clone().map_or(Value::Null, Value::String);
    let (decision, receipt) = bridge
        .decide_call_receipted(
            thread,
            run_id,
            &request.tool,
            &input,
            request.reason.clone(),
            claim,
        )
        .await;
    let decision = match decision {
        ToolDecision::Allow => PermissionDecision::AllowOnce,
        ToolDecision::Deny(_) => PermissionDecision::DENY,
        ToolDecision::Rejected(feedback) => PermissionDecision::Deny {
            feedback: Some(feedback),
        },
    };
    (decision, receipt)
}
