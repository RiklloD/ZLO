//! One script container's lifecycle, from the moment `docker run` is spawned.
//!
//! [`ContainerGuard`] owns the container's name, its lane slot, and the CLI
//! child process. However the execution ends — normal exit, error, timeout,
//! or a panic unwinding through the backend — its `Drop` settles both halves:
//! the CLI child is waited (killed first if still running), and the container
//! is verified gone. Only when both are confirmed is the slot released;
//! otherwise the container, the unwaited child, and the slot go to the
//! [`OrphanReaper`], which keeps retrying and releases the slot only once
//! both are settled. Capacity is therefore never re-admitted over a container
//! that may still be running, and no CLI child is left as a zombie.

use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use super::ScriptExecutionSlot;
use super::docker::{DockerCli, ScriptChild, TeardownBounds};
use super::orphans::{Orphan, OrphanReaper, remove_verified};

/// How often the guard polls for `--rm` to finish removing an exited
/// container before falling back to `docker rm -f`.
const REMOVAL_POLL: Duration = Duration::from_millis(50);

pub(crate) struct ContainerGuard {
    container_name: String,
    slot: Option<ScriptExecutionSlot>,
    child: Option<Box<dyn ScriptChild>>,
    child_waited: bool,
    removal_verified: bool,
    cli: Arc<dyn DockerCli>,
    reaper: Arc<OrphanReaper>,
    bounds: TeardownBounds,
}

impl ContainerGuard {
    pub(crate) fn new(
        container_name: String,
        slot: ScriptExecutionSlot,
        child: Box<dyn ScriptChild>,
        cli: Arc<dyn DockerCli>,
        reaper: Arc<OrphanReaper>,
        bounds: TeardownBounds,
    ) -> Self {
        Self {
            container_name,
            slot: Some(slot),
            child: Some(child),
            child_waited: false,
            removal_verified: false,
            cli,
            reaper,
            bounds,
        }
    }

    /// The CLI child, for taking its pipes. `None` only after `Drop` began.
    pub(crate) fn child(&mut self) -> Option<&mut (dyn ScriptChild + 'static)> {
        self.child.as_deref_mut()
    }

    /// Poll the child; an `Ok(Some(_))` means it has been waited.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let Some(child) = self.child.as_deref_mut() else {
            return Err(io::Error::other("script child already released"));
        };
        let status = child.try_wait();
        if matches!(status, Ok(Some(_))) {
            self.child_waited = true;
        }
        status
    }

    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match self.child.as_deref_mut() {
            Some(child) => child.kill(),
            None => Ok(()),
        }
    }

    /// Poll until the child is waited or `bound` elapses.
    pub(crate) fn wait_for_exit(&mut self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "polling a script's docker child failed");
                    return false;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Force-remove the container by name and record whether that was
    /// verified.
    pub(crate) fn force_remove(&mut self) -> bool {
        self.removal_verified = remove_verified(
            self.cli.as_ref(),
            &self.container_name,
            self.bounds.force_remove,
        );
        self.removal_verified
    }

    /// Settle both halves within the teardown bounds.
    fn settle(&mut self) {
        if !self.child_waited {
            if let Err(error) = self.kill() {
                tracing::debug!(%error, "killing a script's docker child failed");
            }
            self.wait_for_exit(self.bounds.reap);
        }
        if !self.removal_verified {
            self.removal_verified = self.await_removal();
        }
    }

    /// `--rm` removes the container shortly after it exits: give it until the
    /// reap bound, then force-remove.
    fn await_removal(&self) -> bool {
        let deadline = Instant::now() + self.bounds.reap;
        loop {
            match self
                .cli
                .container_exists(&self.container_name, self.bounds.force_remove)
            {
                Ok(false) => return true,
                Ok(true) => {}
                Err(error) => {
                    tracing::debug!(%error, "checking a script container's removal failed");
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(REMOVAL_POLL);
        }
        remove_verified(
            self.cli.as_ref(),
            &self.container_name,
            self.bounds.force_remove,
        )
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        // A panic while settling (we may already be unwinding) must not abort
        // or skip the hand-off below: it counts as "not settled".
        if catch_unwind(AssertUnwindSafe(|| self.settle())).is_err() {
            tracing::debug!("settling a script container panicked; handing it to the reaper");
        }
        let Some(slot) = self.slot.take() else {
            return;
        };
        if self.child_waited && self.removal_verified {
            drop(slot);
            return;
        }
        self.reaper.adopt(Orphan {
            container_name: std::mem::take(&mut self.container_name),
            slot,
            child: if self.child_waited {
                None
            } else {
                self.child.take()
            },
            container_gone: self.removal_verified,
        });
    }
}
