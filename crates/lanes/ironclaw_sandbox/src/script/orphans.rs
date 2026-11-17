//! Script containers whose removal could not be verified.
//!
//! A timed-out script's container is force-removed by name. When that removal
//! cannot be verified, the container may still be running, so its lane slot
//! (the execution's concurrency unit) is not released: the [`OrphanReaper`]
//! takes the container, its slot, and any unwaited CLI child, retries with
//! capped backoff, and drops the slot only once `docker` confirms the
//! container is gone and the child has been waited.
//!
//! Every script container carries labels naming the lane, the process
//! instance that started it, the host, and that process's pid. At startup,
//! [`reap_previous_instances`] removes lane containers from earlier instances
//! on this host whose process is gone — never those of a live process,
//! including another IronClaw running beside this one.

use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::ScriptExecutionSlot;
use super::docker::{DockerCli, ScriptChild};

pub(crate) const LANE_LABEL_KEY: &str = "ironclaw.lane";
pub(crate) const LANE_LABEL_VALUE: &str = "script";
pub(crate) const INSTANCE_LABEL_KEY: &str = "ironclaw.instance";
pub(crate) const HOST_LABEL_KEY: &str = "ironclaw.host";
pub(crate) const PID_LABEL_KEY: &str = "ironclaw.pid";

/// Which process started a script container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptInstance {
    pub(crate) instance: String,
    pub(crate) host: String,
    pub(crate) pid: u32,
}

impl ScriptInstance {
    /// This process: a fresh instance id per process lifetime.
    pub(crate) fn current() -> ScriptInstance {
        static CURRENT: LazyLock<ScriptInstance> = LazyLock::new(|| ScriptInstance {
            instance: uuid::Uuid::new_v4().simple().to_string(),
            host: host_name(),
            pid: std::process::id(),
        });
        CURRENT.clone()
    }

    /// The `--label` values a container started by this instance carries.
    pub(crate) fn labels(&self) -> [String; 4] {
        [
            format!("{LANE_LABEL_KEY}={LANE_LABEL_VALUE}"),
            format!("{INSTANCE_LABEL_KEY}={}", self.instance),
            format!("{HOST_LABEL_KEY}={}", self.host),
            format!("{PID_LABEL_KEY}={}", self.pid),
        ]
    }
}

/// A lane-labelled container as `docker ps` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelledContainer {
    pub(crate) name: String,
    pub(crate) instance: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) pid: Option<u32>,
}

/// Bounds for orphan removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrphanBounds {
    /// Wait before each removal attempt; the last delay repeats.
    pub(crate) backoff: Vec<Duration>,
    /// Bound on each `docker rm -f` and each existence check.
    pub(crate) command: Duration,
}

impl OrphanBounds {
    pub(crate) fn default_bounds() -> Self {
        Self {
            backoff: [1, 2, 4, 8, 16, 30]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            command: Duration::from_secs(5),
        }
    }

    /// Wait before retry `attempt`: the backoff list, then its last (largest)
    /// value forever — the interval is capped, the retries are not.
    pub(crate) fn delay(&self, attempt: usize) -> Duration {
        self.backoff
            .get(attempt)
            .or(self.backoff.last())
            .copied()
            .unwrap_or(Duration::from_secs(1))
    }
}

/// What starts a background worker; injectable so a failed spawn can be
/// tested. Production uses [`thread_spawner`].
pub(crate) type WorkerSpawner =
    Arc<dyn Fn(&'static str, Box<dyn FnOnce() + Send>) -> std::io::Result<()> + Send + Sync>;

pub(crate) fn thread_spawner() -> WorkerSpawner {
    Arc::new(|name, work| {
        thread::Builder::new()
            .name(name.to_string())
            .spawn(work)
            .map(drop)
    })
}

/// Time left until `deadline` (zero once past).
pub(crate) fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Remove `name` and confirm it is gone, both within `deadline`. `false`
/// means "may still exist".
pub(crate) fn remove_verified(cli: &dyn DockerCli, name: &str, deadline: Instant) -> bool {
    if remaining(deadline).is_zero() {
        return false;
    }
    if let Err(error) = cli.force_remove(name, remaining(deadline)) {
        tracing::debug!(%error, "force-removing a script container failed");
    }
    if remaining(deadline).is_zero() {
        return false;
    }
    match cli.container_exists(name, remaining(deadline)) {
        Ok(exists) => !exists,
        Err(error) => {
            tracing::debug!(%error, "verifying a script container's removal failed");
            false
        }
    }
}

/// A script container (and possibly its unwaited CLI child) whose cleanup
/// is not yet confirmed, with the lane slot it keeps held.
pub(crate) struct Orphan {
    pub(crate) container_name: String,
    pub(crate) slot: ScriptExecutionSlot,
    /// The `docker run` CLI child if it has not been waited yet; the reaper
    /// owns it until it is, so it never lingers as a zombie.
    pub(crate) child: Option<Box<dyn ScriptChild>>,
    pub(crate) container_gone: bool,
}

/// An orphan as the reaper holds it: slot and child stay owned here for as
/// long as the orphan is unsettled, whether or not a worker is running.
pub(super) struct HeldOrphan {
    _slot: ScriptExecutionSlot,
    pub(super) child: Option<Box<dyn ScriptChild>>,
    container_gone: bool,
    attempt: usize,
    next_attempt: Instant,
}

pub(super) type HeldOrphans = BTreeMap<String, HeldOrphan>;

pub(super) struct ReaperState {
    held: Mutex<HeldOrphans>,
    worker_running: AtomicBool,
}

/// Holds orphaned containers — their lane slot and any unwaited CLI child —
/// until each container is verified gone and its child waited. One worker
/// thread retries every orphan with backoff that grows to a capped interval
/// and never gives up ownership; a panic in an attempt counts as a failed
/// attempt. If the worker cannot be started, the orphans stay held here and
/// the next adoption retries the spawn. Orphans are bounded by lane capacity,
/// since each holds a slot.
pub(crate) struct OrphanReaper {
    cli: Arc<dyn DockerCli>,
    bounds: OrphanBounds,
    pub(super) state: Arc<ReaperState>,
    spawner: WorkerSpawner,
}

impl OrphanReaper {
    pub(crate) fn new(
        cli: Arc<dyn DockerCli>,
        bounds: OrphanBounds,
        spawner: WorkerSpawner,
    ) -> Self {
        Self {
            cli,
            bounds,
            state: Arc::new(ReaperState {
                held: Mutex::new(BTreeMap::new()),
                worker_running: AtomicBool::new(false),
            }),
            spawner,
        }
    }

    /// Take ownership of an orphan. Its slot stays held (lane capacity stays
    /// reduced) until a retry confirms the container is gone and the child
    /// is waited.
    pub(crate) fn adopt(&self, orphan: Orphan) {
        let Orphan {
            container_name,
            slot,
            child,
            container_gone,
        } = orphan;
        lock_held(&self.state).insert(
            container_name,
            HeldOrphan {
                _slot: slot,
                child,
                container_gone,
                attempt: 0,
                next_attempt: Instant::now() + self.bounds.delay(0),
            },
        );
        self.ensure_worker();
    }

    fn ensure_worker(&self) {
        if self.state.worker_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(&self.state);
        let cli = Arc::clone(&self.cli);
        let bounds = self.bounds.clone();
        let spawned = (self.spawner)(
            "ironclaw-script-orphan-reaper",
            Box::new(move || {
                // A worker that dies anyway must not leave the flag set, or
                // no later adoption would start a new one.
                if catch_unwind(AssertUnwindSafe(|| {
                    run_reaper(&state, cli.as_ref(), &bounds)
                }))
                .is_err()
                {
                    state.worker_running.store(false, Ordering::SeqCst);
                }
            }),
        );
        if let Err(error) = spawned {
            // Slots and children stay held; the next adoption retries.
            self.state.worker_running.store(false, Ordering::SeqCst);
            tracing::debug!(%error, "spawning the script orphan reaper failed; orphans stay held");
        }
    }
}

pub(super) fn lock_held(state: &ReaperState) -> MutexGuard<'_, HeldOrphans> {
    state.held.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run_reaper(state: &ReaperState, cli: &dyn DockerCli, bounds: &OrphanBounds) {
    loop {
        let (due, next_wake) = {
            let held = lock_held(state);
            if held.is_empty() {
                // Cleared under the lock, so a concurrent `adopt` either sees
                // the worker still running or starts a new one.
                state.worker_running.store(false, Ordering::SeqCst);
                return;
            }
            let now = Instant::now();
            let due: Vec<String> = held
                .iter()
                .filter(|(_, orphan)| orphan.next_attempt <= now)
                .map(|(name, _)| name.clone())
                .collect();
            let next_wake = held
                .values()
                .map(|orphan| orphan.next_attempt)
                .min()
                .unwrap_or(now);
            (due, next_wake)
        };
        if due.is_empty() {
            thread::sleep(remaining(next_wake).max(Duration::from_millis(1)));
            continue;
        }
        for name in due {
            attempt_orphan(state, cli, bounds, &name);
        }
    }
}

/// One attempt for one orphan. The child is taken out of the map for the
/// attempt (the lock is not held across Docker calls) and always put back.
fn attempt_orphan(state: &ReaperState, cli: &dyn DockerCli, bounds: &OrphanBounds, name: &str) {
    let Some((mut child, mut container_gone, attempt)) = lock_held(state)
        .get_mut(name)
        .map(|orphan| (orphan.child.take(), orphan.container_gone, orphan.attempt))
    else {
        return;
    };
    let deadline = Instant::now() + bounds.command;
    let settled = catch_unwind(AssertUnwindSafe(|| {
        settle_orphan(cli, name, deadline, &mut child, &mut container_gone)
    }))
    .unwrap_or_else(|_panic| {
        tracing::debug!(attempt, "a script orphan reaper attempt panicked");
        false
    });
    let mut held = lock_held(state);
    if settled {
        tracing::debug!(attempt, "settled an orphaned script container");
        // Releases the slot.
        held.remove(name);
        return;
    }
    if let Some(orphan) = held.get_mut(name) {
        orphan.child = child;
        orphan.container_gone = container_gone;
        orphan.attempt = attempt.saturating_add(1);
        orphan.next_attempt = Instant::now() + bounds.delay(orphan.attempt);
    }
}

/// One reaper attempt: remove the container (until verified) and wait the
/// child (killing it if it is still running). `true` once both are done.
fn settle_orphan(
    cli: &dyn DockerCli,
    container_name: &str,
    deadline: Instant,
    child: &mut Option<Box<dyn ScriptChild>>,
    container_gone: &mut bool,
) -> bool {
    if !*container_gone {
        *container_gone = remove_verified(cli, container_name, deadline);
    }
    if let Some(running) = child.as_deref_mut() {
        match running.try_wait() {
            Ok(Some(_)) => *child = None,
            Ok(None) => {
                if let Err(error) = running.kill() {
                    tracing::debug!(%error, "killing an orphaned script's docker child failed");
                }
            }
            Err(error) => {
                tracing::debug!(%error, "waiting an orphaned script's docker child failed");
            }
        }
    }
    *container_gone && child.is_none()
}

/// Remove lane containers left by earlier instances on this host whose
/// process is gone. Containers of this instance, of another host, of a live
/// process, or with incomplete labels are left alone. Returns how many were
/// removed and verified gone.
pub(crate) fn reap_previous_instances(
    cli: &dyn DockerCli,
    current: &ScriptInstance,
    bound: Duration,
    process_alive: &dyn Fn(u32) -> bool,
) -> Result<usize, String> {
    let mut removed = 0;
    for container in cli.list_script_containers(bound)? {
        let previous_instance_here = container
            .instance
            .as_deref()
            .is_some_and(|instance| instance != current.instance)
            && container.host.as_deref() == Some(current.host.as_str());
        let owner_gone = container.pid.is_some_and(|pid| !process_alive(pid));
        if !(previous_instance_here && owner_gone) {
            continue;
        }
        if remove_verified(cli, &container.name, Instant::now() + bound) {
            removed += 1;
        } else {
            tracing::debug!("a previous instance's script container could not be removed");
        }
    }
    Ok(removed)
}

/// Whether a process with `pid` exists (owned by anyone).
pub(crate) fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        // silent-ok: an unrepresentable pid cannot be checked; keep the container.
        return true;
    };
    // SAFETY: signal 0 performs only the existence/permission check.
    let result = unsafe { libc::kill(pid, 0) };
    // EPERM: the process exists but belongs to someone else.
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn host_name() -> String {
    let mut buffer = [0_u8; 256];
    // SAFETY: the buffer is valid for its full length; gethostname writes at
    // most that many bytes.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return "unknown-host".to_string();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end])
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect()
}
