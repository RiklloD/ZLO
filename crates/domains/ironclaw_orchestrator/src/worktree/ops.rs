//! Engine operations over worktrees: create from a thread's folder, settle
//! when a child's task ends, and the parent's status / merge / remove.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use super::git::Git;
use super::{
    CommitId, HostCommand, MergeStrategy, OnConflict, WorktreeBinding, WorktreeBranch,
    WorktreeConfig, WorktreeHost, WorktreeState, repo_hash, slug, tail,
};
use crate::engine::{Engine, Result};
use crate::error::OrchestratorError;
use crate::model::{TaskId, ThreadRecord, now_ms};
use crate::runtime::TurnRuntime;

/// A checked source for a new worktree.
#[derive(Debug, Clone)]
pub(crate) struct WorktreePlan {
    source_dir: PathBuf,
    repo_root: PathBuf,
    base: CommitId,
}

/// Dirty files listed in a view.
const DIRTY_LIST_MAX: usize = 50;

fn unavailable() -> OrchestratorError {
    OrchestratorError::request(
        "worktree_unavailable",
        "Worktree isolation is not available in this backend. Use isolation='shared'.",
    )
}

impl Engine {
    /// Turn on worktree isolation (first call wins).
    pub fn set_worktree_host(&self, host: WorktreeHost) {
        let _ = self.worktree_host.set(host); // silent-ok: already set by an earlier wiring
    }

    fn worktree_host(&self) -> Result<&WorktreeHost> {
        self.worktree_host.get().ok_or_else(unavailable)
    }

    /// The folder a thread's agent works in: its worktree, else its
    /// profile's folder, else its runtime's default folder.
    pub(crate) async fn thread_cwd(&self, thread: &ThreadRecord) -> Result<Option<PathBuf>> {
        if let Some(path) = self.thread_work_dir(&thread.thread_id).await? {
            return Ok(Some(path));
        }
        if let Some(folder) = self
            .turn_profile(&thread.thread_id)
            .await
            .and_then(|p| p.folder)
        {
            return Ok(Some(folder));
        }
        Ok(self.runtime(&thread.runtime_id)?.default_work_dir())
    }

    /// The worktree folder a thread's turns run in, while it exists.
    pub(crate) async fn thread_work_dir(&self, thread: &ThreadId) -> Result<Option<PathBuf>> {
        Ok(self
            .store
            .worktree(thread)
            .await?
            .filter(|b| b.state.has_folder())
            .map(|b| b.path))
    }

    /// Check that a worktree can be made for a thread on `runtime` from
    /// `source` (default: `from`'s working folder). Runs before any task
    /// or thread exists, so a refusal starts nothing.
    pub(crate) async fn worktree_preflight(
        &self,
        from: &ThreadRecord,
        source: Option<PathBuf>,
        runtime: &Arc<dyn TurnRuntime>,
    ) -> Result<WorktreePlan> {
        let host = self.worktree_host()?;
        if !runtime.supports_work_dir() {
            return Err(OrchestratorError::request(
                "worktree_unavailable",
                format!(
                    "Runtime {} cannot run a thread in its own worktree. Pick a target that can \
                     (t3_capabilities) or use isolation='shared'.",
                    runtime.id()
                ),
            ));
        }
        let source_dir = match source {
            Some(dir) => dir,
            None => self.thread_cwd(from).await?.ok_or_else(|| {
                OrchestratorError::request(
                    "not_a_git_repository",
                    "This thread has no working folder, so a worktree cannot be made from it. \
                     Use isolation='shared'.",
                )
            })?,
        };
        let git = Git::new(host.runner.as_ref());
        let repo_root = git.toplevel(&source_dir).await?;
        let base = git.head(&source_dir).await?;
        Ok(WorktreePlan {
            source_dir,
            repo_root,
            base,
        })
    }

    /// Make the worktree, bind it to `thread` and run the project's
    /// `on_create` commands in it.
    pub(crate) async fn create_worktree(
        &self,
        thread: &ThreadId,
        plan: &WorktreePlan,
        label: &str,
        parent: Option<ThreadId>,
        task: Option<TaskId>,
    ) -> Result<WorktreeBinding> {
        let host = self.worktree_host()?;
        let git = Git::new(host.runner.as_ref());
        let short: String = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect();
        let name = format!("{}-{short}", slug(label));
        let branch = WorktreeBranch::new(format!("zlo/{name}"))?;
        let path = host.root.join(repo_hash(&plan.repo_root)).join(&name);
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| {
                OrchestratorError::request(
                    "worktree_unavailable",
                    format!("could not create the worktrees folder: {e}"),
                )
            })?;
        }
        git.worktree_add(&plan.repo_root, &branch, &path, &plan.base)
            .await?;
        let now = now_ms();
        let binding = WorktreeBinding {
            thread_id: thread.clone(),
            path,
            branch,
            base_commit: plan.base.clone(),
            repo_root: plan.repo_root.clone(),
            source_dir: plan.source_dir.clone(),
            parent_thread: parent,
            task_id: task,
            state: WorktreeState::Active,
            archive_ref: None,
            setup_log: None,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_worktree(&binding).await?;
        let config = WorktreeConfig::load(&[&plan.repo_root, &binding.path]).await;
        match config {
            Ok(config) => {
                self.run_hook(&binding, "on_create", &config.on_create, &config)
                    .await?;
            }
            Err(error) => {
                self.store
                    .append_worktree_log(thread, &format!("setup skipped: {error}"), now_ms())
                    .await?;
            }
        }
        self.require_worktree(thread).await
    }

    /// Run one hook's commands in the worktree, logging each; stops at the
    /// first failure. Returns false when one failed.
    async fn run_hook(
        &self,
        binding: &WorktreeBinding,
        hook: &str,
        commands: &[String],
        config: &WorktreeConfig,
    ) -> Result<bool> {
        let host = self.worktree_host()?;
        for line in commands {
            let outcome = host
                .runner
                .run(HostCommand {
                    line: line.clone(),
                    workdir: binding.path.clone(),
                    timeout: config.timeout,
                })
                .await;
            let (ok, text) = match outcome {
                Ok(out) => (
                    out.success(),
                    format!(
                        "[{hook}] $ {line}\n{}\n[exit {}]",
                        tail(out.output.trim_end(), 3_000),
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

    pub(crate) async fn require_worktree(&self, thread: &ThreadId) -> Result<WorktreeBinding> {
        self.store.worktree(thread).await?.ok_or_else(|| {
            OrchestratorError::request("no_worktree", format!("Thread {thread} has no worktree."))
        })
    }

    /// What a child is told about its worktree, ahead of its brief.
    pub(crate) fn worktree_brief_note(binding: &WorktreeBinding) -> String {
        let setup = match &binding.setup_log {
            Some(log) => format!("\nProject setup ran in it:\n{}\n", tail(log, 2_000)),
            None => String::new(),
        };
        format!(
            "You work in your own git worktree: {path} on branch {branch}, made from commit {base} \
             of {repo}. It holds only committed work: the parent's uncommitted edits are not in it. \
             Other agents may be editing the parent's checkout at the same time, so stay inside \
             this folder. Commit your work on this branch when you are done (uncommitted changes \
             are kept too); the parent reviews and merges it.{setup}",
            path = binding.path.display(),
            branch = binding.branch,
            base = binding.base_commit.short(),
            repo = binding.repo_root.display(),
        )
    }

    /// A finished child's worktree: run `on_settle`, then remove it with its
    /// branch when it holds no work, else keep it for the parent. Returns a
    /// line for the parent's result; `None` without a worktree. Never fails
    /// the task: a settle error keeps the worktree and says so.
    pub(crate) async fn settle_worktree(&self, thread: &ThreadId) -> Option<String> {
        match self.try_settle_worktree(thread).await {
            Ok(note) => note,
            Err(error) => {
                tracing::debug!(%error, %thread, "worktree settle failed; kept");
                Some(format!(
                    "Worktree kept (settle failed: {error}). t3_thread action=worktree_status thread={thread}."
                ))
            }
        }
    }

    async fn try_settle_worktree(&self, thread: &ThreadId) -> Result<Option<String>> {
        let Some(binding) = self.store.worktree(thread).await? else {
            return Ok(None);
        };
        if binding.state != WorktreeState::Active {
            return Ok(None);
        }
        // A child still running (a cancel not yet enforced) keeps its tree.
        if self
            .store
            .thread(thread)
            .await?
            .is_some_and(|t| t.is_running())
        {
            return Ok(None);
        }
        let host = self.worktree_host()?;
        let git = Git::new(host.runner.as_ref());
        if let Ok(config) = WorktreeConfig::load(&[&binding.path, &binding.repo_root]).await {
            self.run_hook(&binding, "on_settle", &config.on_settle, &config)
                .await?;
        }
        let dirty = git.dirty_files(&binding.path).await?;
        let ahead = git
            .count(&binding.path, binding.base_commit.as_str(), "HEAD")
            .await?;
        let now = now_ms();
        if dirty.is_empty() && ahead == 0 {
            git.worktree_remove(&binding.repo_root, &binding.path)
                .await?;
            git.branch_delete(&binding.repo_root, &binding.branch)
                .await?;
            self.store
                .set_worktree_state(
                    thread,
                    &[WorktreeState::Active],
                    WorktreeState::Removed,
                    None,
                    now,
                )
                .await?;
            return Ok(Some(format!(
                "Worktree: no changes, so its folder and branch {} were removed.",
                binding.branch
            )));
        }
        self.store
            .set_worktree_state(
                thread,
                &[WorktreeState::Active],
                WorktreeState::Kept,
                None,
                now,
            )
            .await?;
        Ok(Some(format!(
            "Worktree kept: branch {branch} at {path} ({ahead} commit(s) since {base}, {dirty} \
             uncommitted file(s)). Review it, then t3_thread action=merge_worktree thread={thread} \
             to bring it into {source}, or action=remove_worktree.",
            branch = binding.branch,
            path = binding.path.display(),
            base = binding.base_commit.short(),
            dirty = dirty.len(),
            source = binding.source_dir.display(),
        )))
    }

    /// `t3_thread action=worktree_status`: the binding plus live git state.
    pub async fn worktree_status(&self, thread: &ThreadRecord) -> Result<Value> {
        let binding = self.require_worktree(&thread.thread_id).await?;
        self.worktree_view(&binding).await
    }

    /// The binding as a document, with branch, ahead/behind and dirty files
    /// read live when the folder still exists.
    pub async fn worktree_view(&self, b: &WorktreeBinding) -> Result<Value> {
        let mut view = json!({
            "threadId": b.thread_id.as_str(),
            "path": b.path, "branch": b.branch.as_str(), "baseCommit": b.base_commit.as_str(),
            "repoRoot": b.repo_root, "sourceDir": b.source_dir,
            "parentThreadId": b.parent_thread.as_ref().map(|t| t.as_str()),
            "taskId": b.task_id.as_ref().map(|t| t.as_str()),
            "state": b.state.as_str(), "archiveRef": b.archive_ref, "setupLog": b.setup_log,
            "createdAt": b.created_at, "updatedAt": b.updated_at,
        });
        let Ok(host) = self.worktree_host() else {
            return Ok(view);
        };
        let git = Git::new(host.runner.as_ref());
        let exists = b.state.has_folder() && folder_exists(&b.path).await;
        view["exists"] = json!(exists);
        if exists {
            let dirty = git.dirty_files(&b.path).await?;
            view["dirtyCount"] = json!(dirty.len());
            view["dirtyFiles"] = json!(dirty.iter().take(DIRTY_LIST_MAX).collect::<Vec<_>>());
            view["head"] = json!(git.rev(&b.path, "HEAD").await?.as_str());
        }
        if git.branch_exists(&b.repo_root, &b.branch).await? {
            let branch = b.branch.as_str();
            view["aheadOfBase"] = json!(
                git.count(&b.repo_root, b.base_commit.as_str(), branch)
                    .await?
            );
            if let Ok(source) = git.rev(&b.source_dir, "HEAD").await {
                view["sourceHead"] = json!(source.as_str());
                view["ahead"] = json!(git.count(&b.repo_root, source.as_str(), branch).await?);
                view["behind"] = json!(git.count(&b.repo_root, branch, source.as_str()).await?);
            }
        }
        Ok(view)
    }

    async fn ensure_idle(&self, thread: &ThreadRecord) -> Result<()> {
        if self
            .store
            .thread(&thread.thread_id)
            .await?
            .is_some_and(|t| t.is_running())
        {
            return Err(OrchestratorError::request(
                "thread_busy",
                "The worktree's thread is still running; wait for it to finish (or cancel it).",
            ));
        }
        Ok(())
    }

    /// `t3_thread action=merge_worktree`: bring the worktree's branch into
    /// the checkout it was made from. Uncommitted worktree changes are
    /// committed on the branch first. Conflicts come back as a
    /// model-visible `merge_conflict`; nothing is forced.
    pub async fn merge_worktree(
        &self,
        thread: &ThreadRecord,
        strategy: MergeStrategy,
        on_conflict: OnConflict,
    ) -> Result<Value> {
        let binding = self.require_worktree(&thread.thread_id).await?;
        self.ensure_idle(thread).await?;
        let host = self.worktree_host()?;
        let git = Git::new(host.runner.as_ref());
        if !git
            .branch_exists(&binding.repo_root, &binding.branch)
            .await?
        {
            return Err(OrchestratorError::request(
                "no_worktree",
                format!("Branch {} no longer exists.", binding.branch),
            ));
        }
        let committed = if binding.state.has_folder() && folder_exists(&binding.path).await {
            let label = thread.title.as_deref().unwrap_or("worktree");
            git.commit_all(
                &binding.path,
                &format!(
                    "zlo: uncommitted work from {}",
                    label.trim_start_matches("↳ ")
                ),
            )
            .await?
        } else {
            None
        };
        let target = git.toplevel(&binding.source_dir).await?;
        if target == binding.path {
            return Err(OrchestratorError::invalid(
                "the worktree cannot be merged into itself",
            ));
        }
        let branch = binding.branch.as_str();
        let before = git.head(&target).await?;
        let pending = git.count(&target, before.as_str(), branch).await?;
        if pending == 0 {
            return Ok(json!({
                "ok": true, "merged": false, "message": "Already up to date: the checkout has every commit of the branch.",
                "branch": branch, "target": target, "head": before.as_str(),
            }));
        }
        let message = format!("Merge {branch} (ZLO worktree)");
        let (out, abort): (_, &[&str]) = match strategy {
            MergeStrategy::Merge => (
                git.integrate(
                    &target,
                    &["merge", "--no-ff", "--no-edit", "-m", &message, branch],
                )
                .await?,
                &["merge", "--abort"],
            ),
            MergeStrategy::Squash => (
                git.integrate(&target, &["merge", "--squash", branch])
                    .await?,
                &["reset", "--merge"],
            ),
            MergeStrategy::CherryPick => {
                let base = git.merge_base(&target, before.as_str(), branch).await?;
                let range = format!("{}..{branch}", base.as_str());
                (
                    git.integrate(&target, &["cherry-pick", &range]).await?,
                    &["cherry-pick", "--abort"],
                )
            }
        };
        if !out.success() {
            let conflicts = git.conflicts(&target).await?;
            if conflicts.is_empty() {
                return Err(OrchestratorError::request(
                    "merge_refused",
                    format!(
                        "git refused the merge into {} (nothing was changed): {}",
                        target.display(),
                        tail(out.output.trim(), 1_500)
                    ),
                ));
            }
            let left = on_conflict == OnConflict::Leave;
            if !left {
                git.run(&target, abort).await?;
            }
            return Err(OrchestratorError::request(
                "merge_conflict",
                format!(
                    "Merging {branch} into {} stopped on conflicts in: {}. {}",
                    target.display(),
                    conflicts.join(", "),
                    if left {
                        "The conflicted merge is left in the checkout to resolve there."
                    } else {
                        "The merge was aborted; the checkout is as it was. Resolve in the worktree \
                         (merge the checkout's branch into it) and merge again, or retry with \
                         on_conflict='leave'."
                    }
                ),
            ));
        }
        if strategy == MergeStrategy::Squash {
            let label = thread.title.as_deref().unwrap_or(branch);
            let squash_message = format!(
                "{} (squashed from {branch})",
                label.trim_start_matches("↳ ")
            );
            let commit = git
                .integrate(
                    &target,
                    &["commit", "--no-verify", "-q", "-m", &squash_message],
                )
                .await?;
            if !commit.success() {
                return Err(OrchestratorError::request(
                    "merge_refused",
                    format!(
                        "the squashed changes are staged in {} but could not be committed: {}",
                        target.display(),
                        tail(commit.output.trim(), 1_500)
                    ),
                ));
            }
        }
        let after = git.head(&target).await?;
        self.store
            .set_worktree_state(
                &thread.thread_id,
                &[WorktreeState::Active, WorktreeState::Kept],
                WorktreeState::Merged,
                None,
                now_ms(),
            )
            .await?;
        Ok(json!({
            "ok": true, "merged": true, "branch": branch, "target": target,
            "strategy": match strategy { MergeStrategy::Merge => "merge", MergeStrategy::Squash => "squash", MergeStrategy::CherryPick => "cherry_pick" },
            "commits": pending, "committedUncommitted": committed.map(|c| c.as_str().to_string()),
            "headBefore": before.as_str(), "head": after.as_str(),
            "message": format!("Merged {pending} commit(s) from {branch} into {}. remove_worktree cleans up the folder.", target.display()),
        }))
    }

    /// `t3_thread action=remove_worktree`: remove the folder. Uncommitted
    /// changes are committed on the branch first; a branch with commits the
    /// checkout lacks is kept and archived under `refs/zlo/archive/`.
    pub async fn remove_worktree(&self, thread: &ThreadRecord) -> Result<Value> {
        let binding = self.require_worktree(&thread.thread_id).await?;
        if binding.state == WorktreeState::Removed {
            return self.worktree_view(&binding).await;
        }
        self.ensure_idle(thread).await?;
        let host = self.worktree_host()?;
        let git = Git::new(host.runner.as_ref());
        let exists = folder_exists(&binding.path).await;
        if exists {
            git.commit_all(
                &binding.path,
                "zlo: uncommitted work kept at worktree removal",
            )
            .await?;
        }
        let mut archive = None;
        let mut branch_kept = false;
        if git
            .branch_exists(&binding.repo_root, &binding.branch)
            .await?
        {
            let branch = binding.branch.as_str();
            let tip = git.rev(&binding.repo_root, branch).await?;
            let own = git
                .count(&binding.repo_root, binding.base_commit.as_str(), branch)
                .await?;
            let merged = match git.rev(&binding.source_dir, "HEAD").await {
                Ok(source) => {
                    git.is_ancestor(&binding.repo_root, branch, source.as_str())
                        .await?
                }
                Err(_) => false,
            };
            if own > 0 && !merged {
                let name = binding.branch.archive_ref();
                git.update_ref(&binding.repo_root, &name, &tip).await?;
                archive = Some(name);
                branch_kept = true;
            }
        }
        if exists {
            git.worktree_remove(&binding.repo_root, &binding.path)
                .await?;
        } else {
            git.worktree_prune(&binding.repo_root).await?;
        }
        if !branch_kept
            && git
                .branch_exists(&binding.repo_root, &binding.branch)
                .await?
        {
            git.branch_delete(&binding.repo_root, &binding.branch)
                .await?;
        }
        self.store
            .set_worktree_state(
                &thread.thread_id,
                &[
                    WorktreeState::Active,
                    WorktreeState::Kept,
                    WorktreeState::Merged,
                ],
                WorktreeState::Removed,
                archive.as_deref(),
                now_ms(),
            )
            .await?;
        let current = self.require_worktree(&thread.thread_id).await?;
        let mut view = self.worktree_view(&current).await?;
        view["branchKept"] = json!(branch_kept);
        view["message"] = json!(if branch_kept {
            format!(
                "Removed the folder. Its unmerged commits stay on branch {} (also {}).",
                binding.branch,
                archive.as_deref().unwrap_or("")
            )
        } else {
            "Removed the folder and its branch: every commit was already in the checkout."
                .to_string()
        });
        Ok(view)
    }

    /// Start a user thread in a new worktree made from `source` (default:
    /// the thread's working folder). The thread must not have one yet.
    pub async fn start_worktree(
        &self,
        thread: &ThreadRecord,
        source: Option<PathBuf>,
    ) -> Result<Value> {
        if self.thread_work_dir(&thread.thread_id).await?.is_some() {
            return Err(OrchestratorError::request(
                "worktree_exists",
                "This thread already works in a worktree.",
            ));
        }
        self.ensure_idle(thread).await?;
        self.worktree_host()?;
        let runtime = self.runtime(&thread.runtime_id)?;
        let plan = self.worktree_preflight(thread, source, &runtime).await?;
        let label = thread.title.as_deref().unwrap_or("thread");
        let binding = self
            .create_worktree(&thread.thread_id, &plan, label, None, None)
            .await?;
        self.worktree_view(&binding).await
    }
}

async fn folder_exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false) // silent-ok: an unreadable folder is treated as gone; git reports the rest
}
