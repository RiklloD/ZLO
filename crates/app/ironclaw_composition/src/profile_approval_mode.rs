//! Per-profile approval mode ("Full access" / "Ask first").
//!
//! Wraps the store-backed [`ApprovalSettingsProvider`] and answers the
//! global auto-approve question from the dispatching thread's profile when
//! that profile sets an `approval_mode`; a profile without one, a thread the
//! orchestrator does not know, and a deployment without an orchestrator all
//! fall through to the global setting. Per-tool overrides, always-allow
//! policies and the hard floors the gate checks before step 6 are untouched,
//! so "Full access" never bypasses an action the policy always gates.
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
}
