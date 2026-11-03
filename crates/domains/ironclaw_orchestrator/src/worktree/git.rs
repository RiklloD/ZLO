//! The git commands worktree isolation needs, run through the host's
//! [`HostCommandRunner`]. No command here forces anything: no `--force`,
//! no `reset --hard`, no checkout over local changes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{CommitId, HostCommand, HostCommandOutput, HostCommandRunner, WorktreeBranch};
use crate::error::OrchestratorError;

type Result<T> = std::result::Result<T, OrchestratorError>;

const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Git output past this many bytes is cut in error messages.
const MESSAGE_MAX: usize = 1_500;

/// Quote one argument for `sh -c`.
pub(crate) fn sh_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

fn line(args: &[&str]) -> String {
    let mut out = String::from("git");
    for arg in args {
        out.push(' ');
        out.push_str(&sh_quote(arg));
    }
    out
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(crate) struct Git<'a> {
    runner: &'a dyn HostCommandRunner,
}

impl<'a> Git<'a> {
    pub(crate) fn new(runner: &'a dyn HostCommandRunner) -> Self {
        Self { runner }
    }

    /// Run `git <args>` in `dir`, whatever it exits with.
    pub(crate) async fn raw(&self, dir: &Path, args: &[&str]) -> Result<HostCommandOutput> {
        self.runner
            .run(HostCommand {
                line: line(args),
                workdir: dir.to_path_buf(),
                timeout: GIT_TIMEOUT,
            })
            .await
    }

    /// stdout of a successful `git <args>`; a model-visible `git_failed`
    /// with git's own message otherwise.
    pub(crate) async fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self.raw(dir, args).await?;
        if out.success() {
            Ok(out.output)
        } else {
            Err(failed(args, &out))
        }
    }

    /// Top folder of the repository `dir` is in.
    pub(crate) async fn toplevel(&self, dir: &Path) -> Result<PathBuf> {
        let out = self.raw(dir, &["rev-parse", "--show-toplevel"]).await?;
        if !out.success() {
            return Err(OrchestratorError::request(
                "not_a_git_repository",
                format!(
                    "{} is not inside a git repository, so a worktree cannot be made from it. \
                     Use isolation='shared', or work from a git checkout.",
                    dir.display()
                ),
            ));
        }
        Ok(PathBuf::from(out.output.trim()))
    }

    pub(crate) async fn head(&self, dir: &Path) -> Result<CommitId> {
        let out = self.raw(dir, &["rev-parse", "--verify", "HEAD"]).await?;
        if !out.success() {
            return Err(OrchestratorError::request(
                "git_failed",
                "the repository has no commits yet; commit once before using a worktree",
            ));
        }
        CommitId::new(out.output)
    }

    pub(crate) async fn rev(&self, dir: &Path, rev: &str) -> Result<CommitId> {
        CommitId::new(self.run(dir, &["rev-parse", "--verify", rev]).await?)
    }

    pub(crate) async fn worktree_add(
        &self,
        repo: &Path,
        branch: &WorktreeBranch,
        path: &Path,
        base: &CommitId,
    ) -> Result<()> {
        self.run(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch.as_str(),
                &path_arg(path),
                base.as_str(),
            ],
        )
        .await
        .map(drop)
    }

    /// Paths `git status` reports as changed or untracked.
    pub(crate) async fn dirty_files(&self, dir: &Path) -> Result<Vec<String>> {
        let out = self
            .run(dir, &["status", "--porcelain=v1", "--untracked-files=all"])
            .await?;
        Ok(out
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| l.get(3..).unwrap_or(l).to_string())
            .collect())
    }

    /// Commits reachable from `to` but not from `from`.
    pub(crate) async fn count(&self, dir: &Path, from: &str, to: &str) -> Result<u64> {
        let range = format!("{from}..{to}");
        let out = self.run(dir, &["rev-list", "--count", &range]).await?;
        out.trim().parse().map_err(|_| {
            OrchestratorError::request("git_failed", format!("unexpected rev-list output {out:?}"))
        })
    }

    pub(crate) async fn is_ancestor(&self, dir: &Path, rev: &str, of: &str) -> Result<bool> {
        let out = self
            .raw(dir, &["merge-base", "--is-ancestor", rev, of])
            .await?;
        match out.exit_code {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(failed(&["merge-base", "--is-ancestor"], &out)),
        }
    }

    pub(crate) async fn merge_base(&self, dir: &Path, a: &str, b: &str) -> Result<CommitId> {
        CommitId::new(self.run(dir, &["merge-base", a, b]).await?)
    }

    /// `-c user.*` for commits when the person has no git identity set.
    async fn identity(&self, dir: &Path) -> Result<Vec<&'static str>> {
        let set = self.raw(dir, &["config", "user.email"]).await?.success();
        Ok(if set {
            Vec::new()
        } else {
            vec!["-c", "user.name=ZLO", "-c", "user.email=zlo@localhost"]
        })
    }

    /// Commit everything in the worktree (tracked and untracked, ignored
    /// files excepted) on its branch. `None` when there was nothing.
    pub(crate) async fn commit_all(&self, dir: &Path, message: &str) -> Result<Option<CommitId>> {
        if self.dirty_files(dir).await?.is_empty() {
            return Ok(None);
        }
        self.run(dir, &["add", "-A"]).await?;
        let mut args = self.identity(dir).await?;
        args.extend(["commit", "--no-verify", "-q", "-m", message]);
        self.run(dir, &args).await?;
        self.head(dir).await.map(Some)
    }

    /// Run a merge-like command with the person's identity (or ZLO's).
    pub(crate) async fn integrate(&self, dir: &Path, args: &[&str]) -> Result<HostCommandOutput> {
        let mut full = self.identity(dir).await?;
        full.extend_from_slice(args);
        self.raw(dir, &full).await
    }

    /// Files left with conflict markers by a merge or cherry-pick.
    pub(crate) async fn conflicts(&self, dir: &Path) -> Result<Vec<String>> {
        let out = self
            .run(dir, &["diff", "--name-only", "--diff-filter=U"])
            .await?;
        Ok(out.lines().map(str::to_string).collect())
    }

    pub(crate) async fn update_ref(&self, dir: &Path, name: &str, to: &CommitId) -> Result<()> {
        self.run(dir, &["update-ref", name, to.as_str()])
            .await
            .map(drop)
    }

    /// Remove a clean worktree (git refuses a dirty one; it is never forced).
    pub(crate) async fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<()> {
        self.run(repo, &["worktree", "remove", &path_arg(path)])
            .await
            .map(drop)
    }

    pub(crate) async fn worktree_prune(&self, repo: &Path) -> Result<()> {
        self.run(repo, &["worktree", "prune"]).await.map(drop)
    }

    /// Delete a branch the caller has checked holds no unique work.
    pub(crate) async fn branch_delete(&self, repo: &Path, branch: &WorktreeBranch) -> Result<()> {
        self.run(repo, &["branch", "-D", branch.as_str()])
            .await
            .map(drop)
    }

    pub(crate) async fn branch_exists(&self, repo: &Path, branch: &WorktreeBranch) -> Result<bool> {
        let name = format!("refs/heads/{branch}");
        Ok(self
            .raw(repo, &["show-ref", "--verify", "--quiet", &name])
            .await?
            .success())
    }
}

fn failed(args: &[&str], out: &HostCommandOutput) -> OrchestratorError {
    let what = args
        .iter()
        .find(|a| !a.starts_with('-') && !a.contains('='))
        .copied()
        .unwrap_or("git");
    let text = out.output.trim();
    let text = if text.len() > MESSAGE_MAX {
        super::tail(text, MESSAGE_MAX)
    } else {
        text.to_string()
    };
    OrchestratorError::request(
        "git_failed",
        format!("git {what} failed (exit {}): {text}", out.exit_code),
    )
}

#[cfg(test)]
mod tests {
    use super::sh_quote;

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(sh_quote("zlo/fix-1"), "zlo/fix-1");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }
}
