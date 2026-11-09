//! Worktree isolation's command runner: the orchestrator's git and
//! `.zlo/worktree.toml` setup commands run through the host process port
//! (`ironclaw_host_runtime::HostProcessPort`), with its scrubbed environment
//! plus `HOME` (git identity and config, package caches) and no terminal
//! prompts. Wired only next to the orchestrator, which exists only on the
//! local libSQL deployment whose CLI runtimes already run on this host.
//! The same runner streams output (`spawn`) for the thread repository views
//! remote clients read, and — over the runtime policy's own host process
//! port, only when the policy grants the agent one — for the operator
//! console (`wire_console`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::process::CommandExecutionRequest;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_host_runtime::{HostProcessEvent, HostProcessStream};
use ironclaw_host_runtime::{HostProcessPort, RuntimeProcessPort as _};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::worktree::{
    HookApprover, HostCommand, HostCommandEvent, HostCommandOutput, HostCommandProcess,
    HostCommandRunner, WorktreeHost,
};

pub(crate) struct HostProcessCommands {
    port: HostProcessPort,
    home: PathBuf,
}

impl HostProcessCommands {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self::with_port(HostProcessPort::new(), home)
    }

    /// Over `port` (the runtime policy's own host process port).
    pub(crate) fn with_port(port: HostProcessPort, home: PathBuf) -> Self {
        Self { port, home }
    }

    fn env(&self) -> HashMap<String, String> {
        HashMap::from([
            ("HOME".to_string(), self.home.to_string_lossy().into_owned()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ])
    }
}

/// A streamed command on the host process port.
struct StreamedCommand(HostProcessStream);

#[async_trait]
impl HostCommandProcess for StreamedCommand {
    async fn next(&mut self) -> Option<HostCommandEvent> {
        Some(match self.0.next().await? {
            HostProcessEvent::Stdout(bytes) => HostCommandEvent::Stdout(bytes),
            HostProcessEvent::Stderr(bytes) => HostCommandEvent::Stderr(bytes),
            HostProcessEvent::Exited { code, timed_out } => {
                HostCommandEvent::Exited { code, timed_out }
            }
        })
    }

    fn kill(&mut self) {
        self.0.kill();
    }
}

#[async_trait]
impl HostCommandRunner for HostProcessCommands {
    async fn spawn(
        &self,
        command: HostCommand,
    ) -> Result<Box<dyn HostCommandProcess>, OrchestratorError> {
        let request = CommandExecutionRequest {
            scope: ResourceScope::system(),
            mounts: None,
            command: command.line,
            workdir: Some(command.workdir.to_string_lossy().into_owned()),
            timeout_secs: Some(command.timeout.as_secs().max(1)),
            extra_env: self.env(),
        };
        let stream = self.port.spawn_streaming(request).await.map_err(|error| {
            OrchestratorError::request("command_failed", format!("could not run: {error}"))
        })?;
        Ok(Box::new(StreamedCommand(stream)))
    }

    async fn run(&self, command: HostCommand) -> Result<HostCommandOutput, OrchestratorError> {
        let env = self.env();
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

/// Turn on the operator console for `engine` over `port`, the host process
/// port the runtime policy gives the agent's own shell — so the console
/// exists exactly where a host shell already does (single-user local).
/// Without `HOME` there is none.
pub(crate) fn wire_console(engine: &Engine, port: HostProcessPort) {
    let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
        return; // silent-ok: no home folder; the console then reports unavailable
    };
    engine.set_console_runner(Arc::new(HostProcessCommands::with_port(
        port,
        PathBuf::from(home),
    )));
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

        // Streamed: stdout and stderr apart, unshortened, exit code kept.
        let mut process = runner
            .spawn(run("seq 1 20000; echo done >&2; exit 2"))
            .await
            .unwrap();
        let (mut out, mut err, mut exit) = (Vec::new(), Vec::new(), None);
        while let Some(event) = process.next().await {
            match event {
                HostCommandEvent::Stdout(b) => out.extend(b),
                HostCommandEvent::Stderr(b) => err.extend(b),
                HostCommandEvent::Exited { code, .. } => exit = Some(code),
            }
        }
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.lines().count(), 20000, "no preview truncation");
        assert_eq!(String::from_utf8(err).unwrap(), "done\n");
        assert_eq!(exit, Some(Some(2)));
    }
}
