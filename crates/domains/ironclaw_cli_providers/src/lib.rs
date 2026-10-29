//! Coding-agent CLIs as `ironclaw_orchestrator` turn runtimes.
//!
//! The Claude CLI, as two runtimes (`claude-cli` on the default account,
//! `claude-cli-2` on `~/.claude-account2`), the Antigravity ACP agent
//! (`antigravity`), Meta's Muse Code (`muse-code`) and OpenAI's Codex CLI
//! (`codex`). Every tool call any
//! of them asks to make waits for an IronClaw approval in
//! [`bridge::CliBridge`]; without a bridge, it is denied.

pub mod approval_reason;
mod attachments;
pub mod bridge;
pub mod claude;

pub mod antigravity;
pub mod codex;
mod journal;
pub mod muse;
pub mod permissions;
mod rpc;

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_host_api::reply_visuals::REPLY_VISUALS_PROMPT;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::runtime::{TurnEventSink, TurnProfile};

/// How a thread's goal reaches a CLI agent: added to the system prompt, or
/// put ahead of the prompt where the protocol has no system prompt.
pub(crate) fn goal_note(goal: &str) -> String {
    format!("The user set this goal for the thread; keep working toward it: {goal}")
}

/// What a CLI agent with a system prompt flag gets there: the profile's
/// SOUL.md, the visuals format, then the thread goal.
pub(crate) fn system_note(profile: Option<&TurnProfile>, goal: Option<&str>) -> Option<String> {
    let soul = profile
        .map(|p| p.soul.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    join_notes([soul, visuals(), goal.map(goal_note)])
}

/// How to put a chart, table or stat tiles in a reply (Numoh draws them).
fn visuals() -> Option<String> {
    Some(REPLY_VISUALS_PROMPT.trim().to_string())
}

/// The profile context for agents without a system prompt or a skill
/// system, sent ahead of a session's first prompt: SOUL.md plus the index
/// of the profile's skills.
pub(crate) fn session_preamble(profile: &TurnProfile) -> Option<String> {
    let soul = Some(profile.soul.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let skills = profile
        .skills
        .as_ref()
        .filter(|s| !s.index.trim().is_empty())
        .map(|s| {
            format!(
                "Skills available on this profile. Before a task one of them covers, read its SKILL.md and follow it:\n{}",
                s.index.trim()
            )
        });
    join_notes([soul, visuals(), skills])
}

fn join_notes(parts: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// What goes ahead of the prompt for agents without a system prompt flag:
/// the profile preamble (first turn this process runs for the thread), then
/// the goal.
pub(crate) fn prompt_context(preamble: Option<String>, goal: Option<&str>) -> Option<String> {
    join_notes([preamble, goal.map(goal_note)])
}

/// The prompt as sent on the wire, with its context ahead of it.
pub(crate) fn with_context(text: &str, context: Option<&str>) -> String {
    match context {
        Some(context) => format!("{context}\n\n{text}"),
        None => text.to_string(),
    }
}

/// Threads whose session already got the profile preamble in this process.
/// A backend restart sends it once more, which also refreshes it.
#[derive(Debug, Default)]
pub(crate) struct Primed(std::sync::Mutex<std::collections::HashSet<ThreadId>>);

impl Primed {
    /// The preamble to send now: `Some` only the first time per thread.
    pub(crate) fn take(&self, thread: &ThreadId, profile: Option<&TurnProfile>) -> Option<String> {
        let preamble = session_preamble(profile?)?;
        let mut primed = self.0.lock().ok()?; // silent-ok: a poisoned set only follows a panic elsewhere
        primed.insert(thread.clone()).then_some(preamble)
    }
}

/// Register every installed coding-agent CLI on `engine`, all with their
/// tools behind `bridge`'s approvals, which are recorded in the engine's
/// store. Returns how many runtimes were registered.
pub fn register_cli_runtimes(engine: &Arc<Engine>, bridge: &Arc<bridge::CliBridge>) -> usize {
    bridge.record_to(engine.store().clone());
    register_claude_runtimes(engine, Some(Arc::clone(bridge)))
        + register_antigravity_runtime(engine, Some(Arc::clone(bridge)))
        + register_muse_runtime(engine, Some(Arc::clone(bridge)))
        + register_codex_runtime(engine, Some(Arc::clone(bridge)))
}

/// Register the Codex CLI runtime when a `codex` binary is installed. With
/// `bridge`, it writes in its workspace sandbox and every approval it asks
/// for is an IronClaw approval; without, the sandbox is read-only. Returns
/// how many were registered (0 or 1).
pub fn register_codex_runtime(
    engine: &Arc<Engine>,
    bridge: Option<Arc<bridge::CliBridge>>,
) -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    let Some(binary) = codex::find_binary(&home) else {
        tracing::debug!("codex cli not found; codex runtime not registered");
        return 0;
    };
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(engine) as Weak<dyn TurnEventSink>;
    match codex::CodexRuntime::new(codex::default_config(&home, &binary), sink, bridge) {
        Ok(runtime) => {
            engine.register_runtime(Arc::new(runtime));
            1
        }
        Err(error) => {
            tracing::debug!(%error, "codex runtime id rejected");
            0
        }
    }
}

/// Register the Claude CLI runtimes on `engine` when a `claude` binary is
/// installed. With `bridge`, their tools run behind IronClaw approvals;
/// without, tools are off. Returns how many were registered.
pub fn register_claude_runtimes(
    engine: &Arc<Engine>,
    bridge: Option<Arc<bridge::CliBridge>>,
) -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    let Some(binary) = claude::find_binary(&home) else {
        tracing::debug!("claude cli not found; claude runtimes not registered");
        return 0;
    };
    spawn_version_check(binary.clone());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(engine) as Weak<dyn TurnEventSink>;
    let mut registered = 0;
    for config in claude::default_accounts(&home, &binary) {
        match claude::ClaudeCliRuntime::new(config, sink.clone(), bridge.clone()) {
            Ok(runtime) => {
                engine.register_runtime(Arc::new(runtime));
                registered += 1;
            }
            Err(error) => tracing::debug!(%error, "claude runtime id rejected"),
        }
    }
    registered
}

/// Note a CLI newer or older than the one the wire handling was checked on.
fn spawn_version_check(binary: PathBuf) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let out = tokio::process::Command::new(&binary)
            .arg("--version")
            .output()
            .await;
        let version = out
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()) // silent-ok: version check is advisory
            .unwrap_or_default();
        if !version.starts_with(claude::EXPECTED_CLI_VERSION) {
            tracing::debug!(
                found = %version,
                expected = claude::EXPECTED_CLI_VERSION,
                "claude cli version differs from the one the driver was checked against"
            );
        }
    });
}

/// Register the Antigravity runtime when its ACP server is installed. With
/// `bridge`, its permission requests become IronClaw approvals; without,
/// they are denied. Returns how many were registered (0 or 1).
pub fn register_antigravity_runtime(
    engine: &Arc<Engine>,
    bridge: Option<Arc<bridge::CliBridge>>,
) -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    let Some(config) = antigravity::default_config(&home) else {
        tracing::debug!("antigravity acp runtime not found; antigravity runtime not registered");
        return 0;
    };
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(engine) as Weak<dyn TurnEventSink>;
    match antigravity::AntigravityRuntime::new(config, sink, bridge) {
        Ok(runtime) => {
            engine.register_runtime(Arc::new(runtime));
            1
        }
        Err(error) => {
            tracing::debug!(%error, "antigravity runtime id rejected");
            0
        }
    }
}

/// Register the Muse Code runtime when a `muse` binary is installed. With
/// `bridge`, shell and writes are on and every approval Muse asks for is an
/// IronClaw approval; without, both stay off and approvals are denied.
/// Returns how many were registered (0 or 1).
pub fn register_muse_runtime(
    engine: &Arc<Engine>,
    bridge: Option<Arc<bridge::CliBridge>>,
) -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    let Some(binary) = muse::find_binary(&home) else {
        tracing::debug!("muse cli not found; muse runtime not registered");
        return 0;
    };
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(engine) as Weak<dyn TurnEventSink>;
    match muse::MuseRuntime::new(muse::default_config(&home, &binary), sink, bridge) {
        Ok(runtime) => {
            engine.register_runtime(Arc::new(runtime));
            1
        }
        Err(error) => {
            tracing::debug!(%error, "muse runtime id rejected");
            0
        }
    }
}
