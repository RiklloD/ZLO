//! A project's worktree setup hooks (`.zlo/worktree.toml`). They are
//! repository content, so they are untrusted: read once from the source
//! checkout (never from the child-editable worktree), snapshotted on the
//! binding, and run only after the person approved that exact content
//! through the approval gate ([`super::HookApprover`]). An approval is
//! remembered per repository identity (not path) and content hash; changed
//! content, or another repository at the same path, asks again. A script
//! too long for the gate to show in full is never offered.

use std::path::Path;

use ironclaw_host_api::ids::ThreadId;
use sha2::Digest;

use super::git::Git;
use super::{
    HOOK_SCRIPT_MAX_CHARS, HookApproval, HookDecision, HostCommand, WorktreeBinding,
    WorktreeConfig, WorktreeHooks, tail,
};
use crate::engine::{Engine, Result};
use crate::model::now_ms;

/// A hook command's output kept in the setup log.
const OUTPUT_MAX: usize = 3_000;

impl WorktreeHooks {
    fn from_config(config: &WorktreeConfig, repo_root: &Path, repo_id: &str) -> Self {
        let timeout_s = config.timeout.as_secs();
        let canonical = serde_json::json!({
            "repo": repo_root.to_string_lossy(),
            "repoId": repo_id,
            "onCreate": config.on_create,
            "onSettle": config.on_settle,
            "timeoutS": timeout_s,
        });
        let hash = hex::encode(sha2::Sha256::digest(canonical.to_string().as_bytes()));
        Self {
            on_create: config.on_create.clone(),
            on_settle: config.on_settle.clone(),
            timeout_s,
            hash,
            repo_id: repo_id.to_string(),
        }
    }

    /// What the approval gate shows: every command of both hooks.
    fn script(&self, repo_root: &Path) -> String {
        let mut out = format!(
            "Worktree setup from {}/{} (runs in the new worktree, timeout {}s each; \
             content sha256 {}):",
            repo_root.display(),
            super::config::CONFIG_FILE,
            self.timeout_s,
            self.hash
        );
        for (hook, lines) in [
            ("on_create", &self.on_create),
            ("on_settle", &self.on_settle),
        ] {
            if !lines.is_empty() {
                out.push_str(&format!("\n{hook}:"));
                for line in lines {
                    out.push_str(&format!("\n$ {line}"));
                }
            }
        }
        out
    }
}

/// The source checkout's hooks; `Ok(None)` without any commands.
pub(super) async fn read_hooks(repo_root: &Path, repo_id: &str) -> Result<Option<WorktreeHooks>> {
    let config = WorktreeConfig::load(&[repo_root]).await?;
    if config.on_create.is_empty() && config.on_settle.is_empty() {
        return Ok(None);
    }
    Ok(Some(WorktreeHooks::from_config(
        &config, repo_root, repo_id,
    )))
}

impl Engine {
    /// Whether `hooks` may run for `binding`: approved before (same repo,
    /// same content), or approved now by the person behind `ask`. A
    /// refusal is logged on the binding and returned as its reason.
    pub(super) async fn approve_hooks(
        &self,
        binding: &WorktreeBinding,
        hooks: &WorktreeHooks,
        ask: &ThreadId,
    ) -> Result<bool> {
        if self.hooks_still_trusted(binding, hooks).await? {
            return Ok(true);
        }
        let request = HookApproval {
            thread: ask.clone(),
            repo_root: binding.repo_root.clone(),
            script: hooks.script(&binding.repo_root),
            content_hash: hooks.hash.clone(),
        };
        // The person approves what they can read in full, or nothing.
        let length = request.script.chars().count();
        if length > HOOK_SCRIPT_MAX_CHARS {
            self.store
                .append_worktree_log(
                    &binding.thread_id,
                    &format!(
                        "[setup] not run: the hooks are too long to show in full for approval \
                         ({length} characters, at most {HOOK_SCRIPT_MAX_CHARS}). Shorten \
                         {} (move long steps into a script file in the repository).",
                        super::config::CONFIG_FILE
                    ),
                    now_ms(),
                )
                .await?;
            return Ok(false);
        }
        let why = match self.worktree_host()?.approver.approve(&request).await {
            HookDecision::Approved => {
                self.store
                    .trust_hooks(&hooks.repo_id, &binding.repo_root, &hooks.hash, now_ms())
                    .await?;
                return Ok(true);
            }
            HookDecision::Denied(reason) => format!("declined: {reason}"),
            HookDecision::Unavailable => {
                "nobody could approve them (no live turn to ask in)".to_string()
            }
        };
        self.store
            .append_worktree_log(
                &binding.thread_id,
                &format!(
                    "[setup] not run, {why}. They run once the person approves this exact content.\n{}",
                    request.script
                ),
                now_ms(),
            )
            .await?;
        Ok(false)
    }

    /// The snapshot was approved, and the repository is still the one it
    /// was approved in (not another repository moved to the same path).
    pub(super) async fn hooks_still_trusted(
        &self,
        binding: &WorktreeBinding,
        hooks: &WorktreeHooks,
    ) -> Result<bool> {
        if hooks.repo_id.is_empty()
            || !self
                .store
                .hooks_trusted(&hooks.repo_id, &hooks.hash)
                .await?
        {
            return Ok(false);
        }
        let git = Git::new(self.worktree_host()?.runner.as_ref());
        Ok(git.repo_identity(&binding.repo_root).await? == hooks.repo_id)
    }

    /// Run one approved hook's commands in the worktree, logging each;
    /// stops at the first failure. Returns false when one failed.
    pub(super) async fn run_hook(
        &self,
        binding: &WorktreeBinding,
        hook: &str,
        hooks: &WorktreeHooks,
    ) -> Result<bool> {
        let commands = match hook {
            "on_create" => &hooks.on_create,
            _ => &hooks.on_settle,
        };
        let host = self.worktree_host()?;
        for line in commands {
            let outcome = host
                .runner
                .run(HostCommand {
                    line: line.clone(),
                    workdir: binding.path.clone(),
                    timeout: std::time::Duration::from_secs(hooks.timeout_s),
                })
                .await;
            let (ok, text) = match outcome {
                Ok(out) => (
                    out.success(),
                    format!(
                        "[{hook}] $ {line}\n{}\n[exit {}]",
                        tail(out.output.trim_end(), OUTPUT_MAX),
                        out.exit_code
                    ),
                ),
                Err(error) => (
                    false,
                    format!("[{hook}] $ {line}\n[could not run: {error}]"),
                ),
            };
            self.store
                .append_worktree_log(&binding.thread_id, &text, now_ms())
                .await?;
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
