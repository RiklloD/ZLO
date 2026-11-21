//! What a turn gives the agent besides the user's text: the session notes
//! ([`TurnContext`]) and ZLO's orchestrator tools over the loopback MCP
//! route, under a token bound to the turn ([`McpToken`]).

use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::runtime::TurnProfile;
use serde_json::{Value, json};

use super::launch::AgentLaunch;
use crate::bridge::{CliBridge, MCP_SERVER_NAME, SESSION_TOKEN_TTL, SessionGrant};

/// What every Antigravity session is told about the T3 tools' names here
/// and its own subagent tools.
pub(super) const ANTIGRAVITY_TOOLS_PROMPT: &str =
    include_str!("../../prompts/antigravity_tools.md");

/// `agy`'s own subagent and fan-out tools. Subagents are orchestrator
/// threads (`t3_delegate`); `agy` runs these without asking today, so this
/// is a backstop for a version that does ask: the request is refused before
/// any gate or "Full access" shortcut.
pub(super) const AGY_SUBAGENT_TOOLS: [&str; 3] =
    ["invoke_subagent", "start_subagent", "run_workflow"];
/// What the agent reads when it reaches for one of them.
pub(super) const AGY_SUBAGENT_DENIAL: &str = "Built-in subagents and workflows are switched off here. \
     Start a subagent with mcp_ironclaw_t3_delegate instead, which opens a thread the user can see and steer.";

/// What a turn sends besides the user's text.
#[derive(Default)]
pub(super) struct TurnContext {
    /// Session notes and goal, whole: for an agent that appends them to its
    /// system prompt (it keeps none of it between processes).
    pub(super) system: Option<String>,
    /// Session notes (first turn this process runs for the thread) and
    /// goal, put ahead of the prompt for an agent that cannot.
    pub(super) inline: Option<String>,
}

impl TurnContext {
    /// The session notes are the profile's preamble (SOUL.md, skills) when
    /// there is a profile, the delegation and visuals notes always, and the
    /// Antigravity tool note.
    pub(super) fn new(
        primed: &crate::Primed,
        thread: &ThreadId,
        profile: Option<&TurnProfile>,
        goal: Option<&str>,
    ) -> Self {
        let notes = crate::join_notes([
            crate::session_context(profile),
            Some(ANTIGRAVITY_TOOLS_PROMPT.trim().to_string()),
        ]);
        Self {
            system: crate::prompt_context(notes.clone(), goal),
            inline: crate::prompt_context(primed.take_context(thread, notes), goal),
        }
    }
}

/// One turn's bearer token for the loopback MCP route. Dropping it (the
/// turn ended, or its task unwound) revokes the token.
pub(super) struct McpToken {
    bridge: Arc<CliBridge>,
    url: String,
    token: String,
}

impl McpToken {
    /// The turn's token: `None` without a bridge, before the gateway
    /// reported its URL, or on the legacy runtime (its MCP support was
    /// never checked).
    pub(super) fn mint(
        launch: &AgentLaunch,
        bridge: Option<&Arc<CliBridge>>,
        thread: &ThreadId,
        run_id: &str,
    ) -> Option<Self> {
        if !matches!(launch, AgentLaunch::Cli { .. }) {
            return None;
        }
        let bridge = bridge?;
        let url = bridge.mcp_url()?.to_string();
        let token = bridge.mint_token(
            SessionGrant {
                thread_id: thread.clone(),
                run_id: run_id.to_string(),
            },
            SESSION_TOKEN_TTL,
        );
        Some(Self {
            bridge: Arc::clone(bridge),
            url,
            token,
        })
    }

    /// The `mcpServers` entry ACP takes for an HTTP server.
    pub(super) fn server(&self) -> Value {
        json!({
            "type": "http",
            "name": MCP_SERVER_NAME,
            "url": self.url,
            "headers": [{"name": "Authorization", "value": format!("Bearer {}", self.token)}],
        })
    }
}

impl Drop for McpToken {
    fn drop(&mut self) {
        self.bridge.revoke_token(&self.token);
    }
}
