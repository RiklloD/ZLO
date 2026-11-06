//! Bringing a worktree's branch into the checkout it came from, and
//! removing a worktree. Both refuse rather than guess:
//!
//! - nothing is committed on anyone's behalf: a worktree with uncommitted
//!   changes is refused (they are listed), so no credential file or build
//!   artifact lands in history unasked;
//! - the worktree's HEAD must still be its recorded branch, or hold no
//!   commits that branch lacks; a detached HEAD with its own commits is
//!   archived under a ref before its folder goes;
//! - the target checkout must be clean (index and tracked files) with no
//!   merge, cherry-pick, revert, rebase or bisect in progress;
//! - an integration that does not finish cleanly — conflict, refusal,
//!   timeout, an empty cherry-pick, a failed squash commit — is rolled back
//!   to the saved HEAD and the rollback is verified; only `on_conflict =
//!   leave` keeps a conflicted merge, on request.

use std::path::Path;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use super::git::Git;
use super::ops::{folder_exists, on_branch};
use super::{CommitId, MergeStrategy, OnConflict, WorktreeBinding, WorktreeState, tail};
use crate::engine::{Engine, Result};
use crate::error::OrchestratorError;
use crate::model::{ThreadRecord, now_ms};

/// Files listed in a refusal.
const LIST_MAX: usize = 20;

fn listed(files: &[String]) -> String {
    let mut out: Vec<&str> = files.iter().take(LIST_MAX).map(String::as_str).collect();
    if files.len() > LIST_MAX {
        out.push("…");
    }
    out.join(", ")
}

fn strategy_name(strategy: MergeStrategy) -> &'static str {
    match strategy {
        MergeStrategy::Merge => "merge",
        MergeStrategy::Squash => "squash",
        MergeStrategy::CherryPick => "cherry_pick",
    }
}

impl Engine {
    /// `t3_thread action=merge_worktree`: bring the worktree's branch into
    /// the checkout it was made from. `caller` is the thread asking (its own
    /// running turn may be in that checkout); `None` for a person's request.
    pub async fn merge_worktree(
        &self,
        thread: &ThreadRecord,
        strategy: MergeStrategy,
        on_conflict: OnConflict,
        caller: Option<&ThreadId>,
    ) -> Result<Value> {
        let binding = self.require_worktree(&thread.thread_id).await?;
        let _op = self.reserve_worktree(&thread.thread_id).await?;
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
        if binding.state.has_folder() && folder_exists(&binding.path).await {
            check_worktree(&git, &binding, "merged").await?;
        }
        let target = git.toplevel(&binding.source_dir).await?;
        if target == binding.path {
            return Err(OrchestratorError::invalid(
                "the worktree cannot be merged into itself",
            ));
        }
        let _checkout = self.checkout_lock(&target).await?;
        // No new turn starts in the target until the merge is done.
        let _target = self.reserve_checkout(&target, caller).await?;
        check_target(&git, &target).await?;
        let branch = binding.branch.as_str();
        let before = git.head(&target).await?;
        let pending = git.count(&target, before.as_str(), branch).await?;
        if pending == 0 {
            return Ok(json!({
                "ok": true, "merged": false,
                "message": "Already up to date: the checkout has every commit of the branch.",
                "branch": branch, "target": target, "head": before.as_str(),
            }));
        }
        let label = thread.title.as_deref().unwrap_or(branch);
        let plan = Integration {
            git: &git,
            target: &target,
            branch,
            before: &before,
            strategy,
            on_conflict,
            squash_message: format!(
                "{} (squashed from {branch})",
                label.trim_start_matches("↳ ")
            ),
        };
        // From here on the checkout is being changed: every way out but a
        // finished merge (or a conflict kept on request) is rolled back.
        let after = match plan.run().await {
            Ok(after) => after,
            Err(Stop::Kept(conflicts)) => {
                return Err(OrchestratorError::request(
                    "merge_conflict",
                    format!(
                        "Merging {branch} into {} stopped on conflicts in: {}. The conflicted \
                         merge is left in the checkout to resolve there.",
                        target.display(),
                        listed(&conflicts)
                    ),
                ));
            }
            Err(stop) => {
                let restored = rollback(&git, &target, &before).await;
                return Err(match (stop, restored) {
                    (Stop::Conflict(conflicts), Ok(())) => OrchestratorError::request(
                        "merge_conflict",
                        format!(
                            "Merging {branch} into {} stopped on conflicts in: {}. {} Resolve in \
                             the worktree (merge the checkout's branch into it) and merge again, \
                             or retry with on_conflict='leave'.",
                            target.display(),
                            listed(&conflicts),
                            restored_text(&Ok(())),
                        ),
                    ),
                    (Stop::Conflict(conflicts), Err(state)) => refused(
                        &target,
                        &format!("it stopped on conflicts in {}", listed(&conflicts)),
                        Err(state),
                    ),
                    (Stop::Failed(why), restored) => refused(&target, &why, restored),
                    (Stop::Kept(_), _) => OrchestratorError::invalid("unreachable merge state"),
                });
            }
        };
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
            "strategy": strategy_name(strategy), "commits": pending,
            "headBefore": before.as_str(), "head": after.as_str(),
            "message": format!(
                "Merged {pending} commit(s) from {branch} into {}. remove_worktree cleans up the folder.",
                target.display()
            ),
        }))
    }

    /// `t3_thread action=remove_worktree`: remove the folder. Refused while
    /// it holds uncommitted changes; a branch with commits the checkout
    /// lacks is kept and archived under `refs/zlo/archive/`, and so is a
    /// detached HEAD with commits of its own.
    pub async fn remove_worktree(&self, thread: &ThreadRecord) -> Result<Value> {
        let binding = self.require_worktree(&thread.thread_id).await?;
        if binding.state == WorktreeState::Removed {
            return self.worktree_view(&binding).await;
        }
        let _op = self.reserve_worktree(&thread.thread_id).await?;
        let host = self.worktree_host()?;
        let git = Git::new(host.runner.as_ref());
        let exists = folder_exists(&binding.path).await;
        let source = git.rev(&binding.source_dir, "HEAD").await.ok(); // silent-ok: a gone source checkout means nothing counts as merged; everything is archived
        let mut archives: Vec<String> = Vec::new();
        if exists {
            let dirty = git.dirty_files(&binding.path).await?;
            if !dirty.is_empty() {
                return Err(OrchestratorError::request(
                    "worktree_dirty",
                    format!(
                        "The worktree has {} uncommitted file(s): {}. Nothing was removed and \
                         nothing is committed for you: commit what should be kept on branch {} \
                         (it is archived, never lost) or discard it, then remove again.",
                        dirty.len(),
                        listed(&dirty),
                        binding.branch
                    ),
                ));
            }
            // A HEAD off its branch keeps its own commits reachable.
            if !on_branch(&git, &binding).await? {
                let head = git.rev(&binding.path, "HEAD").await?;
                if !reachable(&git, &binding, &head, source.as_ref()).await? {
                    let name = format!("{}-head", binding.branch.archive_ref());
                    git.update_ref(&binding.repo_root, &name, &head).await?;
                    archives.push(name);
                }
            }
        }
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
            let merged = match &source {
                Some(source) => {
                    git.is_ancestor(&binding.repo_root, branch, source.as_str())
                        .await?
                }
                None => false,
            };
            if own > 0 && !merged {
                let name = binding.branch.archive_ref();
                git.update_ref(&binding.repo_root, &name, &tip).await?;
                archives.insert(0, name);
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
        let archive = (!archives.is_empty()).then(|| archives.join(", "));
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
        view["message"] = json!(match &archive {
            Some(refs) =>
                format!("Removed the folder. Work the checkout lacks stays reachable from {refs}."),
            None => "Removed the folder and its branch: every commit was already in the checkout."
                .to_string(),
        });
        Ok(view)
    }
}

/// Why an integration stopped before finishing.
enum Stop {
    /// Conflicts, to be rolled back.
    Conflict(Vec<String>),
    /// Conflicts, left in the checkout on request.
    Kept(Vec<String>),
    /// Anything else, including errors reading git state mid-way.
    Failed(String),
}

impl From<OrchestratorError> for Stop {
    fn from(error: OrchestratorError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// One integration of `branch` into the clean checkout `target`.
struct Integration<'a> {
    git: &'a Git<'a>,
    target: &'a Path,
    branch: &'a str,
    before: &'a CommitId,
    strategy: MergeStrategy,
    on_conflict: OnConflict,
    squash_message: String,
}

impl Integration<'_> {
    /// Run it to the end; the new HEAD, or why it stopped. Every step after
    /// the first mutation reports through `Stop`, never `?` past a rollback.
    async fn run(&self) -> std::result::Result<CommitId, Stop> {
        let (git, target, branch) = (self.git, self.target, self.branch);
        let base = git.merge_base(target, self.before.as_str(), branch).await?;
        let message = format!("Merge {branch} (ZLO worktree)");
        let range = format!("{}..{branch}", base.as_str());
        let args: Vec<&str> = match self.strategy {
            MergeStrategy::Merge => vec!["merge", "--no-ff", "--no-edit", "-m", &message, branch],
            MergeStrategy::Squash => vec!["merge", "--squash", branch],
            MergeStrategy::CherryPick => vec!["cherry-pick", &range],
        };
        let out = git
            .integrate(target, &args)
            .await
            .map_err(|e| Stop::Failed(format!("git did not finish: {e}")))?;
        if !out.success() {
            let conflicts = git.conflicts(target).await?;
            return Err(match (conflicts.is_empty(), self.on_conflict) {
                (true, _) => Stop::Failed(out.output.trim().to_string()),
                (false, OnConflict::Leave) => Stop::Kept(conflicts),
                (false, OnConflict::Abort) => Stop::Conflict(conflicts),
            });
        }
        if self.strategy == MergeStrategy::Squash {
            // Only the branch's own changes may be committed: anything else
            // staged meanwhile is not this merge's to commit.
            let own = git.changed_between(target, base.as_str(), branch).await?;
            let extra: Vec<String> = git
                .staged(target)
                .await?
                .into_iter()
                .filter(|p| !own.contains(p))
                .collect();
            if !extra.is_empty() {
                return Err(Stop::Failed(format!(
                    "the checkout's index gained other changes during the merge: {}",
                    listed(&extra)
                )));
            }
            let commit = git
                .integrate(
                    target,
                    &["commit", "--no-verify", "-q", "-m", &self.squash_message],
                )
                .await
                .map_err(|e| Stop::Failed(format!("the squash commit did not finish: {e}")))?;
            if !commit.success() {
                return Err(Stop::Failed(format!(
                    "the squashed changes could not be committed: {}",
                    commit.output.trim()
                )));
            }
        }
        // A "successful" run that left an operation behind is not a merge.
        if let Some(op) = git.in_progress(target).await? {
            return Err(Stop::Failed(format!("git left a {op} in progress")));
        }
        Ok(git.head(target).await?)
    }
}

/// The worktree is safe to merge from: no uncommitted changes, and HEAD on
/// its branch or holding nothing the branch lacks.
async fn check_worktree(git: &Git<'_>, b: &WorktreeBinding, verb: &str) -> Result<()> {
    let dirty = git.dirty_files(&b.path).await?;
    if !dirty.is_empty() {
        return Err(OrchestratorError::request(
            "worktree_dirty",
            format!(
                "The worktree has {} uncommitted file(s): {}. Only branch {} is {verb} and \
                 nothing is committed for you: commit what should go in (or discard it), then \
                 try again.",
                dirty.len(),
                listed(&dirty),
                b.branch
            ),
        ));
    }
    if !on_branch(git, b).await? {
        let head = git.rev(&b.path, "HEAD").await?;
        if !git
            .is_ancestor(&b.repo_root, head.as_str(), b.branch.as_str())
            .await?
        {
            let at = git
                .symbolic_head(&b.path)
                .await?
                .unwrap_or_else(|| format!("detached at {}", head.short()));
            return Err(OrchestratorError::request(
                "worktree_head_moved",
                format!(
                    "The worktree's HEAD is {at}, with commits branch {} does not have; only that \
                     branch is {verb}. Put the work on branch {} first, then try again.",
                    b.branch, b.branch
                ),
            ));
        }
    }
    Ok(())
}

/// The target checkout can take an integration: no operation in progress
/// and no staged or unstaged changes to tracked files.
async fn check_target(git: &Git<'_>, target: &Path) -> Result<()> {
    if let Some(op) = git.in_progress(target).await? {
        return Err(OrchestratorError::request(
            "target_busy",
            format!(
                "A {op} is in progress in {}; finish or abort it there first. Nothing was changed.",
                target.display()
            ),
        ));
    }
    let changed = git.tracked_changes(target).await?;
    if !changed.is_empty() {
        return Err(OrchestratorError::request(
            "target_dirty",
            format!(
                "{} has uncommitted changes to tracked files: {}. Commit or stash them there \
                 first so the merge cannot mix with them. Nothing was changed.",
                target.display(),
                listed(&changed)
            ),
        ));
    }
    Ok(())
}

/// `head` is reachable from the branch or the source checkout.
async fn reachable(
    git: &Git<'_>,
    b: &WorktreeBinding,
    head: &CommitId,
    source: Option<&CommitId>,
) -> Result<bool> {
    if git.branch_exists(&b.repo_root, &b.branch).await?
        && git
            .is_ancestor(&b.repo_root, head.as_str(), b.branch.as_str())
            .await?
    {
        return Ok(true);
    }
    match source {
        Some(source) => {
            git.is_ancestor(&b.repo_root, head.as_str(), source.as_str())
                .await
        }
        None => Ok(false),
    }
}

/// Undo an unfinished integration in `target` (which was clean before it)
/// and verify: HEAD back at `before`, no operation in progress, no tracked
/// changes. `Err` describes what is still off.
async fn rollback(
    git: &Git<'_>,
    target: &Path,
    before: &CommitId,
) -> std::result::Result<(), String> {
    let step = async {
        match git.in_progress(target).await? {
            Some("merge") => {
                git.raw(target, &["merge", "--abort"]).await?;
            }
            Some("cherry-pick") | Some("cherry-pick/revert sequence") => {
                git.raw(target, &["cherry-pick", "--abort"]).await?;
            }
            _ => {}
        }
        let head = git.head(target).await?;
        if head != *before
            || !git.tracked_changes(target).await?.is_empty()
            || git.in_progress(target).await?.is_some()
        {
            // The checkout was clean before: this only drops what the
            // integration itself brought in.
            git.raw(target, &["reset", "--merge", before.as_str()])
                .await?;
        }
        let head = git.head(target).await?;
        let changed = git.tracked_changes(target).await?;
        let op = git.in_progress(target).await?;
        Ok::<_, OrchestratorError>((head, changed, op))
    };
    match step.await {
        Ok((head, changed, op)) if head == *before && changed.is_empty() && op.is_none() => Ok(()),
        Ok((head, changed, op)) => Err(format!(
            "HEAD at {} (expected {}), {} changed tracked file(s){}",
            head.short(),
            before.short(),
            changed.len(),
            op.map(|o| format!(", a {o} in progress"))
                .unwrap_or_default()
        )),
        Err(error) => Err(error.to_string()),
    }
}

fn restored_text(restored: &std::result::Result<(), String>) -> String {
    match restored {
        Ok(()) => "It was rolled back; the checkout is as it was.".to_string(),
        Err(state) => format!(
            "Rolling it back did not fully succeed ({state}); check the checkout before anything else."
        ),
    }
}

fn refused(
    target: &Path,
    why: &str,
    restored: std::result::Result<(), String>,
) -> OrchestratorError {
    let code = if restored.is_ok() {
        "merge_refused"
    } else {
        "merge_rollback_failed"
    };
    OrchestratorError::request(
        code,
        format!(
            "The merge into {} did not complete: {}. {}",
            target.display(),
            tail(why, 1_500),
            restored_text(&restored)
        ),
    )
}
