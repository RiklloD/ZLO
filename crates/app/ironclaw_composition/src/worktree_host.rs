//! Worktree isolation's command runner: the orchestrator's git and
//! `.zlo/worktree.toml` setup commands run through the host process port
//! (`ironclaw_host_runtime::HostProcessPort`), with its scrubbed environment
//! plus `HOME` (git identity and config, package caches) and no terminal
//! prompts. Wired only next to the orchestrator, which exists only on the
//! local libSQL deployment whose CLI runtimes already run on this host.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::process::CommandExecutionRequest;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_host_runtime::{HostProcessPort, RuntimeProcessPort as _};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::worktree::{
    HookApprover, HostCommand, HostCommandOutput, HostCommandRunner, WorktreeHost,
};

pub(crate) struct HostProcessCommands {
    port: HostProcessPort,
    home: PathBuf,
}

impl HostProcessCommands {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self {
            port: HostProcessPort::new(),
            home,
        }
    }
}

#[async_trait]
impl HostCommandRunner for HostProcessCommands {
    async fn run(&self, command: HostCommand) -> Result<HostCommandOutput, OrchestratorError> {
        let env = HashMap::from([
            ("HOME".to_string(), self.home.to_string_lossy().into_owned()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ]);
        let request = CommandExecutionRequest {
            scope: ResourceScope::system(),
            mounts: None,
            // One stream: git's messages stay next to its output.
            command: format!("({}) 2>&1", command.line),
            workdir: Some(command.workdir.to_string_lossy().into_owned()),
            timeout_secs: Some(command.timeout.as_secs().max(1)),
            extra_env: env,
        };
        let output = self.port.run_command(request).await.map_err(|error| {
            OrchestratorError::request("command_failed", format!("could not run: {error}"))
        })?;
        Ok(HostCommandOutput {
            exit_code: output.exit_code,
            output: output.output,
        })
    }
}

/// Turn on worktree isolation for `engine`: worktrees under
/// `$HOME/.ironclaw/worktrees`, project setup hooks approved through
/// `approver` (the CLI bridge's gate). Without `HOME` the engine refuses
/// `isolation='worktree'` model-visibly.
pub(crate) fn wire_worktrees<A: HookApprover + 'static>(engine: &Engine, approver: Arc<A>) {
    let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
        return; // silent-ok: no home folder means no worktree root; delegation reports it
    };
    let home = PathBuf::from(home);
    engine.set_worktree_host(WorktreeHost {
        runner: Arc::new(HostProcessCommands::new(home.clone())),
        root: home.join(".ironclaw").join("worktrees"),
        approver,
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn sh(dir: &std::path::Path, line: &str) {
        let ok = std::process::Command::new("sh")
            .args(["-c", line])
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{line}");
    }

    /// Git through the host process port: its output (a commit id the
    /// output scanner must not redact), stderr folded in, exit codes kept.
    #[tokio::test]
    async fn git_runs_through_the_host_process_port() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        sh(repo, "git init -q -b main && echo a > a && git add a");
        sh(
            repo,
            "git -c user.name=t -c user.email=t@example.com commit -q -m one",
        );
        let runner = HostProcessCommands::new(repo.to_path_buf());
        let run = |line: &str| HostCommand {
            line: line.to_string(),
            workdir: repo.to_path_buf(),
            timeout: Duration::from_secs(30),
        };
        let head = runner.run(run("git rev-parse HEAD")).await.unwrap();
        assert!(head.success());
        let head = head.output.trim();
        assert_eq!(head.len(), 40, "{head}");
        assert!(head.chars().all(|c| c.is_ascii_hexdigit()));
        let missing = runner
            .run(run("git rev-parse --verify no-such-branch"))
            .await
            .unwrap();
        assert_ne!(missing.exit_code, 0);
        assert!(missing.output.contains("fatal"), "{}", missing.output);
    }
}
