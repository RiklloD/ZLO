//! Coding-agent CLIs as `ironclaw_orchestrator` turn runtimes.
//!
//! The Claude CLI, as two runtimes (`claude-cli` on the default account,
//! `claude-cli-2` on `~/.claude-account2`), the Antigravity ACP agent
//! (`antigravity`) and Meta's Muse Code (`muse-code`). Every tool-permission
//! request of the latter two goes through [`permissions::decide`].

pub mod bridge;
pub mod claude;

pub mod antigravity;
mod journal;
pub mod muse;
pub mod permissions;
mod rpc;

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::runtime::TurnEventSink;

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

/// Register the Antigravity runtime when its ACP server is installed.
/// Returns how many were registered (0 or 1).
pub fn register_antigravity_runtime(engine: &Arc<Engine>) -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    let Some(config) = antigravity::default_config(&home) else {
        tracing::debug!("antigravity acp runtime not found; antigravity runtime not registered");
        return 0;
    };
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(engine) as Weak<dyn TurnEventSink>;
    match antigravity::AntigravityRuntime::new(config, sink) {
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

/// Register the Muse Code runtime when a `muse` binary is installed.
/// Returns how many were registered (0 or 1).
pub fn register_muse_runtime(engine: &Arc<Engine>) -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    let Some(binary) = muse::find_binary(&home) else {
        tracing::debug!("muse cli not found; muse runtime not registered");
        return 0;
    };
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(engine) as Weak<dyn TurnEventSink>;
    match muse::MuseRuntime::new(muse::default_config(&home, &binary), sink) {
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
