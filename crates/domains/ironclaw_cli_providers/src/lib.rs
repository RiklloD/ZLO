//! Coding-agent CLIs as `ironclaw_orchestrator` turn runtimes.
//!
//! Today: the Claude CLI, as two runtimes (`claude-cli` on the default
//! account, `claude-cli-2` on `~/.claude-account2`). Antigravity and Muse
//! Code drivers follow on the same seam.

pub mod claude;

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::runtime::TurnEventSink;

/// Register the Claude CLI runtimes on `engine` when a `claude` binary is
/// installed. Returns how many were registered.
pub fn register_claude_runtimes(engine: &Arc<Engine>) -> usize {
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
        match claude::ClaudeCliRuntime::new(config, sink.clone()) {
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
