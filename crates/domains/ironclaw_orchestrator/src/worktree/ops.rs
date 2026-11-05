//! Engine operations over worktrees: create from a thread's folder, settle
//! when a child's task ends, status, and a user thread's own worktree.
//! Merge and removal live in `integrate.rs`, folder resolution and the
//! lifecycle exclusion in `lifecycle.rs`, setup hooks in `hooks.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use super::git::Git;
use super::hooks::read_hooks;
use super::{
    CommitId, WorktreeBinding, WorktreeBranch, WorktreeHost, WorktreeState, repo_hash, slug, tail,
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

/// A runtime that cannot run turns in a thread's worktree.
pub(crate) fn runtime_refused(runtime: &dyn TurnRuntime) -> OrchestratorError {
    OrchestratorError::request(
        "worktree_unavailable",
        format!(
            "Runtime {} cannot run a thread in a worktree, so this thread (which works in one) \
             cannot use it. Pick a target that can (t3_capabilities).",
            runtime.id()
        ),
    )
}

impl Engine {
    /// Turn on worktree isolation (first call wins).
    pub fn set_worktree_host(&self, host: WorktreeHost) {
        let _ = self.worktree_host.set(host); // silent-ok: already set by an earlier wiring
    }

    pub(super) fn worktree_host(&self) -> Result<&WorktreeHost> {
        self.worktree_host.get().ok_or_else(unavailable)
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
            return Err(runtime_refused(runtime.as_ref()));
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

    /// Make the worktree and bind it to `thread`. The source checkout's
    /// setup hooks are snapshotted and, once the person behind `parent`
    /// (else `thread`) approves that exact content, `on_create` runs.
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
        // Read from the source checkout before the child can touch anything.
        let (hooks, unreadable) = match read_hooks(&plan.repo_root).await {
            Ok(hooks) => (hooks, None),
            Err(error) => (None, Some(error)),
        };
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
            parent_thread: parent.clone(),
            task_id: task,
            state: WorktreeState::Active,
            archive_ref: None,
            setup_log: None,
            hooks,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_worktree(&binding).await?;
        if let Some(error) = unreadable {
            self.store
                .append_worktree_log(thread, &format!("setup skipped: {error}"), now_ms())
                .await?;
        }
        if let Some(hooks) = &binding.hooks {
            let ask = parent.unwrap_or_else(|| thread.clone());
            if self.approve_hooks(&binding, hooks, &ask).await? && !hooks.on_create.is_empty() {
                self.run_hook(&binding, "on_create", hooks).await?;
            }
        }
        self.require_worktree(thread).await
    }

    pub(crate) async fn require_worktree(&self, thread: &ThreadId) -> Result<WorktreeBinding> {
        self.store.worktree(thread).await?.ok_or_else(|| {
            OrchestratorError::request("no_worktree", format!("Thread {thread} has no worktree."))
        })
    }

    /// What a child is told about its worktree, ahead of its brief.
    pub(crate) fn worktree_brief_note(binding: &WorktreeBinding) -> String {
        let setup = match &binding.setup_log {
            Some(log) => format!("\nProject setup:\n{}\n", tail(log, 2_000)),
            None => String::new(),
        };
        format!(
            "You work in your own git worktree: {path} on branch {branch}, made from commit {base} \
             of {repo}. It holds only committed work: the parent's uncommitted edits are not in it. \
             Other agents may be editing the parent's checkout at the same time, so stay inside \
             this folder and on this branch. Commit the work you want merged on this branch before \
             you finish: nothing is committed for you, and the parent merges only the branch.{setup}",
            path = binding.path.display(),
            branch = binding.branch,
            base = binding.base_commit.short(),
            repo = binding.repo_root.display(),
        )
    }

    /// A finished child's worktree: run its approved `on_settle`, then
    /// remove it with its branch when it holds no work and HEAD is still on
    /// that branch, else keep it for the parent. Returns a line for the
    /// parent's result; `None` without an active worktree. Never fails the
    /// task: a settle error keeps the worktree and says so.
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
        // A thread still running in it (a cancel not yet enforced, a shared
        // grandchild) keeps the tree; so does another step holding it.
        let _op = match self.reserve_worktree(thread).await {
            Ok(op) => op,
            Err(error) if error.code() == "thread_busy" => return Ok(None),
            Err(error) => return Err(error),
        };
        let host = self.worktree_host()?;
        let git = Git::new(host.runner.as_ref());
        if let Some(hooks) = &binding.hooks
            && !hooks.on_settle.is_empty()
        {
            if self
                .store
                .hooks_trusted(&binding.repo_root, &hooks.hash)
                .await?
            {
                self.run_hook(&binding, "on_settle", hooks).await?;
            } else {
                self.store
                    .append_worktree_log(
                        thread,
                        "[on_settle] not run: the setup hooks were not approved.",
                        now_ms(),
                    )
                    .await?;
            }
        }
        let dirty = git.dirty_files(&binding.path).await?;
        let on_branch = on_branch(&git, &binding).await?;
        let ahead = git
            .count(
                &binding.repo_root,
                binding.base_commit.as_str(),
                binding.branch.as_str(),
            )
            .await?;
        let now = now_ms();
        if dirty.is_empty() && on_branch && ahead == 0 {
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
        let moved = if on_branch {
            String::new()
        } else {
            " Its HEAD is no longer on that branch: check worktree_status before merging.".into()
        };
        Ok(Some(format!(
            "Worktree kept: branch {branch} at {path} ({ahead} commit(s) since {base}, {dirty} \
             uncommitted file(s)).{moved} Review it, then t3_thread action=merge_worktree \
             thread={thread} to bring the branch into {source}, or action=remove_worktree.",
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
            view["headRef"] = json!(git.symbolic_head(&b.path).await?);
            view["onBranch"] = json!(on_branch(&git, b).await?);
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

    /// Start a user thread in a new worktree made from `source` (default:
    /// the thread's working folder). The thread must not have one yet.
    pub async fn start_worktree(
        &self,
        thread: &ThreadRecord,
        source: Option<PathBuf>,
    ) -> Result<Value> {
        self.worktree_host()?;
        // Held across the check and the create: two starts cannot both pass.
        let _op = self.reserve_worktree(&thread.thread_id).await?;
        if self.thread_work_dir(&thread.thread_id).await?.is_some() {
            return Err(OrchestratorError::request(
                "worktree_exists",
                "This thread already works in a worktree.",
            ));
        }
        let runtime = self.runtime(&thread.runtime_id)?;
        let plan = self.worktree_preflight(thread, source, &runtime).await?;
        let label = thread.title.as_deref().unwrap_or("thread");
        let binding = self
            .create_worktree(&thread.thread_id, &plan, label, None, None)
            .await?;
        self.worktree_view(&binding).await
    }
}

/// The worktree's HEAD is still its recorded branch (not detached, not
/// switched to another branch).
pub(super) async fn on_branch(git: &Git<'_>, b: &WorktreeBinding) -> Result<bool> {
    Ok(git.symbolic_head(&b.path).await?.as_deref() == Some(&format!("refs/heads/{}", b.branch)))
}

pub(super) async fn folder_exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false) // silent-ok: an unreadable folder is treated as gone; git reports the rest
}
