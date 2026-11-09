//! A thread's repository for a client on another computer
//! (`ironclaw_product_contracts::thread_repo`): status, diffs, history, and
//! stage / unstage / discard / last-turn undo.
//!
//! The repository is the one the thread works in ([`Engine::thread_cwd`]:
//! its worktree, its profile's folder or its runtime's folder) — a request
//! never names a folder. Paths in a request are repository-relative, made
//! of plain components only, and reach git as literal pathspecs after
//! `--`. Every command runs through worktree isolation's hardened [`Git`]
//! (hooks and fsmonitor off, no network, no credential helper), plus
//! `--no-ext-diff --no-textconv` on diffs, and outputs are capped by the
//! contract's limits. Nothing here commits, pushes, fetches or forces.
//!
//! The last turn's checkpoint (`git stash create`: the working tree as a
//! commit, without touching it) is kept in memory per thread; a restart
//! forgets it, like the client's own card.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::thread_repo::*;
use serde_json::Value;

use crate::engine::Engine;
use crate::error::OrchestratorError;
use crate::model::ThreadRecord;
use crate::worktree::CommitId;
use crate::worktree::git::Git;

type Result<T> = std::result::Result<T, OrchestratorError>;

/// Untracked files a checkpoint remembers; more and the turn card is off.
const CHECKPOINT_UNTRACKED_MAX: usize = 20_000;
/// Files an undo puts back in one go.
const UNDO_PATHS_MAX: usize = 2_000;
/// Small outputs (a ref name, a count, a file list).
const SMALL: usize = 64 * 1024;
/// A status listing: generous, entries are capped after parsing.
const STATUS_BYTES: usize = 4 * 1024 * 1024;
/// Commit messages and logs.
const LOG_BYTES: usize = 2 * 1024 * 1024;

/// The working tree as it stood when a turn began.
#[derive(Debug, Clone)]
pub(crate) struct Checkpoint {
    repo: PathBuf,
    tree: CommitId,
    untracked: HashSet<String>,
}

pub(crate) type Checkpoints = std::sync::Mutex<HashMap<ThreadId, Checkpoint>>;

fn refused(message: impl Into<String>) -> OrchestratorError {
    OrchestratorError::request("path_outside_repository", message)
}

/// A repository-relative path: non-empty, bounded, plain components only
/// (no `..`, `.`, root or drive), no NUL. Anything else is refused before
/// git runs.
fn repo_path(raw: &str) -> Result<String> {
    if raw.is_empty() || raw.len() > GIT_PATH_MAX_BYTES || raw.contains('\0') {
        return Err(refused("That path is not a file in this repository."));
    }
    // Segment by segment: `Path::components` would quietly drop a `.`.
    let plain = raw
        .split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && Path::new(raw)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(refused(format!(
            "{raw} is outside this thread's repository; only paths inside it can be changed."
        )));
    }
    Ok(raw.to_string())
}

fn repo_paths(raw: &[String]) -> Result<Vec<String>> {
    if raw.is_empty() {
        return Err(OrchestratorError::invalid("name at least one path"));
    }
    if raw.len() > GIT_PATHS_MAX {
        return Err(OrchestratorError::invalid(format!(
            "at most {GIT_PATHS_MAX} paths per change"
        )));
    }
    raw.iter().map(|p| repo_path(p)).collect()
}

/// A patch from a capped capture: whole lines only.
fn patch_of(out: &crate::worktree::git::Captured) -> (String, bool) {
    let text = out.text();
    if !out.truncated {
        return (text, false);
    }
    // Drop the half line the cut left, so a parser sees whole hunks lines.
    match text.rfind('\n') {
        Some(nl) => (text[..=nl].to_string(), true),
        None => (String::new(), true),
    }
}

/// Porcelain v1 with `-z`: `XY path\0`, renames followed by `from\0`.
fn parse_status(out: &str) -> Vec<GitChange> {
    let mut changes = Vec::new();
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    while let Some(entry) = parts.next() {
        let mut chars = entry.chars();
        let (Some(index), Some(worktree)) = (chars.next(), chars.next()) else {
            continue;
        };
        let path = entry.get(3..).unwrap_or_default().to_string();
        let from = matches!(index, 'R' | 'C')
            .then(|| parts.next().map(str::to_string))
            .flatten();
        changes.push(GitChange {
            path,
            from,
            index,
            worktree,
        });
    }
    changes
}

fn parse_log(out: &str) -> Vec<GitCommit> {
    out.split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').split('\x1f');
            let sha = f.next()?.trim().to_string();
            if sha.is_empty() {
                return None;
            }
            Some(GitCommit {
                sha,
                parents: f.next()?.split_whitespace().map(str::to_string).collect(),
                author: f.next()?.to_string(),
                time: f.next()?.trim().parse().unwrap_or(0),
                refs: f
                    .next()?
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(|r| r.trim_start_matches("HEAD -> ").to_string())
                    .collect(),
                subject: f.next().unwrap_or_default().trim_end().to_string(),
            })
        })
        .collect()
}

fn nul_list(out: &str) -> Vec<String> {
    out.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

/// One thread's repository on this machine.
struct Repo<'a> {
    git: Git<'a>,
    root: PathBuf,
}

impl Repo<'_> {
    async fn small(&self, args: &[&str]) -> Result<String> {
        Ok(self.git.capture_ok(&self.root, args, SMALL).await?.text())
    }

    async fn has_head(&self) -> Result<bool> {
        Ok(self
            .git
            .capture(&self.root, &["rev-parse", "--verify", "-q", "HEAD"], SMALL)
            .await?
            .success())
    }

    async fn branch(&self) -> Result<GitBranch> {
        let name = self
            .git
            .capture(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"], SMALL)
            .await?;
        let name = if name.success() {
            name.text().trim().to_string()
        } else {
            "HEAD".to_string() // silent-ok: no commits yet, git has no branch name to print
        };
        let upstream = self
            .git
            .capture(
                &self.root,
                &["rev-parse", "--abbrev-ref", "@{upstream}"],
                SMALL,
            )
            .await?;
        let upstream = upstream
            .success()
            .then(|| upstream.text().trim().to_string());
        let (mut ahead, mut behind) = (0, 0);
        if upstream.is_some() {
            let counts = self
                .small(&["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
                .await?;
            let mut it = counts.split_whitespace().map(|n| n.parse::<u32>());
            if let (Some(Ok(b)), Some(Ok(a))) = (it.next(), it.next()) {
                behind = b;
                ahead = a;
            }
        }
        Ok(GitBranch {
            name,
            upstream,
            ahead,
            behind,
        })
    }

    async fn changes(&self, path: Option<&str>) -> Result<(Vec<GitChange>, bool)> {
        let mut args = vec![
            "--no-optional-locks",
            "--literal-pathspecs",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
        ];
        if let Some(path) = path {
            args.extend(["--", path]);
        }
        let out = self.git.capture_ok(&self.root, &args, STATUS_BYTES).await?;
        let mut changes = parse_status(&out.text());
        let mut truncated = out.truncated;
        if changes.len() > GIT_STATUS_MAX_ENTRIES {
            changes.truncate(GIT_STATUS_MAX_ENTRIES);
            truncated = true;
        }
        Ok((changes, truncated))
    }

    async fn untracked(&self) -> Result<(HashSet<String>, bool)> {
        let out = self
            .git
            .capture_ok(
                &self.root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                STATUS_BYTES,
            )
            .await?;
        let list = nul_list(&out.text());
        let over = out.truncated || list.len() > CHECKPOINT_UNTRACKED_MAX;
        Ok((list.into_iter().collect(), over))
    }

    /// `base` patches plus new files as patches against nothing, within
    /// [`GIT_PATCH_MAX_BYTES`].
    async fn patch_with_new(&self, base: &[&str], new: &[String]) -> Result<GitPatchView> {
        let out = self
            .git
            .capture_ok(&self.root, base, GIT_PATCH_MAX_BYTES)
            .await?;
        let (mut patch, mut truncated) = patch_of(&out);
        truncated |= new.len() > GIT_UNTRACKED_DIFF_MAX;
        for path in new.iter().take(GIT_UNTRACKED_DIFF_MAX) {
            let room = GIT_PATCH_MAX_BYTES.saturating_sub(patch.len());
            if room == 0 {
                truncated = true;
                break;
            }
            // Exit 1 means "differs": the patch is the answer either way.
            let one = self
                .git
                .capture(
                    &self.root,
                    &[
                        "diff",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-index",
                        "--",
                        "/dev/null",
                        path,
                    ],
                    room,
                )
                .await?;
            let (text, cut) = patch_of(&one);
            patch.push_str(&text);
            truncated |= cut;
        }
        Ok(GitPatchView { patch, truncated })
    }

    async fn status(&self, checkpoint: bool) -> Result<GitStatusView> {
        let (changes, truncated) = self.changes(None).await?;
        Ok(GitStatusView {
            repo_root: self.root.to_string_lossy().into_owned(),
            branch: self.branch().await?,
            changes,
            truncated,
            checkpoint,
        })
    }

    async fn diff(
        &self,
        scope: GitDiffScope,
        path: Option<&str>,
        checkpoint: Option<&Checkpoint>,
    ) -> Result<GitPatchView> {
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
        ];
        let tree;
        let untracked_now = || async {
            let (changes, _) = self.changes(path).await?;
            Ok::<_, OrchestratorError>(
                changes
                    .into_iter()
                    .filter(|c| c.index == '?')
                    .map(|c| c.path)
                    .collect::<Vec<_>>(),
            )
        };
        let new = match scope {
            GitDiffScope::Working => {
                if self.has_head().await? {
                    args.push("HEAD");
                }
                untracked_now().await?
            }
            GitDiffScope::Staged => {
                args.push("--cached");
                Vec::new()
            }
            GitDiffScope::Unstaged => untracked_now().await?,
            GitDiffScope::Turn => {
                let Some(cp) = checkpoint else {
                    return Err(OrchestratorError::request(
                        "no_checkpoint",
                        "There is no checkpoint for this thread's last turn.",
                    ));
                };
                tree = cp.tree.as_str().to_string();
                args.push(&tree);
                let (now, _) = self.untracked().await?;
                let mut created: Vec<String> = now
                    .into_iter()
                    .filter(|p| !cp.untracked.contains(p) && path.is_none_or(|w| w == p))
                    .collect();
                created.sort();
                created
            }
        };
        if let Some(path) = path {
            args.extend(["--", path]);
        }
        self.patch_with_new(&args, &new).await
    }

    async fn log(&self, limit: u32) -> Result<GitLogView> {
        if !self.has_head().await? {
            return Ok(GitLogView {
                commits: Vec::new(),
            });
        }
        let limit = format!("-n{}", limit.clamp(1, GIT_LOG_MAX));
        let out = self
            .git
            .capture_ok(
                &self.root,
                &[
                    "log",
                    "--all",
                    "--topo-order",
                    &limit,
                    "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s%x1e",
                ],
                LOG_BYTES,
            )
            .await?;
        let mut commits = parse_log(&out.text());
        if out.truncated {
            commits.pop(); // the last record may be cut
        }
        Ok(GitLogView { commits })
    }

    async fn commit(&self, sha: &str) -> Result<GitCommitView> {
        let sha = CommitId::new(sha)
            .map_err(|_| OrchestratorError::invalid("sha must be a commit id (hex)"))?;
        let message = self
            .git
            .capture_ok(
                &self.root,
                &["show", "-s", "--format=%B", sha.as_str()],
                LOG_BYTES,
            )
            .await?
            .text()
            .trim()
            .to_string();
        let base = [
            "show",
            "--format=",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
        ];
        let mut first_parent = base.to_vec();
        first_parent.extend(["--diff-merges=first-parent", sha.as_str()]);
        let mut out = self
            .git
            .capture(&self.root, &first_parent, GIT_PATCH_MAX_BYTES)
            .await?;
        if !out.success() && !out.truncated {
            // Older git has no `--diff-merges`.
            let mut plain = base.to_vec();
            plain.push(sha.as_str());
            out = self
                .git
                .capture_ok(&self.root, &plain, GIT_PATCH_MAX_BYTES)
                .await?;
        }
        let (patch, truncated) = patch_of(&out);
        Ok(GitCommitView {
            sha: sha.as_str().to_string(),
            message,
            patch,
            truncated,
        })
    }

    /// `git clean` one untracked file: never follows a symlink, never
    /// touches ignored files or anything outside the repository.
    async fn delete_untracked(&self, path: &str) -> Result<()> {
        self.small(&["--literal-pathspecs", "clean", "-f", "-q", "--", path])
            .await
            .map(drop)
    }

    async fn stage(&self, paths: &[String]) -> Result<()> {
        let mut args = vec!["--literal-pathspecs", "add", "-A", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.small(&args).await.map(drop)
    }

    async fn unstage(&self, paths: &[String]) -> Result<()> {
        let mut args = vec!["--literal-pathspecs", "restore", "--staged", "--"];
        args.extend(paths.iter().map(String::as_str));
        if self.has_head().await? {
            return self.small(&args).await.map(drop);
        }
        // No HEAD yet: `restore --staged` has nothing to restore from.
        let mut args = vec!["--literal-pathspecs", "rm", "--cached", "-q", "-r", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.small(&args).await.map(drop)
    }

    async fn discard(&self, path: &str) -> Result<()> {
        let (changes, _) = self.changes(Some(path)).await?;
        let Some(change) = changes.into_iter().find(|c| c.path == path) else {
            return Ok(()); // nothing to discard: already as committed
        };
        if change.index == '?' || change.index == 'A' {
            if change.index == 'A' {
                self.small(&[
                    "--literal-pathspecs",
                    "rm",
                    "--cached",
                    "-q",
                    "--ignore-unmatch",
                    "--",
                    path,
                ])
                .await?;
            }
            return self.delete_untracked(path).await;
        }
        self.small(&[
            "--literal-pathspecs",
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            path,
        ])
        .await
        .map(drop)
    }

    async fn checkpoint(&self) -> Result<Checkpoint> {
        let stash = self.small(&["stash", "create"]).await?;
        let stash = stash.trim();
        let tree = if stash.is_empty() {
            let head = self.small(&["rev-parse", "--verify", "HEAD"]).await?;
            CommitId::new(head.trim())?
        } else {
            CommitId::new(stash)?
        };
        let (untracked, over) = self.untracked().await?;
        if over {
            return Err(OrchestratorError::request(
                "too_many_files",
                "This repository has too many untracked files to checkpoint.",
            ));
        }
        Ok(Checkpoint {
            repo: self.root.clone(),
            tree,
            untracked,
        })
    }

    /// Put every file the turn touched back as it was at `cp`.
    async fn undo(&self, cp: &Checkpoint) -> Result<usize> {
        let touched = self
            .git
            .capture_ok(
                &self.root,
                &["diff", "--name-only", "-z", cp.tree.as_str()],
                STATUS_BYTES,
            )
            .await?;
        let touched = nul_list(&touched.text());
        let (now, _) = self.untracked().await?;
        let mut created: Vec<String> = now
            .into_iter()
            .filter(|p| !cp.untracked.contains(p))
            .collect();
        created.sort();
        if touched.len() + created.len() > UNDO_PATHS_MAX {
            return Err(OrchestratorError::request(
                "too_many_files",
                "The last turn touched too many files to undo here.",
            ));
        }
        let mut count = 0;
        for path in &touched {
            let spec = format!("{}:{path}", cp.tree);
            let existed = self
                .git
                .capture(&self.root, &["cat-file", "-e", &spec], SMALL)
                .await?
                .success();
            if existed {
                self.small(&[
                    "--literal-pathspecs",
                    "restore",
                    "--source",
                    cp.tree.as_str(),
                    "--worktree",
                    "--",
                    path,
                ])
                .await?;
            } else {
                self.small(&[
                    "--literal-pathspecs",
                    "rm",
                    "--cached",
                    "-q",
                    "--ignore-unmatch",
                    "--",
                    path,
                ])
                .await?;
                // Untracked before the turn and staged since: the checkpoint
                // never held its bytes, so it goes back to untracked and
                // stays on disk. Only files the turn created are deleted.
                if cp.untracked.contains(path) {
                    continue;
                }
                self.delete_untracked(path).await?;
            }
            count += 1;
        }
        for path in &created {
            self.delete_untracked(path).await?;
            count += 1;
        }
        Ok(count)
    }
}

impl Engine {
    /// The repository `thread` works in, with the hardened git runner.
    async fn thread_repo(&self, thread: &ThreadRecord) -> Result<Repo<'_>> {
        let host = self.worktree_host.get().ok_or_else(|| {
            OrchestratorError::request(
                "git_unavailable",
                "This backend does not run git for clients.",
            )
        })?;
        let Some(dir) = self.thread_cwd(thread).await? else {
            return Err(OrchestratorError::request(
                "no_repository",
                "This thread has no folder on its computer.",
            ));
        };
        let git = Git::new(host.runner.as_ref());
        let top = git
            .capture(&dir, &["rev-parse", "--show-toplevel"], SMALL)
            .await?;
        if !top.success() {
            return Err(OrchestratorError::request(
                "not_a_git_repository",
                "This thread's folder is not a git repository.",
            ));
        }
        Ok(Repo {
            git,
            root: PathBuf::from(top.text().trim()),
        })
    }

    fn checkpoint_of(&self, thread: &ThreadId, repo: &Path) -> Result<Option<Checkpoint>> {
        let map = self
            .git_checkpoints
            .lock()
            .map_err(|e| OrchestratorError::Store(format!("git checkpoints: {e}")))?;
        Ok(map.get(thread).filter(|cp| cp.repo == repo).cloned())
    }

    fn set_checkpoint(&self, thread: &ThreadId, cp: Option<Checkpoint>) -> Result<()> {
        let mut map = self
            .git_checkpoints
            .lock()
            .map_err(|e| OrchestratorError::Store(format!("git checkpoints: {e}")))?;
        match cp {
            Some(cp) => map.insert(thread.clone(), cp),
            None => map.remove(thread),
        };
        Ok(())
    }

    /// One read of `thread`'s repository (the caller checked ownership).
    pub async fn thread_git(&self, thread: &ThreadRecord, read: ThreadGitRead) -> Result<Value> {
        let repo = self.thread_repo(thread).await?;
        let cp = self.checkpoint_of(&thread.thread_id, &repo.root)?;
        match read {
            ThreadGitRead::Status {} => to_value(&repo.status(cp.is_some()).await?),
            ThreadGitRead::Diff { scope, path } => {
                let path = path.as_deref().map(repo_path).transpose()?;
                to_value(&repo.diff(scope, path.as_deref(), cp.as_ref()).await?)
            }
            ThreadGitRead::Log { limit } => to_value(&repo.log(limit.unwrap_or(200)).await?),
            ThreadGitRead::Commit { sha } => to_value(&repo.commit(sha.trim()).await?),
        }
    }

    /// One change to `thread`'s repository, answered with the status after it.
    pub async fn thread_git_change(
        &self,
        thread: &ThreadRecord,
        action: ThreadGitAction,
    ) -> Result<Value> {
        let repo = self.thread_repo(thread).await?;
        // A merge is writing into this checkout: wait for it.
        if self.checkout_reserved(&repo.root).await? {
            return Err(OrchestratorError::request(
                "thread_busy",
                "A worktree merge is writing into this checkout; try again in a moment.",
            ));
        }
        let id = &thread.thread_id;
        let mut note = None;
        match action {
            ThreadGitAction::Stage { paths } => repo.stage(&repo_paths(&paths)?).await?,
            ThreadGitAction::Unstage { paths } => repo.unstage(&repo_paths(&paths)?).await?,
            ThreadGitAction::Discard { path } => repo.discard(&repo_path(&path)?).await?,
            ThreadGitAction::Checkpoint {} => {
                let cp = repo.checkpoint().await?;
                self.set_checkpoint(id, Some(cp))?;
            }
            ThreadGitAction::UndoTurn {} => {
                let Some(cp) = self.checkpoint_of(id, &repo.root)? else {
                    return Err(OrchestratorError::request(
                        "no_checkpoint",
                        "There is no checkpoint for this thread's last turn.",
                    ));
                };
                let n = repo.undo(&cp).await?;
                note = Some(format!(
                    "Restored {n} file{}.",
                    if n == 1 { "" } else { "s" }
                ));
            }
            ThreadGitAction::KeepTurn {} => self.set_checkpoint(id, None)?,
        }
        let cp = self.checkpoint_of(id, &repo.root)?;
        to_value(&GitChangeResult {
            status: repo.status(cp.is_some()).await?,
            note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_relative_paths_pass() {
        for ok in ["a.txt", "src/lib.rs", "dir with space/f", "-n"] {
            assert_eq!(repo_path(ok).unwrap(), ok);
        }
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "a/../../b",
            "./a",
            "a/./b",
            "a//b",
            "dir/",
            "a\0b",
        ] {
            let err = repo_path(bad).unwrap_err();
            assert_eq!(err.code(), "path_outside_repository", "{bad:?}");
        }
        assert!(repo_path(&"a".repeat(GIT_PATH_MAX_BYTES + 1)).is_err());
        assert!(repo_paths(&[]).is_err());
        assert!(repo_paths(&vec!["a".to_string(); GIT_PATHS_MAX + 1]).is_err());
    }

    #[test]
    fn status_reads_renames_and_untracked() {
        let changes = parse_status("R  new.rs\0old.rs\0 M src/a.rs\0?? notes.md\0");
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].from.as_deref(), Some("old.rs"));
        assert_eq!(changes[2].index, '?');
    }
}
