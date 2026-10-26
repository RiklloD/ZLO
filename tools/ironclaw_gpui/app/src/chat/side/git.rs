//! Git for the right pane's coding tabs, run on this Mac with the system
//! `git` (architecture choice A: the app reads the project folder itself;
//! the gateway has no git routes yet). GPUI-free and blocking: callers run
//! these on the background executor.
//!
//! Shapes follow MonoCode Native's `monocode_core::git` (MIT): porcelain v1
//! status, unified patches for the diff viewer, a topo-ordered log for the
//! graph, and a per-turn checkpoint made with `git stash create`, which
//! records the working tree as a commit without touching it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Patches past this many bytes are cut, so a vendored tree or a lockfile
/// rewrite cannot stall the diff viewer.
const DIFF_LIMIT: usize = 4 * 1024 * 1024;
/// Untracked files are diffed one by one; past this many, they are listed
/// but not expanded.
const UNTRACKED_DIFF_MAX: usize = 60;

pub type GitResult<T> = Result<T, String>;

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        // A push that needs a password fails fast instead of waiting on a
        // prompt nobody can see.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    cmd
}

/// stdout of a successful run; stderr (or the exit code) otherwise.
fn git(dir: &Path, args: &[&str]) -> GitResult<String> {
    let out = command(dir, args)
        .output()
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        format!("git {} failed ({})", args.join(" "), out.status)
    } else {
        err
    })
}

/// stdout whatever the exit code: `diff --no-index` exits 1 when files differ.
fn git_any(dir: &Path, args: &[&str]) -> GitResult<String> {
    let out = command(dir, args)
        .output()
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repository's top folder, or an error when `dir` is not in one.
pub fn root(dir: &Path) -> GitResult<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(s.trim()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

pub fn branch(dir: &Path) -> Branch {
    let name = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "HEAD".into());
    let upstream = git(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        .ok()
        .map(|s| s.trim().to_string());
    let (behind, ahead) = upstream
        .as_ref()
        .and_then(|_| {
            git(
                dir,
                &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
            )
            .ok()
        })
        .and_then(|s| {
            let mut it = s.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
            Some((it.next()?, it.next()?))
        })
        .unwrap_or((0, 0));
    Branch {
        name,
        upstream,
        ahead,
        behind,
    }
}

/// One changed path. `index` and `worktree` are porcelain's X and Y.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub from: Option<String>,
    pub index: char,
    pub worktree: char,
}

impl Change {
    pub fn untracked(&self) -> bool {
        self.index == '?'
    }
    pub fn staged(&self) -> bool {
        !matches!(self.index, ' ' | '?' | '!')
    }
    pub fn unstaged(&self) -> bool {
        self.untracked() || !matches!(self.worktree, ' ' | '!')
    }
    /// The letter the list shows: the more telling of the two columns.
    pub fn letter(&self) -> char {
        match (self.index, self.worktree) {
            ('?', _) => 'U',
            ('U', _) | (_, 'U') => '!',
            (x, ' ') => x,
            (_, y) => y,
        }
    }
}

pub fn status(dir: &Path) -> GitResult<Vec<Change>> {
    let out = git(dir, &["status", "--porcelain=v1", "-z", "-uall"])?;
    Ok(parse_status(&out))
}

/// Porcelain v1 with `-z`: `XY path\0`, renames followed by `from\0`.
fn parse_status(out: &str) -> Vec<Change> {
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
        changes.push(Change {
            path,
            from,
            index,
            worktree,
        });
    }
    changes
}

/// Paths `git status` reports as ignored, for the explorer to dim or hide.
pub fn ignored(dir: &Path) -> HashSet<String> {
    git(dir, &["status", "--porcelain=v1", "-z", "--ignored"])
        .map(|out| {
            out.split('\0')
                .filter_map(|e| e.strip_prefix("!! "))
                .map(|p| p.trim_end_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn cap(mut patch: String) -> String {
    if patch.len() > DIFF_LIMIT {
        let mut cut = DIFF_LIMIT;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        patch.truncate(cut);
        // End on a whole line so the parser sees no half hunk line.
        if let Some(nl) = patch.rfind('\n') {
            patch.truncate(nl + 1);
        }
    }
    patch
}

/// New files as patches against nothing, so they show in the viewer too.
fn untracked_patches(dir: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths.iter().take(UNTRACKED_DIFF_MAX) {
        if let Ok(patch) = git_any(
            dir,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-index",
                "--",
                "/dev/null",
                path,
            ],
        ) {
            out.push_str(&patch);
        }
    }
    out
}

/// Everything not committed: staged and unstaged against HEAD, plus new
/// files. A repository without commits compares against the empty index.
pub fn working_diff(dir: &Path, changes: &[Change]) -> GitResult<String> {
    let mut patch = git(dir, &["diff", "HEAD", "--no-color", "--no-ext-diff", "-M"])
        .or_else(|_| git(dir, &["diff", "--no-color", "--no-ext-diff", "-M"]))?;
    let new: Vec<String> = changes
        .iter()
        .filter(|c| c.untracked())
        .map(|c| c.path.clone())
        .collect();
    patch.push_str(&untracked_patches(dir, &new));
    Ok(cap(patch))
}

pub fn stage(dir: &Path, paths: &[String]) -> GitResult<()> {
    let mut args = vec!["add", "-A", "--"];
    args.extend(paths.iter().map(String::as_str));
    git(dir, &args).map(drop)
}

pub fn unstage(dir: &Path, paths: &[String]) -> GitResult<()> {
    let mut args = vec!["restore", "--staged", "--"];
    args.extend(paths.iter().map(String::as_str));
    // No HEAD yet: `restore --staged` has nothing to restore from.
    git(dir, &args).map(drop).or_else(|_| {
        let mut args = vec!["rm", "--cached", "-q", "-r", "--"];
        args.extend(paths.iter().map(String::as_str));
        git(dir, &args).map(drop)
    })
}

/// Drop a file's changes, staged and unstaged. A new file is deleted.
pub fn discard(dir: &Path, change: &Change) -> GitResult<()> {
    if change.untracked() || change.index == 'A' {
        let _ = git(
            dir,
            &[
                "rm",
                "--cached",
                "-q",
                "--ignore-unmatch",
                "--",
                &change.path,
            ],
        );
        return std::fs::remove_file(dir.join(&change.path))
            .or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })
            .map_err(|e| format!("Couldn't delete {}: {e}", change.path));
    }
    git(
        dir,
        &[
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            &change.path,
        ],
    )
    .map(drop)
}

/// Commit the staged files, or every change when nothing is staged.
pub fn commit(dir: &Path, message: &str) -> GitResult<String> {
    let staged = status(dir)?.iter().any(Change::staged);
    if !staged {
        git(dir, &["add", "-A"])?;
    }
    git(dir, &["commit", "-q", "-m", message])?;
    git(dir, &["rev-parse", "--short", "HEAD"]).map(|s| s.trim().to_string())
}

/// Push, setting the upstream on a branch's first push.
pub fn push(dir: &Path) -> GitResult<()> {
    if git(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_ok() {
        git(dir, &["push", "-q"]).map(drop)
    } else {
        git(dir, &["push", "-q", "-u", "origin", "HEAD"]).map(drop)
    }
}

pub fn pull(dir: &Path) -> GitResult<()> {
    git(dir, &["pull", "-q", "--ff-only"]).map(drop)
}

/// One commit of the history graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub refs: Vec<String>,
    pub subject: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        self.sha.get(..7).unwrap_or(&self.sha)
    }
}

pub fn log(dir: &Path, limit: usize) -> GitResult<Vec<Commit>> {
    let limit = format!("-n{limit}");
    let out = git(
        dir,
        &[
            "log",
            "--all",
            "--topo-order",
            &limit,
            "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s%x1e",
        ],
    )?;
    Ok(out
        .split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').split('\x1f');
            let sha = f.next()?.trim().to_string();
            if sha.is_empty() {
                return None;
            }
            Some(Commit {
                sha,
                parents: f.next()?.split_whitespace().map(str::to_string).collect(),
                author: f.next()?.to_string(),
                time: f.next()?.parse().unwrap_or(0),
                refs: f
                    .next()?
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(|r| r.trim_start_matches("HEAD -> ").to_string())
                    .collect(),
                subject: f.next().unwrap_or_default().trim_end().to_string(),
            })
        })
        .collect())
}

/// One commit's own changes; a merge shows what it brought to its first parent.
pub fn show(dir: &Path, sha: &str) -> GitResult<String> {
    git(
        dir,
        &[
            "show",
            "--format=",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--diff-merges=first-parent",
            sha,
        ],
    )
    .or_else(|_| {
        git(
            dir,
            &["show", "--format=", "--no-color", "--no-ext-diff", sha],
        )
    })
    .map(cap)
}

/// The full message of a commit, for the detail header.
pub fn message(dir: &Path, sha: &str) -> GitResult<String> {
    git(dir, &["show", "-s", "--format=%B", sha]).map(|s| s.trim().to_string())
}

/// The working tree as it stood when a turn began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// A commit whose tree is the working tree at the time (or HEAD when clean).
    pub tree: String,
    /// Untracked files then, so files the turn created can be told apart.
    pub untracked: HashSet<String>,
}

pub fn checkpoint(dir: &Path) -> GitResult<Checkpoint> {
    let stash = git(dir, &["stash", "create"])?.trim().to_string();
    let tree = if stash.is_empty() {
        git(dir, &["rev-parse", "HEAD"])?.trim().to_string()
    } else {
        stash
    };
    Ok(Checkpoint {
        tree,
        untracked: untracked(dir),
    })
}

fn untracked(dir: &Path) -> HashSet<String> {
    git(dir, &["ls-files", "--others", "--exclude-standard", "-z"])
        .map(|s| {
            s.split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Files the turn created: untracked now, not untracked before.
fn created(dir: &Path, cp: &Checkpoint) -> Vec<String> {
    let mut new: Vec<String> = untracked(dir)
        .into_iter()
        .filter(|p| !cp.untracked.contains(p))
        .collect();
    new.sort();
    new
}

/// What changed since the checkpoint: tracked files against its tree, plus
/// the files the turn created.
pub fn turn_diff(dir: &Path, cp: &Checkpoint) -> GitResult<String> {
    let mut patch = git(
        dir,
        &["diff", "--no-color", "--no-ext-diff", "-M", &cp.tree],
    )?;
    patch.push_str(&untracked_patches(dir, &created(dir, cp)));
    Ok(cap(patch))
}

/// Put every file the turn touched back as it was at the checkpoint.
pub fn undo_turn(dir: &Path, cp: &Checkpoint) -> GitResult<usize> {
    let touched = git(dir, &["diff", "--name-only", "-z", &cp.tree])?;
    let mut count = 0;
    for path in touched.split('\0').filter(|p| !p.is_empty()) {
        let existed = git(dir, &["cat-file", "-e", &format!("{}:{path}", cp.tree)]).is_ok();
        if existed {
            git(
                dir,
                &["restore", "--source", &cp.tree, "--worktree", "--", path],
            )?;
        } else {
            let _ = git(
                dir,
                &["rm", "--cached", "-q", "--ignore-unmatch", "--", path],
            );
            let _ = std::fs::remove_file(dir.join(path));
        }
        count += 1;
    }
    for path in created(dir, cp) {
        if std::fs::remove_file(dir.join(&path)).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_renames_and_untracked() {
        let out = "R  new.rs\0old.rs\0 M src/a.rs\0?? notes.md\0";
        let changes = parse_status(out);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].path, "new.rs");
        assert_eq!(changes[0].from.as_deref(), Some("old.rs"));
        assert!(changes[0].staged() && !changes[0].unstaged());
        assert!(changes[1].unstaged() && !changes[1].staged());
        assert_eq!(changes[2].letter(), 'U');
    }
}
