//! Script containers whose removal could not be verified.
//!
//! A timed-out script's container is force-removed by name. When that removal
//! cannot be verified, the container may still be running, so its lane slot
//! (the execution's concurrency unit) is not released: the [`OrphanReaper`]
//! takes the container and its slot, retries removal with capped backoff, and
//! drops the slot only once `docker` confirms the container is gone.
//!
//! Every script container carries labels naming the lane, the process
//! instance that started it, the host, and that process's pid. At startup,
//! [`reap_previous_instances`] removes lane containers from earlier instances
//! on this host whose process is gone — never those of a live process,
//! including another IronClaw running beside this one.

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError},
    thread,
    time::Duration,
};

use super::ScriptExecutionSlot;
use super::docker::DockerCli;

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
}

/// Remove `name` and confirm it is gone. `false` means "may still exist".
pub(crate) fn remove_verified(cli: &dyn DockerCli, name: &str, bound: Duration) -> bool {
    if let Err(error) = cli.force_remove(name, bound) {
        tracing::debug!(%error, "force-removing a script container failed");
    }
    match cli.container_exists(name, bound) {
        Ok(exists) => !exists,
        Err(error) => {
            tracing::debug!(%error, "verifying a script container's removal failed");
            false
        }
    }
}

/// Holds orphaned containers' lane slots until their removal is verified.
pub(crate) struct OrphanReaper {
    cli: Arc<dyn DockerCli>,
    bounds: OrphanBounds,
    pub(super) held: Arc<Mutex<BTreeMap<String, ScriptExecutionSlot>>>,
}

impl OrphanReaper {
    pub(crate) fn new(cli: Arc<dyn DockerCli>, bounds: OrphanBounds) -> Self {
        Self {
            cli,
            bounds,
            held: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(super) fn held(
        held: &Mutex<BTreeMap<String, ScriptExecutionSlot>>,
    ) -> MutexGuard<'_, BTreeMap<String, ScriptExecutionSlot>> {
        held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take `container_name` and its slot. The slot stays held (lane capacity
    /// stays reduced) until a retry verifies the container is gone.
    pub(crate) fn adopt(&self, container_name: String, slot: ScriptExecutionSlot) {
        Self::held(&self.held).insert(container_name.clone(), slot);
        let cli = Arc::clone(&self.cli);
        let bounds = self.bounds.clone();
        let held = Arc::clone(&self.held);
        let spawned = thread::Builder::new()
            .name("ironclaw-script-orphan-reaper".to_string())
            .spawn(move || {
                let mut attempt = 0_usize;
                loop {
                    let delay = bounds
                        .backoff
                        .get(attempt)
                        .or(bounds.backoff.last())
                        .copied()
                        .unwrap_or(Duration::from_secs(1));
                    thread::sleep(delay);
                    if remove_verified(cli.as_ref(), &container_name, bounds.command) {
                        tracing::debug!(attempt, "removed an orphaned script container");
                        // Releases the slot.
                        Self::held(&held).remove(&container_name);
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                    tracing::debug!(attempt, "orphaned script container still present; retrying");
                }
            });
        if let Err(error) = spawned {
            // Without a reaper thread the slot stays held: capacity fails
            // closed rather than re-admitting over a live container.
            tracing::debug!(%error, "spawning the script orphan reaper failed; slot stays held");
        }
    }
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
        if remove_verified(cli, &container.name, bound) {
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
