//! What a turn gives the agent besides the user's text: the session notes
//! ([`TurnContext`]) and ZLO's orchestrator tools over the loopback MCP
//! route, under a token bound to the turn ([`McpToken`]).

use std::collections::HashMap;
use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::runtime::TurnProfile;
use serde_json::{Value, json};

use super::launch::AgentLaunch;
use crate::bridge::{CliBridge, MCP_SERVER_NAME, SESSION_TOKEN_TTL, SessionGrant};
use crate::context_meter::Notes;

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

/// Which session notes each thread's agent already got ahead of a prompt in
/// this process (a hash of the notes). Committed only once the prompt that
/// carried them is on the wire, so a turn that failed to launch sends them
/// again; notes that changed (a profile was picked) are sent again too.
#[derive(Debug, Default)]
pub(super) struct InlinePrimed(std::sync::Mutex<HashMap<ThreadId, u64>>);

impl InlinePrimed {
    fn delivered(&self, thread: &ThreadId) -> Option<u64> {
        self.0.lock().ok()?.get(thread).copied() // silent-ok: a poisoned map only follows a panic elsewhere; the notes go again
    }

    fn commit(&self, thread: &ThreadId, stamp: u64) {
        if let Ok(mut primed) = self.0.lock() {
            primed.insert(thread.clone(), stamp);
        }
    }
}

/// The inline notes a turn carries, to mark delivered once they are sent.
struct PrimeTicket {
    primed: Arc<InlinePrimed>,
    thread: ThreadId,
    stamp: u64,
}

fn stamp(notes: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    notes.hash(&mut hasher);
    hasher.finish()
}

/// What a turn sends besides the user's text.
#[derive(Default)]
pub(super) struct TurnContext {
    /// Session notes and goal, whole: for an agent that appends them to its
    /// system prompt (it keeps none of it between processes).
    pub(super) system: Option<String>,
    /// Session notes (until delivered for the thread in this process) and
    /// goal, put ahead of the prompt for an agent that cannot.
    pub(super) inline: Option<String>,
    ticket: Option<PrimeTicket>,
    /// The session notes and this turn's own, as the context meter sees
    /// them (the agent holds the session notes either way).
    pub(super) session: Notes,
    pub(super) turn: Notes,
}

impl TurnContext {
    /// The session notes are the profile's preamble (SOUL.md, skills) when
    /// there is a profile, the delegation and visuals notes always, and the
    /// Antigravity tool note; `requested` (the skills this message asks
    /// for) joins them for this turn only.
    pub(super) fn new(
        primed: &Arc<InlinePrimed>,
        thread: &ThreadId,
        profile: Option<&TurnProfile>,
        requested: Option<&str>,
        goal: Option<&str>,
    ) -> Self {
        let notes = crate::session_context(profile)
            .instructions(Some(ANTIGRAVITY_TOOLS_PROMPT.trim().to_string()));
        let ticket = notes.text().as_deref().map(stamp).and_then(|stamp| {
            (primed.delivered(thread) != Some(stamp)).then(|| PrimeTicket {
                primed: Arc::clone(primed),
                thread: thread.clone(),
                stamp,
            })
        });
        let inline_notes = ticket.as_ref().map(|_| notes.clone()).unwrap_or_default();
        // The skills this message asks for ride every turn that asks, beside
        // (not inside) the stamped session notes.
        let turn = Notes::default()
            .instructions(requested.map(str::to_string))
            .then(crate::goal_notes(goal));
        Self {
            system: notes.clone().then(turn.clone()).text(),
            inline: inline_notes.then(turn.clone()).text(),
            ticket,
            session: notes,
            turn,
        }
    }

    /// The prompt that carried the inline notes is on the wire: later turns
    /// of this thread (in this process) go without them.
    pub(super) fn inline_delivered(&self) {
        if let Some(ticket) = &self.ticket {
            ticket.primed.commit(&ticket.thread, ticket.stamp);
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
        if !Self::available(launch, bridge) {
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

    /// Whether a turn would get a token now.
    pub(super) fn available(launch: &AgentLaunch, bridge: Option<&Arc<CliBridge>>) -> bool {
        matches!(launch, AgentLaunch::Cli { .. }) && bridge.is_some_and(|b| b.mcp_url().is_some())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn thread() -> ThreadId {
        ThreadId::new("thread-1").unwrap()
    }

    #[test]
    fn inline_notes_repeat_until_a_prompt_carried_them() {
        let primed = Arc::new(InlinePrimed::default());
        let first = TurnContext::new(&primed, &thread(), None, None, None);
        assert!(first.inline.is_some());
        // The launch failed before the prompt went out: the retry carries them.
        let retry = TurnContext::new(&primed, &thread(), None, None, None);
        assert_eq!(retry.inline, first.inline);
        retry.inline_delivered();
        let next = TurnContext::new(&primed, &thread(), None, None, Some("ship it"));
        assert_eq!(
            next.inline.as_deref().map(|i| i.contains("ship it")),
            Some(true)
        );
        assert!(
            !next
                .inline
                .unwrap()
                .contains(ANTIGRAVITY_TOOLS_PROMPT.trim()),
            "notes went once; the goal still rides along"
        );
    }

    #[test]
    fn a_profile_picked_later_still_reaches_the_agent() {
        let primed = Arc::new(InlinePrimed::default());
        TurnContext::new(&primed, &thread(), None, None, None).inline_delivered();
        let profile = TurnProfile {
            soul: "You are the release captain.".into(),
            ..TurnProfile::default()
        };
        let with_profile = TurnContext::new(&primed, &thread(), Some(&profile), None, None);
        assert!(
            with_profile
                .inline
                .as_deref()
                .is_some_and(|i| i.contains("release captain")),
        );
    }
}
