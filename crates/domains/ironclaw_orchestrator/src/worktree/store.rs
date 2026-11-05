//! `orch_worktrees`: one row per worktree a thread was given. Rows are never
//! deleted; a thread's current binding is its newest row.

use std::path::{Path, PathBuf};

use ironclaw_host_api::ids::ThreadId;
use libsql::Value;

use super::{CommitId, WorktreeBinding, WorktreeBranch, WorktreeState};
use crate::error::OrchestratorError;
use crate::model::{Millis, TaskId};
use crate::store::{Cols, Store, opt, s};

type Result<T> = std::result::Result<T, OrchestratorError>;

pub(crate) const WORKTREES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_worktrees (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  thread_id TEXT NOT NULL,
  path TEXT NOT NULL,
  branch TEXT NOT NULL,
  base_commit TEXT NOT NULL,
  repo_root TEXT NOT NULL,
  source_dir TEXT NOT NULL,
  parent_thread TEXT,
  task_id TEXT,
  state TEXT NOT NULL,
  archive_ref TEXT,
  setup_log TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  hooks TEXT
);
CREATE INDEX IF NOT EXISTS orch_worktrees_thread ON orch_worktrees(thread_id, id);
CREATE TABLE IF NOT EXISTS orch_worktree_hook_trust (
  repo_root TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  approved_at INTEGER NOT NULL,
  PRIMARY KEY (repo_root, content_hash)
);
CREATE TABLE IF NOT EXISTS orch_worktree_shares (
  thread_id TEXT PRIMARY KEY,
  owner_thread TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS orch_worktree_shares_owner ON orch_worktree_shares(owner_thread);
"#;

const COLUMNS: &str = "thread_id, path, branch, base_commit, repo_root, source_dir, parent_thread, \
                       task_id, state, archive_ref, setup_log, created_at, updated_at, hooks";

/// Setup output kept per worktree.
const SETUP_LOG_MAX: usize = 8_000;

fn read(row: &libsql::Row) -> Result<WorktreeBinding> {
    let mut c = Cols::new(row);
    Ok(WorktreeBinding {
        thread_id: ThreadId::new(c.text()?)
            .map_err(|e| OrchestratorError::Corrupt(e.to_string()))?,
        path: PathBuf::from(c.text()?),
        branch: WorktreeBranch::new(c.text()?)?,
        base_commit: CommitId::new(c.text()?)
            .map_err(|e| OrchestratorError::Corrupt(e.to_string()))?,
        repo_root: PathBuf::from(c.text()?),
        source_dir: PathBuf::from(c.text()?),
        parent_thread: c
            .otext()?
            .map(ThreadId::new)
            .transpose()
            .map_err(|e| OrchestratorError::Corrupt(e.to_string()))?,
        task_id: c.otext()?.map(TaskId::new).transpose()?,
        state: WorktreeState::parse(&c.text()?)?,
        archive_ref: c.otext()?,
        setup_log: c.otext()?,
        created_at: c.int()?,
        updated_at: c.int()?,
        hooks: c
            .otext()?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()?,
    })
}

impl Store {
    pub(crate) async fn insert_worktree(&self, b: &WorktreeBinding) -> Result<()> {
        self.exec(
            &format!(
                "INSERT INTO orch_worktrees ({COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            vec![
                s(b.thread_id.as_str()),
                s(b.path.to_string_lossy()),
                s(b.branch.as_str()),
                s(b.base_commit.as_str()),
                s(b.repo_root.to_string_lossy()),
                s(b.source_dir.to_string_lossy()),
                opt(b.parent_thread.as_ref().map(|t| t.as_str().to_string())),
                opt(b.task_id.as_ref().map(|t| t.as_str().to_string())),
                s(b.state.as_str()),
                opt(b.archive_ref.clone()),
                opt(b.setup_log.clone()),
                Value::Integer(b.created_at),
                Value::Integer(b.updated_at),
                match &b.hooks {
                    Some(hooks) => s(serde_json::to_string(hooks)?),
                    None => Value::Null,
                },
            ],
        )
        .await?;
        Ok(())
    }

    /// The thread's current worktree (its newest row), whatever its state.
    pub async fn worktree(&self, thread: &ThreadId) -> Result<Option<WorktreeBinding>> {
        self.row(
            &format!(
                "SELECT {COLUMNS} FROM orch_worktrees WHERE thread_id = ?1 ORDER BY id DESC LIMIT 1"
            ),
            vec![s(thread.as_str())],
            read,
        )
        .await
    }

    /// Move the thread's current worktree forward (`from` guards the move).
    pub(crate) async fn set_worktree_state(
        &self,
        thread: &ThreadId,
        from: &[WorktreeState],
        to: WorktreeState,
        archive_ref: Option<&str>,
        now: Millis,
    ) -> Result<bool> {
        let allowed: Vec<String> = from.iter().map(|s| format!("'{}'", s.as_str())).collect();
        let changed = self
            .exec(
                &format!(
                    "UPDATE orch_worktrees SET state = ?2, archive_ref = COALESCE(?3, archive_ref), \
                     updated_at = ?4 WHERE id = (SELECT MAX(id) FROM orch_worktrees WHERE thread_id = ?1) \
                     AND state IN ({})",
                    allowed.join(", ")
                ),
                vec![
                    s(thread.as_str()),
                    s(to.as_str()),
                    opt(archive_ref.map(str::to_string)),
                    Value::Integer(now),
                ],
            )
            .await?;
        Ok(changed > 0)
    }

    /// Append setup output to the thread's current worktree (bounded tail).
    pub(crate) async fn append_worktree_log(
        &self,
        thread: &ThreadId,
        text: &str,
        now: Millis,
    ) -> Result<()> {
        let current = self
            .worktree(thread)
            .await?
            .and_then(|b| b.setup_log)
            .unwrap_or_default();
        let joined = if current.is_empty() {
            text.to_string()
        } else {
            format!("{current}\n{text}")
        };
        self.exec(
            "UPDATE orch_worktrees SET setup_log = ?2, updated_at = ?3 \
             WHERE id = (SELECT MAX(id) FROM orch_worktrees WHERE thread_id = ?1)",
            vec![
                s(thread.as_str()),
                s(super::tail(&joined, SETUP_LOG_MAX)),
                Value::Integer(now),
            ],
        )
        .await?;
        Ok(())
    }

    /// Branch of every thread whose current worktree folder still exists,
    /// by thread id (inbox chips).
    pub async fn live_worktree_branches(
        &self,
    ) -> Result<std::collections::HashMap<String, String>> {
        let rows = self
            .rows(
                "SELECT w.thread_id, w.branch FROM orch_worktrees w \
                 WHERE w.state != 'removed' \
                 AND w.id = (SELECT MAX(id) FROM orch_worktrees x WHERE x.thread_id = w.thread_id)",
                vec![],
                |r| {
                    let mut c = Cols::new(r);
                    Ok((c.text()?, c.text()?))
                },
            )
            .await?;
        Ok(rows.into_iter().collect())
    }

    /// The person approved running this exact hook content for this repo.
    pub(crate) async fn hooks_trusted(&self, repo_root: &Path, hash: &str) -> Result<bool> {
        Ok(self
            .row(
                "SELECT 1 FROM orch_worktree_hook_trust WHERE repo_root = ?1 AND content_hash = ?2",
                vec![s(repo_root.to_string_lossy()), s(hash)],
                |r| Cols::new(r).int(),
            )
            .await?
            .is_some())
    }

    pub(crate) async fn trust_hooks(
        &self,
        repo_root: &Path,
        hash: &str,
        now: Millis,
    ) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO orch_worktree_hook_trust (repo_root, content_hash, approved_at) \
             VALUES (?1, ?2, ?3)",
            vec![s(repo_root.to_string_lossy()), s(hash), Value::Integer(now)],
        )
        .await?;
        Ok(())
    }

    /// `thread` works in `owner`'s worktree (a shared child of a worktree
    /// thread). First record wins: a thread's folder never changes owner.
    pub(crate) async fn share_worktree(
        &self,
        thread: &ThreadId,
        owner: &ThreadId,
        now: Millis,
    ) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO orch_worktree_shares (thread_id, owner_thread, created_at) \
             VALUES (?1, ?2, ?3)",
            vec![s(thread.as_str()), s(owner.as_str()), Value::Integer(now)],
        )
        .await?;
        Ok(())
    }

    /// The thread whose worktree `thread` shares, if any.
    pub(crate) async fn worktree_share(&self, thread: &ThreadId) -> Result<Option<ThreadId>> {
        self.row(
            "SELECT owner_thread FROM orch_worktree_shares WHERE thread_id = ?1",
            vec![s(thread.as_str())],
            read_thread,
        )
        .await
    }

    /// Threads working in `owner`'s worktree besides `owner` itself.
    pub(crate) async fn worktree_sharers(&self, owner: &ThreadId) -> Result<Vec<ThreadId>> {
        self.rows(
            "SELECT thread_id FROM orch_worktree_shares WHERE owner_thread = ?1",
            vec![s(owner.as_str())],
            read_thread,
        )
        .await
    }
}

fn read_thread(row: &libsql::Row) -> Result<ThreadId> {
    ThreadId::new(Cols::new(row).text()?).map_err(|e| OrchestratorError::Corrupt(e.to_string()))
}
