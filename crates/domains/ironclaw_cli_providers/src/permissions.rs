//! The one place a CLI agent's tool-permission request is decided.
//!
//! The user keeps every tool call behind IronClaw's approval queue
//! (`ironclaw_approvals`), and that bridge is not wired yet. Until it is,
//! every request is denied: never auto-approved, never remembered. When the
//! bridge lands, it replaces the body of [`decide`] (made async if it has to
//! wait for the user) and both the Antigravity and Muse drivers follow.

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

/// Decide one permission request. Fails closed: denies everything.
pub fn decide(request: &PermissionRequest) -> PermissionDecision {
    tracing::debug!(
        runtime = %request.runtime,
        tool = %request.tool,
        "cli agent permission request denied (approval bridge not wired)"
    );
    PermissionDecision::Deny
}
