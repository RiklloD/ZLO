//! Worktree isolation: a delegated child (or a user thread) runs in its own
//! `git worktree` on a fresh `zlo/<slug>-<id>` branch, so parallel
//! code-editing agents never edit the same folder.
//!
//! - **Create** — `t3_delegate isolation='worktree'` (or the thread route)
//!   runs `git worktree add -b <branch> <root>/<repo-hash>/<slug>-<id> <HEAD>`
//!   from the parent thread's working folder. Only committed work is in the
//!   new tree: uncommitted parent edits stay where they are.
//! - **Bind** — the [`WorktreeBinding`] is recorded in `orch_worktrees`
//!   (never deleted; state moves forward) and every turn of the thread gets
//!   it as [`crate::runtime::TurnInput::work_dir`].
//!   A `shared` child of a worktree thread works in that same worktree
//!   (`orch_worktree_shares`), without owning its cleanup.
//! - **Setup** — `.zlo/worktree.toml` hooks are read from the source
//!   checkout once, snapshotted on the binding, and run only after the
//!   person approved that exact content ([`HookApprover`]); `on_settle`
//!   runs the approved snapshot, never the child-editable copy.
//! - **Settle** — when the child's task ends: a clean tree with no commits,
//!   still on its branch, is removed with its branch; anything else is kept.
//! - **Merge / remove** — `t3_thread action=merge_worktree|remove_worktree`.
//!   Nothing is ever forced and nothing is committed for anyone: a worktree
//!   with uncommitted changes, or whose HEAD left its branch with work on
//!   it, is refused; a failed merge is rolled back and verified; unmerged
//!   commits keep their branch plus a `refs/zlo/archive/<branch>` ref.
//! - **Exclusion** — every lifecycle step holds a reservation taken under
//!   the engine's dispatch lock, so no turn starts in a worktree being
//!   merged or removed; merges into one checkout run one at a time.
//!
//! Every git and setup command runs through [`HostCommandRunner`], the
//! host's process runtime (composition wires it); the engine never spawns.
//! Git runs with the repository's hooks switched off.

mod config;
mod git;
mod hooks;
mod integrate;
mod lifecycle;
mod ops;
mod store;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::model::{Millis, TaskId};

pub(crate) use config::WorktreeConfig;
pub(crate) use ops::{WorktreePlan, runtime_refused};
pub(crate) use store::WORKTREES_SCHEMA;

/// One host command: a `sh -c` line run in `workdir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCommand {
    pub line: String,
    pub workdir: PathBuf,
    pub timeout: Duration,
}

/// What a host command printed (stdout and stderr) and how it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCommandOutput {
    pub exit_code: i64,
    pub output: String,
}

impl HostCommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Runs host commands for the engine (git, worktree setup scripts).
/// Implemented by composition over the host process runtime; a command that
/// ran and exited non-zero is `Ok` with its exit code, `Err` means it could
/// not run at all.
#[async_trait]
pub trait HostCommandRunner: Send + Sync {
    async fn run(&self, command: HostCommand) -> Result<HostCommandOutput, OrchestratorError>;
}

/// One request to run a project's worktree setup: exactly what would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookApproval {
    /// The thread whose person is asked (the delegating parent).
    pub thread: ThreadId,
    pub repo_root: PathBuf,
    /// The commands of both hooks, one `$ line` each, as the gate shows them.
    pub script: String,
    /// SHA-256 of the exact hook content; an approval covers only it.
    pub content_hash: String,
}

/// The person's answer to a [`HookApproval`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Approved,
    Denied(String),
    /// Nobody can be asked right now (no live turn to show the gate in).
    Unavailable,
}

/// Asks the person before a repository's setup commands run, through the
/// same approval gate as any shell command an agent runs. Implemented by
/// the CLI bridge; approved content is remembered per repository and hash.
#[async_trait]
pub trait HookApprover: Send + Sync {
    async fn approve(&self, request: &HookApproval) -> HookDecision;
}

/// Where worktrees live and how their commands run, plugged in by the host.
#[derive(Clone)]
pub struct WorktreeHost {
    pub runner: Arc<dyn HostCommandRunner>,
    /// `<app-data>/worktrees`; each repository gets a hashed subfolder.
    pub root: PathBuf,
    pub approver: Arc<dyn HookApprover>,
}

/// A project's setup hooks as read from the source checkout when the
/// worktree was made. Settle runs this snapshot, never a newer file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeHooks {
    pub on_create: Vec<String>,
    pub on_settle: Vec<String>,
    pub timeout_s: u64,
    /// SHA-256 over the repository and the content above.
    pub hash: String,
}

/// `t3_delegate isolation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    /// The child works in the parent's folder (the default).
    #[default]
    Shared,
    /// The child gets its own git worktree and branch.
    Worktree,
}

impl Isolation {
    pub fn parse(raw: Option<&str>) -> Result<Self, OrchestratorError> {
        match raw.map(str::trim) {
            None | Some("") | Some("shared") => Ok(Self::Shared),
            Some("worktree") => Ok(Self::Worktree),
            Some(_) => Err(OrchestratorError::invalid(
                "isolation must be shared or worktree",
            )),
        }
    }
}

/// How `merge_worktree` brings the branch into the source checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeStrategy {
    /// `git merge --no-ff`: one merge commit keeps the branch history.
    #[default]
    Merge,
    /// `git merge --squash` then one commit.
    Squash,
    /// `git cherry-pick` of the branch's own commits.
    CherryPick,
}

impl MergeStrategy {
    pub fn parse(raw: Option<&str>) -> Result<Self, OrchestratorError> {
        match raw.map(str::trim) {
            None | Some("") | Some("merge") => Ok(Self::Merge),
            Some("squash") => Ok(Self::Squash),
            Some("cherry_pick") | Some("cherry-pick") => Ok(Self::CherryPick),
            Some(_) => Err(OrchestratorError::invalid(
                "strategy must be merge, squash or cherry_pick",
            )),
        }
    }
}

/// What `merge_worktree` does when git stops on conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// Abort the merge: the source checkout is as it was.
    #[default]
    Abort,
    /// Leave the conflicted merge in the source checkout to resolve there.
    Leave,
}

impl OnConflict {
    pub fn parse(raw: Option<&str>) -> Result<Self, OrchestratorError> {
        match raw.map(str::trim) {
            None | Some("") | Some("abort") => Ok(Self::Abort),
            Some("leave") => Ok(Self::Leave),
            Some(_) => Err(OrchestratorError::invalid(
                "on_conflict must be abort or leave",
            )),
        }
    }
}

/// A branch ZLO created for a worktree (`zlo/<slug>-<id>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct WorktreeBranch(String);

impl WorktreeBranch {
    fn validate(s: &str) -> Result<(), OrchestratorError> {
        let ok = !s.is_empty()
            && s.len() <= 120
            && !s.starts_with(['-', '/', '.'])
            && !s.ends_with(['/', '.'])
            && !s.ends_with(".lock")
            && !s.contains("..")
            && !s.contains("//")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'));
        if ok {
            Ok(())
        } else {
            Err(OrchestratorError::Corrupt(format!(
                "invalid worktree branch {s:?}"
            )))
        }
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, OrchestratorError> {
        let s = raw.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The ref a removed worktree's unmerged commits stay reachable from.
    pub fn archive_ref(&self) -> String {
        format!("refs/zlo/archive/{}", self.0)
    }
}

impl TryFrom<String> for WorktreeBranch {
    type Error = OrchestratorError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl fmt::Display for WorktreeBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A full commit id as git prints it (SHA-1 or SHA-256 hex).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct CommitId(String);

impl CommitId {
    fn validate(s: &str) -> Result<(), OrchestratorError> {
        if (7..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(())
        } else {
            Err(OrchestratorError::request(
                "git_failed",
                format!("git printed {s:?} where a commit id was expected"),
            ))
        }
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, OrchestratorError> {
        let s = raw.into().trim().to_ascii_lowercase();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> &str {
        self.0.get(..10).unwrap_or(&self.0)
    }
}

impl TryFrom<String> for CommitId {
    type Error = OrchestratorError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a worktree is in its life. Moves forward only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    /// The thread works in it.
    Active,
    /// The child finished with changes; kept for the parent to merge.
    Kept,
    /// Merged into the source checkout; the folder is still there.
    Merged,
    /// The folder is gone (the branch or archive ref keeps any work).
    Removed,
}

impl WorktreeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Kept => "kept",
            Self::Merged => "merged",
            Self::Removed => "removed",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, OrchestratorError> {
        Ok(match raw {
            "active" => Self::Active,
            "kept" => Self::Kept,
            "merged" => Self::Merged,
            "removed" => Self::Removed,
            other => {
                return Err(OrchestratorError::Corrupt(format!(
                    "unknown worktree state {other:?}"
                )));
            }
        })
    }

    /// The folder still exists, so turns run in it.
    pub fn has_folder(self) -> bool {
        self != Self::Removed
    }
}

/// A thread's git worktree, as recorded in `orch_worktrees`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeBinding {
    pub thread_id: ThreadId,
    /// The worktree folder; the thread's turns run here.
    pub path: PathBuf,
    pub branch: WorktreeBranch,
    /// The commit the branch started from (the source checkout's HEAD).
    pub base_commit: CommitId,
    /// Top folder of the repository the worktree belongs to.
    pub repo_root: PathBuf,
    /// The checkout it was made from; merges land there.
    pub source_dir: PathBuf,
    pub parent_thread: Option<ThreadId>,
    pub task_id: Option<TaskId>,
    pub state: WorktreeState,
    /// Set when the folder was removed while commits were unmerged.
    pub archive_ref: Option<String>,
    /// Output of the project's setup commands (bounded tail).
    pub setup_log: Option<String>,
    /// The setup hooks snapshot, when the project has any.
    pub hooks: Option<WorktreeHooks>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

/// A folder- and branch-safe slug: lowercase ASCII words joined by `-`.
pub(crate) fn slug(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 32 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

/// A stable folder name for a repository: the first 12 hex digits of the
/// SHA-256 of its top folder path.
pub(crate) fn repo_hash(repo_root: &std::path::Path) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(repo_root.to_string_lossy().as_bytes());
    hex::encode(digest).chars().take(12).collect()
}

/// Keep the last `max` bytes of `text`, on a char boundary.
pub(crate) fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}
