//! Per-profile approval mode ("Full access" / "Ask first").
//!
//! Wraps the store-backed [`ApprovalSettingsProvider`] and answers the
//! global auto-approve question from the dispatching thread's profile when
//! that profile sets an `approval_mode`; a profile without one, a thread the
//! orchestrator does not know, and a deployment without an orchestrator all
//! fall through to the global setting.
//!
//! "Full access" is therefore the thread profile's `approval_mode`, or, for a
//! thread whose profile sets none, the global auto-approve switch. Under Full
//! access the gate asks for nothing: hard floors, the origin matrix's
//! `AskAlways` gate and per-tool `ask_each_time` overrides are all skipped,
//! and only a per-tool `disabled` still denies (step 2b of the gate in
//! `ironclaw_approvals::profile_gate`). "Ask first" — the profile's mode, or
//! the global switch off for a profile that sets none — keeps every gate.
//!
//! The orchestrator engine is built after the host-runtime authorizer, so it
//! arrives through a [`ProfileApprovalEngineSlot`] filled once at assembly.
//! Lookups are cached per thread and invocation for the same short TTL as
//! the other approval settings: one tool call reads the mode once, and a
//! mode switch reaches the very next tool call.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ironclaw_approvals::{ApprovalSettingsProvider, ToolPermissionOverride};
use ironclaw_host_api::{
    ids::{CapabilityId, InvocationId, ThreadId},
    resource::ResourceScope,
    scope::Principal,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_product_contracts::profiles::ApprovalMode;

/// Filled with the orchestrator engine once it exists (libSQL deployments).
pub(crate) type ProfileApprovalEngineSlot = Arc<OnceLock<Arc<Engine>>>;

const PROFILE_MODE_CACHE_TTL: Duration = Duration::from_millis(500);
const PROFILE_MODE_CACHE_MAX_ENTRIES: usize = 512;

/// One tool call's profile mode, stamped with when it was read.
type ModeCache = HashMap<(ThreadId, InvocationId), (Instant, Option<ApprovalMode>)>;

pub(crate) struct ProfileApprovalSettingsProvider {
    inner: Arc<dyn ApprovalSettingsProvider>,
    engine: ProfileApprovalEngineSlot,
    cache: Mutex<ModeCache>,
}

impl ProfileApprovalSettingsProvider {
    pub(crate) fn new(
        inner: Arc<dyn ApprovalSettingsProvider>,
        engine: ProfileApprovalEngineSlot,
    ) -> Self {
        Self {
            inner,
            engine,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The thread's profile mode; `Err(())` when the lookup failed.
    async fn profile_mode(
        &self,
        thread: &ThreadId,
        invocation: &InvocationId,
    ) -> Result<Option<ApprovalMode>, ()> {
        let Some(engine) = self.engine.get() else {
            return Ok(None);
        };
        let key = (thread.clone(), *invocation);
        if let Some((at, mode)) = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            && at.elapsed() < PROFILE_MODE_CACHE_TTL
        {
            return Ok(*mode);
        }
        let mode = match engine.store().thread_profile(thread).await {
            Ok(profile) => profile.and_then(|p| p.settings.approval_mode),
            Err(error) => {
                tracing::debug!(%error, "profile approval mode lookup failed; asking");
                return Err(());
            }
        };
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= PROFILE_MODE_CACHE_MAX_ENTRIES {
            cache.retain(|_, (at, _)| at.elapsed() < PROFILE_MODE_CACHE_TTL);
            if cache.len() >= PROFILE_MODE_CACHE_MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), mode));
        Ok(mode)
    }
}

#[async_trait]
impl ApprovalSettingsProvider for ProfileApprovalSettingsProvider {
    async fn tool_override(
        &self,
        scope: &ResourceScope,
        capability_id: &CapabilityId,
    ) -> Option<ToolPermissionOverride> {
        self.inner.tool_override(scope, capability_id).await
    }

    async fn global_auto_approve(&self, scope: &ResourceScope) -> bool {
        let mode = match &scope.thread_id {
            Some(thread) => match self.profile_mode(thread, &scope.invocation_id).await {
                Ok(mode) => mode,
                // Fail safe like the store provider: an unreadable profile asks.
                Err(()) => return false,
            },
            None => None,
        };
        if mode.is_some() {
            return ApprovalMode::resolve_auto_approve(mode, false);
        }
        self.inner.global_auto_approve(scope).await
    }

    /// "Full access" as the pill shows it: the profile's mode, or the global
    /// switch for a profile that follows it.
    async fn full_access(&self, scope: &ResourceScope) -> bool {
        scope.thread_id.is_some() && self.global_auto_approve(scope).await
    }

    async fn tool_always_allow(
        &self,
        scope: &ResourceScope,
        capability_id: &CapabilityId,
        grantee: &Principal,
    ) -> bool {
        self.inner
            .tool_always_allow(scope, capability_id, grantee)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_mode_overrides_global_and_missing_inherits() {
        for global in [true, false] {
            assert!(ApprovalMode::resolve_auto_approve(
                Some(ApprovalMode::FullAccess),
                global
            ));
            assert!(!ApprovalMode::resolve_auto_approve(
                Some(ApprovalMode::AskFirst),
                global
            ));
            assert_eq!(ApprovalMode::resolve_auto_approve(None, global), global);
        }
    }

    struct GlobalOn;

    #[async_trait]
    impl ApprovalSettingsProvider for GlobalOn {
        async fn tool_override(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
        ) -> Option<ToolPermissionOverride> {
            None
        }

        async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
            true
        }

        async fn tool_always_allow(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
            _grantee: &Principal,
        ) -> bool {
            false
        }
    }

    /// A thread whose profile cannot be read never runs a gated call on the
    /// global switch: with the thread in scope (as every gate asks) the
    /// failed lookup asks instead of falling through to global auto-approve.
    #[tokio::test]
    async fn an_unreadable_thread_profile_asks_even_with_global_auto_approve_on() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let tenant = ironclaw_host_api::ids::TenantId::new("t1").unwrap();
        let user = ironclaw_host_api::ids::UserId::new("alice").unwrap();
        let thread = ThreadId::new("thread-unreadable-profile").unwrap();
        engine
            .ensure_thread(
                &thread,
                &ironclaw_orchestrator::model::ThreadScope {
                    tenant_id: tenant.clone(),
                    user_id: user.clone(),
                    agent_id: None,
                    project_id: None,
                },
                None,
            )
            .await
            .unwrap();
        db.connect()
            .unwrap()
            .execute("DROP TABLE orch_profiles", ())
            .await
            .unwrap();
        let slot: ProfileApprovalEngineSlot = Arc::new(OnceLock::new());
        assert!(slot.set(engine).is_ok());
        let provider = ProfileApprovalSettingsProvider::new(Arc::new(GlobalOn), slot);

        let verdict = |thread_id: Option<ThreadId>| {
            let scope = ResourceScope {
                tenant_id: tenant.clone(),
                user_id: user.clone(),
                agent_id: None,
                project_id: None,
                mission_id: None,
                thread_id,
                invocation_id: InvocationId::new(),
            };
            let provider = &provider;
            async move {
                let capability = CapabilityId::new("builtin.t3_delegate").unwrap();
                ironclaw_approvals::approval_gate_verdict(
                    provider,
                    ironclaw_approvals::ApprovalGateQuery {
                        scope: &scope,
                        capability_id: &capability,
                        grantee: &Principal::HostRuntime,
                        approved_for_this_invocation: false,
                        force_approval: false,
                        exempt_from_approval: false,
                        global_auto_approve_eligible: true,
                        always_allow_eligible: true,
                        approved_for_this_task: false,
                        policy_requires_approval: true,
                    },
                )
                .await
            }
        };
        assert_eq!(
            verdict(Some(thread)).await,
            ironclaw_approvals::ApprovalGateVerdict::RequireApproval,
            "a failed profile lookup must ask, never fall through to allow"
        );
        assert_eq!(
            verdict(None).await,
            ironclaw_approvals::ApprovalGateVerdict::Allow,
            "control: without the thread the global switch decides (the old bug's path)"
        );
    }
}
