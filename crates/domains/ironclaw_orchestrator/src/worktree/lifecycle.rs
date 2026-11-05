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
        let Some(owner) = self.store.worktree_share(thread).await? else {
            return Ok(None);
        };
        match self.store.worktree(&owner).await? {
            Some(b) if b.state.has_folder() => Ok(Some(b.path)),
            _ => Err(OrchestratorError::request(
                "worktree_removed",
                format!(
                    "This thread worked in the worktree of thread {owner}, which was removed. \
                     Start a new delegation for further work."
                ),
            )),
        }
    }

    /// The thread whose worktree `thread` works in (itself or the one it
    /// shares), while that worktree exists.
    pub(crate) async fn worktree_owner(&self, thread: &ThreadId) -> Result<Option<ThreadId>> {
        if let Some(own) = self.store.worktree(thread).await?
            && own.state.has_folder()
        {
            return Ok(Some(thread.clone()));
        }
        self.store.worktree_share(thread).await
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
        threads.extend(self.store.worktree_sharers(owner).await?);
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
