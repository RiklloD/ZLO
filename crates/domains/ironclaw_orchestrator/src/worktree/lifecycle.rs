//! Which folder a thread works in, and the exclusion that keeps worktree
//! lifecycle steps (create, settle, merge, remove) from racing turns.
//!
//! A step reserves the worktree's owner thread under the engine's dispatch
//! lock, after checking that neither the owner nor any thread sharing its
//! worktree is running. Turn admission (`Engine::deliver_with_files`, under
//! the same lock) refuses to start a turn on a reserved worktree, so a turn
//! can never begin in a folder that is being merged or removed. Merges into
//! one checkout additionally run one at a time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use ironclaw_host_api::ids::ThreadId;

use crate::engine::{Engine, Result};
use crate::error::OrchestratorError;
use crate::model::ThreadRecord;

/// Holds a worktree's lifecycle reservation; released on drop.
pub(crate) struct WorktreeOp<'a> {
    reserved: &'a StdMutex<HashSet<ThreadId>>,
    owner: ThreadId,
}

impl Drop for WorktreeOp<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.reserved.lock() {
            set.remove(&self.owner);
        }
    }
}

/// Holds a checkout against turn admission during a merge into it.
pub(crate) struct CheckoutOp<'a> {
    reserved: &'a StdMutex<HashSet<PathBuf>>,
    dir: PathBuf,
}

impl Drop for CheckoutOp<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.reserved.lock() {
            set.remove(&self.dir);
        }
    }
}

/// `path` with symlinks resolved (`/var` vs `/private/var` on macOS), or
/// as given when it cannot be resolved.
async fn canonical(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf()) // silent-ok: a missing folder compares as written
}

fn busy(message: &str) -> OrchestratorError {
    OrchestratorError::request("thread_busy", message.to_string())
}

impl Engine {
    /// The folder a thread's agent works in: its worktree (own or shared),
    /// else its profile's folder, else its runtime's default folder.
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

    /// The worktree a thread's turns run in: its own while the folder
    /// exists, else the one it shares. A shared worktree that was removed
    /// is an error, never a silent fall back to the original checkout.
    pub(crate) async fn thread_work_dir(&self, thread: &ThreadId) -> Result<Option<PathBuf>> {
        if let Some(own) = self.store.worktree(thread).await?
            && own.state.has_folder()
        {
            return Ok(Some(own.path));
        }
        let Some(binding) = self.store.worktree_share(thread).await? else {
            return Ok(None);
        };
        // The exact worktree it was given, never the owner's later one.
        match self.store.worktree_by_id(binding).await? {
            Some(b) if b.state.has_folder() => Ok(Some(b.path)),
            _ => Err(OrchestratorError::request(
                "worktree_removed",
                "The worktree this thread shared with its parent was removed. Start a new \
                 delegation for further work.",
            )),
        }
    }

    /// The worktree instance `thread` works in (its own or the one it
    /// shares), while its folder exists.
    pub(crate) async fn worktree_of(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<super::WorktreeBinding>> {
        if let Some(own) = self.store.worktree(thread).await?
            && own.state.has_folder()
        {
            return Ok(Some(own));
        }
        let Some(binding) = self.store.worktree_share(thread).await? else {
            return Ok(None);
        };
        Ok(self
            .store
            .worktree_by_id(binding)
            .await?
            .filter(|b| b.state.has_folder()))
    }

    /// The thread whose worktree `thread` works in (itself or the one it
    /// shares), while that worktree exists.
    pub(crate) async fn worktree_owner(&self, thread: &ThreadId) -> Result<Option<ThreadId>> {
        if let Some(own) = self.store.worktree(thread).await?
            && own.state.has_folder()
        {
            return Ok(Some(thread.clone()));
        }
        let Some(binding) = self.store.worktree_share(thread).await? else {
            return Ok(None);
        };
        Ok(self
            .store
            .worktree_by_id(binding)
            .await?
            .filter(|b| b.state.has_folder())
            .map(|b| b.thread_id))
    }

    /// A lifecycle step holds `thread`'s worktree (turn admission refuses).
    pub(crate) async fn worktree_reserved(&self, thread: &ThreadId) -> Result<bool> {
        let owner = self.worktree_owner(thread).await?;
        let set = self
            .worktree_ops
            .lock()
            .map_err(|e| OrchestratorError::Store(format!("worktree reservations: {e}")))?;
        Ok(set.contains(thread) || owner.is_some_and(|o| set.contains(&o)))
    }

    /// Reserve `owner`'s worktree for one lifecycle step: under the dispatch
    /// lock, refuse when the owner or a thread sharing its worktree runs or
    /// another step holds it. Turns stay out until the guard drops.
    pub(crate) async fn reserve_worktree(&self, owner: &ThreadId) -> Result<WorktreeOp<'_>> {
        let _admission = self.dispatch.lock().await;
        let mut threads = vec![owner.clone()];
        if let Some(binding) = self.store.worktree(owner).await? {
            threads.extend(self.store.worktree_sharers(binding.id).await?);
        }
        for id in &threads {
            if self.store.thread(id).await?.is_some_and(|t| t.is_running()) {
                return Err(busy(&format!(
                    "Thread {id} is running in this worktree; wait for it to finish (or cancel it)."
                )));
            }
        }
        let mut set = self
            .worktree_ops
            .lock()
            .map_err(|e| OrchestratorError::Store(format!("worktree reservations: {e}")))?;
        if !set.insert(owner.clone()) {
            return Err(busy(
                "Another worktree operation on this thread is running; try again when it ends.",
            ));
        }
        Ok(WorktreeOp {
            reserved: &self.worktree_ops,
            owner: owner.clone(),
        })
    }

    /// Hold the checkout at `target` (a merge into it): no turn starts in a
    /// folder inside it until the guard drops, and the threads known to
    /// work there — the worktree's owner and its sharers when `target` is a
    /// worktree — must not be running, `caller` (the thread asking for the
    /// merge, already mid-turn) excepted.
    pub(crate) async fn reserve_checkout(
        &self,
        target: &Path,
        caller: Option<&ThreadId>,
    ) -> Result<CheckoutOp<'_>> {
        let target = canonical(target).await;
        let _admission = self.dispatch.lock().await;
        for binding in self.store.live_worktrees().await? {
            if canonical(&binding.path).await != target {
                continue;
            }
            let mut threads = vec![binding.thread_id.clone()];
            threads.extend(self.store.worktree_sharers(binding.id).await?);
            for id in threads.iter().filter(|id| Some(*id) != caller) {
                if self.store.thread(id).await?.is_some_and(|t| t.is_running()) {
                    return Err(busy(&format!(
                        "Thread {id} is running in {}; wait for it to finish before merging into it.",
                        target.display()
                    )));
                }
            }
        }
        let mut dirs = self
            .reserved_dirs
            .lock()
            .map_err(|e| OrchestratorError::Store(format!("checkout reservations: {e}")))?;
        if !dirs.insert(target.clone()) {
            return Err(busy(
                "Another merge into this checkout is running; try again when it ends.",
            ));
        }
        Ok(CheckoutOp {
            reserved: &self.reserved_dirs,
            dir: target,
        })
    }

    /// A merge holds the checkout `cwd` is in (turn admission waits).
    pub(crate) async fn checkout_reserved(&self, cwd: &Path) -> Result<bool> {
        let dirs: Vec<PathBuf> = {
            let set = self
                .reserved_dirs
                .lock()
                .map_err(|e| OrchestratorError::Store(format!("checkout reservations: {e}")))?;
            if set.is_empty() {
                return Ok(false);
            }
            set.iter().cloned().collect()
        };
        let cwd = canonical(cwd).await;
        Ok(dirs.iter().any(|d| cwd.starts_with(d)))
    }

    /// One merge at a time into the checkout at `target`.
    pub(crate) async fn checkout_lock(
        &self,
        target: &Path,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let mut locks = self
                .checkout_locks
                .lock()
                .map_err(|e| OrchestratorError::Store(format!("checkout locks: {e}")))?;
            Arc::clone(locks.entry(target.to_path_buf()).or_default())
        };
        Ok(lock.lock_owned().await)
    }
}
