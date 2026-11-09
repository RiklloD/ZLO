//! A thread's repository and console on the backend's machine, for a client
//! on another computer (ZLO remote environments, phase 7): git status,
//! diffs, history, stage/unstage/discard and the last turn's undo, plus one
//! console command at a time in the thread's folder.
//!
//! Operator-only. The repository is always the one the thread works in on
//! the backend (its worktree, its profile's folder, or its runtime's
//! folder); a request never names a folder, and the file paths it names are
//! repository-relative and checked before git sees them. Outputs are
//! bounded by the limits below. The console exists only where the agent
//! itself gets a host shell (a single-user local deployment); elsewhere its
//! view says `available: false` and a start is refused.
//!
//! Correctable failures (no repository, a refused path, a busy console)
//! come back as `{"ok": false, "error": <code>, "message": ...}` with HTTP
//! 200, like the other orchestrator documents; a thread the caller does not
//! own is a 404. The behavior lives in `ironclaw_orchestrator` (`repo`,
//! `console`) behind `ironclaw_assistant`'s orchestration decorator.

use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};

/// Bytes of patch text one diff or commit view returns; the rest is cut at
/// a line boundary and `truncated` is set.
pub const GIT_PATCH_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Changed paths one status view lists.
pub const GIT_STATUS_MAX_ENTRIES: usize = 2_000;
/// Untracked files a working diff expands into patches (the rest are listed only).
pub const GIT_UNTRACKED_DIFF_MAX: usize = 60;
/// Commits one log view returns (the default is 200).
pub const GIT_LOG_MAX: u32 = 500;
/// Paths one stage/unstage request may name.
pub const GIT_PATHS_MAX: usize = 500;
/// Longest repository-relative path accepted.
pub const GIT_PATH_MAX_BYTES: usize = 4_096;
/// Longest console command accepted.
pub const CONSOLE_COMMAND_MAX_BYTES: usize = 8 * 1024;
/// Lines the console keeps; older ones are dropped first.
pub const CONSOLE_LINES_MAX: usize = 4_000;
/// Longest console line kept; longer lines are cut.
pub const CONSOLE_LINE_MAX_BYTES: usize = 4 * 1024;
/// Lines one console view returns after its cursor.
pub const CONSOLE_PAGE_LINES: usize = 1_000;

/// Which changes a diff shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    /// Everything not committed (HEAD against the working tree) plus new files.
    #[default]
    Working,
    /// The index against HEAD.
    Staged,
    /// The working tree against the index, plus new files.
    Unstaged,
    /// What changed since the thread's last checkpoint (see
    /// [`ThreadGitAction::Checkpoint`]), plus the files created since.
    Turn,
}

/// One read of the thread's repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadGitRead {
    /// Branch, ahead/behind and changed paths → [`GitStatusView`].
    Status {},
    /// A unified patch, whole or for one `path` → [`GitPatchView`].
    Diff {
        #[serde(default)]
        scope: GitDiffScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Recent commits of every branch, topo-ordered → [`GitLogView`].
    Log {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// One commit's message and own changes → [`GitCommitView`].
    Commit { sha: String },
}

/// Unknown fields are refused by the flattened enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGitQuery {
    pub thread_id: String,
    #[serde(flatten)]
    pub read: ThreadGitRead,
}

/// One change to the thread's repository. Nothing is ever forced and nothing
/// is committed, pushed or fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadGitAction {
    Stage {
        paths: Vec<String>,
    },
    Unstage {
        paths: Vec<String>,
    },
    /// Drop one file's staged and unstaged changes; a new file is deleted.
    Discard {
        path: String,
    },
    /// Record the working tree now (before a turn) for [`GitDiffScope::Turn`]
    /// and [`Self::UndoTurn`]; replaces the previous checkpoint.
    Checkpoint {},
    /// Put every file changed since the checkpoint back as it was then and
    /// delete the files created since; the checkpoint stays.
    UndoTurn {},
    /// Forget the checkpoint (the last turn's changes are kept).
    KeepTurn {},
}

/// Unknown fields are refused by the flattened enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGitChange {
    pub thread_id: String,
    #[serde(flatten)]
    pub action: ThreadGitAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitBranch {
    /// The branch, or `HEAD` when detached.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// One changed path; `index` and `worktree` are porcelain v1's X and Y.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitChange {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub index: char,
    pub worktree: char,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusView {
    /// The repository's top folder on the backend's machine.
    pub repo_root: String,
    pub branch: GitBranch,
    pub changes: Vec<GitChange>,
    /// More than [`GIT_STATUS_MAX_ENTRIES`] paths changed.
    pub truncated: bool,
    /// A checkpoint exists, so [`GitDiffScope::Turn`] and undo work.
    pub checkpoint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPatchView {
    pub patch: String,
    /// Cut at [`GIT_PATCH_MAX_BYTES`], or new files past
    /// [`GIT_UNTRACKED_DIFF_MAX`] were left out.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub refs: Vec<String>,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLogView {
    pub commits: Vec<GitCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommitView {
    pub sha: String,
    pub message: String,
    pub patch: String,
    pub truncated: bool,
}

/// A change's read-back: the repository's status after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitChangeResult {
    pub status: GitStatusView,
    /// What happened, for the person ("Restored 3 files.").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadConsoleQuery {
    pub thread_id: String,
    /// Lines with a sequence number above this; all kept lines when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadConsoleAction {
    /// Run `command` with `sh -c` in the thread's folder. Refused while
    /// another command of the thread runs.
    Start { command: String },
    /// Kill the running command and everything it started.
    Cancel {},
    /// Drop the kept lines (the console's `clear`).
    Clear {},
}

/// Unknown fields are refused by the flattened enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadConsoleControl {
    pub thread_id: String,
    #[serde(flatten)]
    pub action: ThreadConsoleAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleLineKind {
    /// The command as typed.
    Command,
    Out,
    Err,
    /// The console's own notes (exit code, stopped, cut).
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleLine {
    pub seq: u64,
    pub kind: ConsoleLineKind,
    pub text: String,
}

/// The thread's console: the kept lines after the cursor and the command's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadConsoleView {
    /// Whether this backend runs console commands at all.
    pub available: bool,
    /// The folder commands run in, when the thread has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub running: bool,
    /// The running (or last) command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Exit code of the last command; `None` while running or when killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub lines: Vec<ConsoleLine>,
    /// The cursor for the next view (the last sequence number sent).
    pub next: u64,
    /// Lines after the asked cursor that were already dropped.
    pub dropped: u64,
}

/// `{status, note?}` or the error document.
pub const THREAD_GIT_VIEW: ProductView<ThreadGitQuery, serde_json::Value> =
    ProductView::unpaginated("orchestrator.thread_git");
pub const THREAD_GIT_CHANGE_COMMAND_ID: &str = "orchestrator.thread_git_change";
pub const THREAD_GIT_CHANGE_COMMAND: ProductSurfaceCommandDescriptor<
    ThreadGitChange,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(THREAD_GIT_CHANGE_COMMAND_ID);
/// [`ThreadConsoleView`].
pub const THREAD_CONSOLE_VIEW: ProductView<ThreadConsoleQuery, serde_json::Value> =
    ProductView::unpaginated("orchestrator.thread_console");
pub const THREAD_CONSOLE_CONTROL_COMMAND_ID: &str = "orchestrator.thread_console_control";
/// The [`ThreadConsoleView`] after the action, or the error document.
pub const THREAD_CONSOLE_CONTROL_COMMAND: ProductSurfaceCommandDescriptor<
    ThreadConsoleControl,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(THREAD_CONSOLE_CONTROL_COMMAND_ID);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_changes_use_a_tagged_flat_wire() {
        let query: ThreadGitQuery = serde_json::from_value(serde_json::json!({
            "thread_id": "t", "view": "diff", "scope": "staged", "path": "src/a.rs"
        }))
        .unwrap();
        assert_eq!(
            query.read,
            ThreadGitRead::Diff {
                scope: GitDiffScope::Staged,
                path: Some("src/a.rs".into())
            }
        );
        let change: ThreadGitChange = serde_json::from_value(serde_json::json!({
            "thread_id": "t", "action": "stage", "paths": ["a"]
        }))
        .unwrap();
        assert_eq!(
            change.action,
            ThreadGitAction::Stage {
                paths: vec!["a".into()]
            }
        );
        let console: ThreadConsoleControl = serde_json::from_value(serde_json::json!({
            "thread_id": "t", "action": "start", "command": "ls"
        }))
        .unwrap();
        assert_eq!(
            console.action,
            ThreadConsoleAction::Start {
                command: "ls".into()
            }
        );
        assert!(
            serde_json::from_value::<ThreadGitChange>(serde_json::json!({
                "thread_id": "t", "action": "stage", "paths": [], "repo": "/etc"
            }))
            .is_err(),
            "a request cannot name a folder"
        );
        // Field-less actions refuse extra fields too.
        assert!(
            serde_json::from_value::<ThreadGitChange>(serde_json::json!({
                "thread_id": "t", "action": "checkpoint", "repo": "/"
            }))
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(ThreadGitChange {
                thread_id: "t".into(),
                action: ThreadGitAction::Checkpoint {},
            })
            .unwrap(),
            serde_json::json!({"thread_id": "t", "action": "checkpoint"})
        );
    }
}
