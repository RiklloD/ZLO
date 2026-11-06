//! The git commands worktree isolation needs, run through the host's
//! [`HostCommandRunner`]. No command here forces anything: no `--force`,
//! no `reset --hard`, no checkout over local changes.
//!
//! Every invocation runs with the repository's hooks and fsmonitor off and
//! with every network path closed: no lazy fetch of missing objects from a
//! promisor remote, no transport of any protocol, no ssh, askpass or
//! credential helper. None of these commands needs the network, so a
//! missing object fails the command instead of reaching out.
//!
//! Trusted: the repository's own local config (`.git/config`). Filters
//! (smudge/clean, e.g. git-lfs) and merge drivers named by
//! `.gitattributes` run as configured, because they can only be defined in
//! that local config, which a clone never brings — the same trust the
//! person already gives their own git.

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

/// Environment and config every orchestration git command runs with: no
/// hooks of any kind (`post-checkout` on `worktree add`, `post-commit`,
/// `post-merge`, `reference-transaction`, …), no fsmonitor program, and no
/// network: lazy fetch off, every transport protocol refused, ssh, askpass
/// and credential helpers neutralized.
const SAFE_CONFIG: &str = "GIT_NO_LAZY_FETCH=1 GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=false \
SSH_ASKPASS=false GIT_SSH_COMMAND=false git -c core.hooksPath=/dev/null -c core.fsmonitor=false \
-c protocol.allow=never -c core.sshCommand=false -c credential.helper=";

fn line(args: &[&str]) -> String {
    let mut out = String::from(SAFE_CONFIG);
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
        Ok(PathBuf::from(last_line(&out.output)))
    }

    pub(crate) async fn head(&self, dir: &Path) -> Result<CommitId> {
        let out = self.raw(dir, &["rev-parse", "--verify", "HEAD"]).await?;
        if !out.success() {
            return Err(OrchestratorError::request(
                "git_failed",
                "the repository has no commits yet; commit once before using a worktree",
            ));
        }
        CommitId::new(last_line(&out.output))
    }

    pub(crate) async fn rev(&self, dir: &Path, rev: &str) -> Result<CommitId> {
        CommitId::new(last_line(
            &self.run(dir, &["rev-parse", "--verify", rev]).await?,
        ))
    }

    /// The ref `HEAD` points at (`refs/heads/<name>`), `None` when detached.
    pub(crate) async fn symbolic_head(&self, dir: &Path) -> Result<Option<String>> {
        let out = self.raw(dir, &["symbolic-ref", "-q", "HEAD"]).await?;
        match out.exit_code {
            0 => Ok(Some(last_line(&out.output).to_string())),
            1 => Ok(None),
            _ => Err(failed(&["symbolic-ref"], &out)),
        }
    }

    /// Paths staged in the index of the checkout `dir`.
    pub(crate) async fn staged(&self, dir: &Path) -> Result<Vec<String>> {
        let out = self.run(dir, &["diff", "--cached", "--name-only"]).await?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// Paths that differ between two commits.
    pub(crate) async fn changed_between(
        &self,
        dir: &Path,
        a: &str,
        b: &str,
    ) -> Result<Vec<String>> {
        let out = self.run(dir, &["diff", "--name-only", a, b]).await?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// The repository's identity, as opposed to its path: the device and
    /// inode of its common git dir plus its root commits. Another repository
    /// put at the same path has another identity.
    pub(crate) async fn repo_identity(&self, dir: &Path) -> Result<String> {
        let common = self.run(dir, &["rev-parse", "--git-common-dir"]).await?;
        let common = PathBuf::from(last_line(&common));
        let common = if common.is_absolute() {
            common
        } else {
            dir.join(common)
        };
        let meta = tokio::fs::metadata(&common).await.map_err(|e| {
            OrchestratorError::request("git_failed", format!("could not read the git dir: {e}"))
        })?;
        #[cfg(unix)]
        let place = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}", meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let place = {
            let _ = &meta;
            tokio::fs::canonicalize(&common)
                .await
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|e| {
                    OrchestratorError::request(
                        "git_failed",
                        format!("could not read the git dir: {e}"),
                    )
                })?
        };
        let roots = self
            .run(dir, &["rev-list", "--max-parents=0", "HEAD"])
            .await?;
        let mut roots: Vec<&str> = roots
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        roots.sort_unstable();
        Ok(format!("{place}:{}", roots.join(",")))
    }

    /// Tracked files with staged or unstaged changes (untracked ignored).
    pub(crate) async fn tracked_changes(&self, dir: &Path) -> Result<Vec<String>> {
        let out = self
            .run(dir, &["status", "--porcelain=v1", "--untracked-files=no"])
            .await?;
        Ok(porcelain_paths(&out))
    }

    /// A merge, cherry-pick, revert, rebase or bisect left in progress in
    /// the checkout `dir`, by name.
    pub(crate) async fn in_progress(&self, dir: &Path) -> Result<Option<&'static str>> {
        for (name, what) in [
            ("MERGE_HEAD", "merge"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
            ("sequencer", "cherry-pick/revert sequence"),
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase or am"),
            ("BISECT_LOG", "bisect"),
        ] {
            let out = self.run(dir, &["rev-parse", "--git-path", name]).await?;
            let path = PathBuf::from(last_line(&out));
            let path = if path.is_absolute() {
                path
            } else {
                dir.join(path)
            };
            if tokio::fs::try_exists(&path).await.map_err(|e| {
                OrchestratorError::request("git_failed", format!("could not read git state: {e}"))
            })? {
                return Ok(Some(what));
            }
        }
        Ok(None)
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
        Ok(porcelain_paths(&out))
    }

    /// Commits reachable from `to` but not from `from`.
    pub(crate) async fn count(&self, dir: &Path, from: &str, to: &str) -> Result<u64> {
        let range = format!("{from}..{to}");
        let out = self.run(dir, &["rev-list", "--count", &range]).await?;
        last_line(&out).parse().map_err(|e| {
            OrchestratorError::request(
                "git_failed",
                format!("unexpected rev-list output {out:?}: {e}"),
            )
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
        CommitId::new(last_line(&self.run(dir, &["merge-base", a, b]).await?))
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

/// The last non-empty line: git's answer, after any warning it printed
/// (stderr is folded into the output).
fn last_line(out: &str) -> &str {
    out.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn porcelain_paths(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.len() > 3)
        .map(|l| l.get(3..).unwrap_or(l).to_string())
        .collect()
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
