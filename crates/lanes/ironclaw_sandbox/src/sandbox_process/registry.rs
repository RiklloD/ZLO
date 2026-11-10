//! Docker label identity plus bounded process-local lifecycle bookkeeping.
//!
//! User containers carry immutable tenant, user, image, and posture labels so
//! a new IronClaw process can adopt them safely. Mutable active-exec counts,
//! activity timestamps, and per-user lifecycle gates remain in
//! [`SandboxActivityRegistry`].

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use ironclaw_host_api::{
    ids::{TenantId, UserId},
    process::RuntimeProcessError,
};

use crate::sandbox_process::user_key::RebornSandboxUserKey;

pub(crate) const USER_CONTAINER_LABEL_PREFIX: &str = "ironclaw";
pub(crate) const USER_CONTAINER_LABEL_TENANT: &str = "ironclaw.tenant";
pub(crate) const USER_CONTAINER_LABEL_USER: &str = "ironclaw.user";
pub(crate) const USER_CONTAINER_LABEL_IMAGE: &str = "ironclaw.image";
pub(crate) const USER_CONTAINER_LABEL_SECURITY_POSTURE: &str = "ironclaw.security_posture";

pub(crate) fn label_tenant(prefix: &str) -> String {
    format!("{prefix}.tenant")
}
pub(crate) fn label_user(prefix: &str) -> String {
    format!("{prefix}.user")
}
pub(crate) fn label_image(prefix: &str) -> String {
    format!("{prefix}.image")
}
/// Creation timestamp stamped on a launched container.
pub(crate) fn label_created_at(prefix: &str) -> String {
    format!("{prefix}.created_at")
}

/// Label for the deterministic security and mount posture used at creation.
pub(crate) fn label_security_posture(prefix: &str) -> String {
    format!("{prefix}.security_posture")
}

pub(crate) fn build_user_container_launch_labels(
    prefix: &str,
    tenant_id: &TenantId,
    user_id: &UserId,
    image: &str,
    security_posture_stamp: &str,
) -> HashMap<String, String> {
    HashMap::from([
        (label_tenant(prefix), tenant_id.as_str().to_string()),
        (label_user(prefix), user_id.as_str().to_string()),
        (label_image(prefix), image.to_string()),
        (label_created_at(prefix), Utc::now().to_rfc3339()),
        (
            label_security_posture(prefix),
            security_posture_stamp.to_string(),
        ),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingContainerDecision {
    ReuseRunning,
    StartStopped,
    Recreate,
}

pub(crate) fn existing_container_decision(
    labels: Option<&HashMap<String, String>>,
    running: bool,
    expected: &HashMap<String, String>,
) -> ExistingContainerDecision {
    let compatible = labels.is_some_and(|labels| {
        expected
            .iter()
            .filter(|(key, _)| !key.ends_with(".created_at"))
            .all(|(key, value)| labels.get(key) == Some(value))
    });
    if !compatible {
        ExistingContainerDecision::Recreate
    } else if running {
        ExistingContainerDecision::ReuseRunning
    } else {
        ExistingContainerDecision::StartStopped
    }
}

/// Per-user execution activity and lifecycle gates. Each gate serializes shell
/// commands for one tenant and user; different users have different gates.
/// Mutable activity is process-local; immutable identity and compatibility
/// remain in Docker labels.
#[derive(Debug, Default)]
pub struct SandboxActivityRegistry {
    state: Mutex<HashMap<RebornSandboxUserKey, ActivityEntry>>,
}

#[derive(Debug)]
struct ActivityEntry {
    last_activity: Instant,
    active_execs: usize,
    gate: Arc<tokio::sync::Mutex<()>>,
    expected_labels: Option<HashMap<String, String>>,
    recycle_required: bool,
    stopped_at: Option<DateTime<Utc>>,
    egress_suspended: bool,
}

const MAX_TRACKED_USERS: usize = 4096;

impl SandboxActivityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RebornSandboxUserKey, ActivityEntry>> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        key: &RebornSandboxUserKey,
    ) -> Result<SandboxActivityGuard, RuntimeProcessError> {
        let mut state = self.lock();
        if !state.contains_key(key) && state.len() >= MAX_TRACKED_USERS {
            let oldest_inactive = state
                .iter()
                .filter(|(_, entry)| {
                    entry.active_execs == 0
                        && !entry.recycle_required
                        && Arc::strong_count(&entry.gate) == 1
                        && entry.expected_labels.is_none()
                })
                .min_by_key(|(_, entry)| entry.last_activity)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest_inactive {
                state.remove(&oldest);
            } else {
                return Err(RuntimeProcessError::ExecutionFailed(
                    "sandbox user activity registry is at capacity".to_string(),
                ));
            }
        }
        let entry = state.entry(key.clone()).or_insert_with(|| ActivityEntry {
            last_activity: Instant::now(),
            active_execs: 0,
            gate: Arc::new(tokio::sync::Mutex::new(())),
            expected_labels: None,
            recycle_required: false,
            stopped_at: None,
            egress_suspended: false,
        });
        entry.active_execs = entry.active_execs.saturating_add(1);
        entry.last_activity = Instant::now();
        entry.stopped_at = None;
        entry.egress_suspended = false;
        Ok(SandboxActivityGuard {
            registry: Arc::clone(self),
            key: key.clone(),
        })
    }

    pub(crate) fn gate(&self, key: &RebornSandboxUserKey) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.lock().get(key).map(|entry| Arc::clone(&entry.gate))
    }

    pub(crate) fn set_expected_labels(
        &self,
        key: &RebornSandboxUserKey,
        labels: HashMap<String, String>,
    ) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.expected_labels = Some(labels);
        }
    }

    pub(crate) fn mark_recycle_required(&self, key: &RebornSandboxUserKey) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.recycle_required = true;
        }
    }

    pub(crate) fn clear_recycle_required(&self, key: &RebornSandboxUserKey) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.recycle_required = false;
        }
    }

    pub(crate) fn recycle_required(&self, key: &RebornSandboxUserKey) -> bool {
        self.lock()
            .get(key)
            .is_some_and(|entry| entry.recycle_required)
    }

    pub(crate) fn register_discovered_container(
        &self,
        key: RebornSandboxUserKey,
        labels: HashMap<String, String>,
        stopped_at: Option<DateTime<Utc>>,
    ) -> Result<(), RuntimeProcessError> {
        let mut state = self.lock();
        if let Some(entry) = state.get_mut(&key) {
            entry.expected_labels = Some(labels);
            entry.stopped_at = stopped_at;
            entry.egress_suspended = false;
            return Ok(());
        }
        if state.len() >= MAX_TRACKED_USERS {
            return Err(RuntimeProcessError::ExecutionFailed(
                "sandbox user activity registry is at capacity".to_string(),
            ));
        }
        state.insert(
            key,
            ActivityEntry {
                last_activity: Instant::now(),
                active_execs: 0,
                gate: Arc::new(tokio::sync::Mutex::new(())),
                expected_labels: Some(labels),
                recycle_required: false,
                stopped_at,
                egress_suspended: false,
            },
        );
        Ok(())
    }

    pub(crate) fn sweep_candidates(
        &self,
        now: Instant,
        idle_timeout: Duration,
        wall_now: DateTime<Utc>,
        retention_timeout: Duration,
    ) -> Vec<(RebornSandboxUserKey, HashMap<String, String>)> {
        self.lock()
            .iter()
            .filter(|(_, entry)| {
                entry.active_execs == 0
                    && !entry.recycle_required
                    && sweep_due(entry, now, idle_timeout, wall_now, retention_timeout)
            })
            .filter_map(|(key, entry)| {
                entry
                    .expected_labels
                    .as_ref()
                    .map(|labels| (key.clone(), labels.clone()))
            })
            .collect()
    }

    pub(crate) fn sweep_eligible(
        &self,
        key: &RebornSandboxUserKey,
        now: Instant,
        idle_timeout: Duration,
        wall_now: DateTime<Utc>,
        retention_timeout: Duration,
    ) -> bool {
        self.lock().get(key).is_some_and(|entry| {
            entry.active_execs == 0
                && !entry.recycle_required
                && sweep_due(entry, now, idle_timeout, wall_now, retention_timeout)
        })
    }

    pub(crate) fn mark_stopped(&self, key: &RebornSandboxUserKey, now: DateTime<Utc>) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.stopped_at.get_or_insert(now);
        }
    }

    pub(crate) fn mark_egress_suspended(&self, key: &RebornSandboxUserKey) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.egress_suspended = true;
        }
    }

    pub(crate) fn retention_eligible(
        &self,
        key: &RebornSandboxUserKey,
        now: DateTime<Utc>,
        retention_timeout: Duration,
    ) -> bool {
        self.lock().get(key).is_some_and(|entry| {
            entry.active_execs == 0
                && !entry.recycle_required
                && entry.stopped_at.is_some_and(|stopped_at| {
                    now.signed_duration_since(stopped_at)
                        .to_std()
                        .is_ok_and(|elapsed| elapsed >= retention_timeout)
                })
        })
    }

    pub(crate) fn forget_if_inactive(&self, key: &RebornSandboxUserKey) {
        let mut state = self.lock();
        if state
            .get(key)
            .is_some_and(|entry| entry.active_execs == 0 && !entry.recycle_required)
        {
            state.remove(key);
        }
    }
}
fn sweep_due(
    entry: &ActivityEntry,
    now: Instant,
    idle_timeout: Duration,
    wall_now: DateTime<Utc>,
    retention_timeout: Duration,
) -> bool {
    match entry.stopped_at {
        Some(stopped_at) => {
            !entry.egress_suspended
                || wall_now
                    .signed_duration_since(stopped_at)
                    .to_std()
                    .is_ok_and(|elapsed| elapsed >= retention_timeout)
        }
        None => now.saturating_duration_since(entry.last_activity) >= idle_timeout,
    }
}

pub(crate) struct SandboxActivityGuard {
    registry: Arc<SandboxActivityRegistry>,
    key: RebornSandboxUserKey,
}

impl Drop for SandboxActivityGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.registry.lock().get_mut(&self.key) {
            entry.active_execs = entry.active_execs.saturating_sub(1);
            entry.last_activity = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::{
        ids::{TenantId, ThreadId, UserId},
        resource::ResourceScope,
    };
    fn active_execs(registry: &SandboxActivityRegistry, key: &RebornSandboxUserKey) -> usize {
        registry
            .lock()
            .get(key)
            .map_or(0, |entry| entry.active_execs)
    }

    fn set_last_activity(
        registry: &SandboxActivityRegistry,
        key: &RebornSandboxUserKey,
        activity: Instant,
    ) {
        if let Some(entry) = registry.lock().get_mut(key) {
            entry.last_activity = activity;
        }
    }

    #[test]
    fn compatibility_requires_every_user_identity_and_posture_label() {
        let tenant = TenantId::new("tenant-a").unwrap();
        let user = UserId::new("user-a").unwrap();
        let expected =
            build_user_container_launch_labels("ironclaw", &tenant, &user, "image:v1", "p1");

        assert_eq!(
            existing_container_decision(Some(&expected), true, &expected),
            ExistingContainerDecision::ReuseRunning
        );
        assert_eq!(
            existing_container_decision(Some(&expected), false, &expected),
            ExistingContainerDecision::StartStopped
        );
        assert!(!expected.contains_key("ironclaw.thread"));

        for key in [
            label_tenant("ironclaw"),
            label_user("ironclaw"),
            label_image("ironclaw"),
            label_security_posture("ironclaw"),
        ] {
            let mut mismatched = expected.clone();
            mismatched.insert(key, "different".to_string());
            assert_eq!(
                existing_container_decision(Some(&mismatched), true, &expected),
                ExistingContainerDecision::Recreate
            );
        }
        assert_eq!(
            existing_container_decision(None, true, &expected),
            ExistingContainerDecision::Recreate
        );
    }

    fn test_scope(tenant: &str, user: &str, thread: Option<&str>) -> ResourceScope {
        ResourceScope {
            tenant_id: TenantId::new(tenant).unwrap(),
            user_id: UserId::new(user).unwrap(),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: thread.map(|value| ThreadId::new(value).unwrap()),
            invocation_id: ironclaw_host_api::ids::InvocationId::new(),
        }
    }

    fn test_key(tenant: &str, user: &str) -> RebornSandboxUserKey {
        RebornSandboxUserKey::from_scope(&test_scope(tenant, user, None))
    }

    #[test]
    fn lifecycle_gate_serializes_one_user_but_not_different_users() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        let first = RebornSandboxUserKey::from_scope(&test_scope("t", "u", Some("thread-one")));
        let second = RebornSandboxUserKey::from_scope(&test_scope("t", "u", Some("thread-two")));
        let without_thread = RebornSandboxUserKey::from_scope(&test_scope("t", "u", None));
        let other_user = RebornSandboxUserKey::from_scope(&test_scope("t", "other", None));

        let first_guard = registry.begin(&first).unwrap();
        let second_guard = registry.begin(&second).unwrap();
        let other_user_guard = registry.begin(&other_user).unwrap();

        assert_eq!(first, second);
        assert_eq!(first, without_thread);
        let first_gate = registry.gate(&first).unwrap();
        let same_user_gate = registry.gate(&without_thread).unwrap();
        let other_user_gate = registry.gate(&other_user).unwrap();
        assert!(Arc::ptr_eq(&first_gate, &same_user_gate));
        assert!(!Arc::ptr_eq(&first_gate, &other_user_gate));
        let _first_lock = first_gate.try_lock().unwrap();
        assert!(same_user_gate.try_lock().is_err());
        assert!(other_user_gate.try_lock().is_ok());
        assert_eq!(active_execs(&registry, &without_thread), 2);

        drop(first_guard);
        drop(second_guard);
        drop(other_user_guard);
        assert_eq!(active_execs(&registry, &first), 0);
    }

    #[test]
    fn activity_guard_tracks_active_exec_and_updates_last_activity_on_drop() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        let key = test_key("t", "u");
        let guard = registry.begin(&key).unwrap();
        registry.set_expected_labels(
            &key,
            HashMap::from([("identity".to_string(), "v".to_string())]),
        );

        assert_eq!(active_execs(&registry, &key), 1);
        assert!(
            registry
                .sweep_candidates(Instant::now(), Duration::ZERO, Utc::now(), Duration::MAX,)
                .is_empty()
        );

        drop(guard);
        assert_eq!(active_execs(&registry, &key), 0);
        let candidates =
            registry.sweep_candidates(Instant::now(), Duration::ZERO, Utc::now(), Duration::MAX);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, key);
    }

    #[test]
    fn sweep_eligibility_requires_zero_active_execs_and_elapsed_idle_timeout() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        let key = test_key("t", "u");
        let guard = registry.begin(&key).unwrap();
        set_last_activity(&registry, &key, Instant::now() - Duration::from_secs(60));

        assert!(!registry.sweep_eligible(
            &key,
            Instant::now(),
            Duration::from_secs(30),
            Utc::now(),
            Duration::MAX,
        ));
        drop(guard);
        assert!(!registry.sweep_eligible(
            &key,
            Instant::now(),
            Duration::from_secs(30),
            Utc::now(),
            Duration::MAX,
        ));

        set_last_activity(&registry, &key, Instant::now() - Duration::from_secs(60));
        assert!(registry.sweep_eligible(
            &key,
            Instant::now(),
            Duration::from_secs(30),
            Utc::now(),
            Duration::MAX,
        ));
    }

    #[test]
    fn retention_starts_when_container_stops_and_resets_on_activity() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        let key = test_key("t", "retention");
        drop(registry.begin(&key).unwrap());
        let stopped_at = Utc::now() - chrono::Duration::seconds(60);
        registry.mark_stopped(&key, stopped_at);

        assert!(!registry.retention_eligible(&key, Utc::now(), Duration::from_secs(3600)));
        assert!(registry.retention_eligible(&key, Utc::now(), Duration::from_secs(30)));

        let guard = registry.begin(&key).unwrap();
        drop(guard);
        assert!(!registry.retention_eligible(&key, Utc::now(), Duration::ZERO));
    }

    #[test]
    fn discovered_stopped_container_preserves_its_wall_clock_retention_age() {
        let registry = SandboxActivityRegistry::new();
        let key = test_key("t", "discovered-retention");
        let stopped_at = Utc::now() - chrono::Duration::hours(25);

        registry
            .register_discovered_container(key.clone(), HashMap::new(), Some(stopped_at))
            .unwrap();

        assert_eq!(
            registry
                .sweep_candidates(
                    Instant::now(),
                    Duration::from_secs(60),
                    Utc::now(),
                    Duration::from_secs(24 * 60 * 60),
                )
                .len(),
            1
        );
        assert!(registry.retention_eligible(&key, Utc::now(), Duration::from_secs(24 * 60 * 60)));
    }

    #[test]
    fn suspended_container_is_not_reselected_before_retention_expires() {
        let registry = SandboxActivityRegistry::new();
        let key = test_key("t", "suspended");
        registry
            .register_discovered_container(
                key.clone(),
                HashMap::from([("identity".to_string(), "v".to_string())]),
                Some(Utc::now()),
            )
            .unwrap();
        registry.mark_egress_suspended(&key);

        assert!(
            registry
                .sweep_candidates(
                    Instant::now(),
                    Duration::ZERO,
                    Utc::now(),
                    Duration::from_secs(60),
                )
                .is_empty()
        );

        assert_eq!(
            registry
                .sweep_candidates(
                    Instant::now(),
                    Duration::ZERO,
                    Utc::now() + chrono::Duration::seconds(61),
                    Duration::from_secs(60),
                )
                .len(),
            1
        );
    }

    #[test]
    fn inactive_entries_can_be_pruned_but_active_entries_cannot() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        let key = test_key("t", "u");
        let guard = registry.begin(&key).unwrap();

        registry.forget_if_inactive(&key);
        assert_eq!(active_execs(&registry, &key), 1);

        drop(guard);
        registry.forget_if_inactive(&key);
        assert!(registry.gate(&key).is_none());
    }
    #[test]
    fn registry_capacity_evicts_only_container_free_entries() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        for index in 0..MAX_TRACKED_USERS {
            let key = test_key("tenant", &format!("free-{index}"));
            drop(registry.begin(&key).unwrap());
        }

        let extra = test_key("tenant", "extra");
        drop(
            registry
                .begin(&extra)
                .expect("container-free entry is evictable"),
        );
        assert!(registry.gate(&extra).is_some());
        assert_eq!(registry.lock().len(), MAX_TRACKED_USERS);
    }

    #[test]
    fn registry_capacity_never_evicts_container_backed_entries() {
        let registry = Arc::new(SandboxActivityRegistry::new());
        for index in 0..MAX_TRACKED_USERS {
            let key = test_key("tenant", &format!("container-{index}"));
            drop(registry.begin(&key).unwrap());
            registry.set_expected_labels(
                &key,
                HashMap::from([("container".to_string(), index.to_string())]),
            );
        }
        let result = registry.begin(&test_key("tenant", "overflow"));
        let Err(error) = result else {
            panic!("container-backed entries must not be orphaned");
        };
        assert_eq!(
            error,
            RuntimeProcessError::ExecutionFailed(
                "sandbox user activity registry is at capacity".to_string()
            )
        );
        assert_eq!(registry.lock().len(), MAX_TRACKED_USERS);
    }
}
