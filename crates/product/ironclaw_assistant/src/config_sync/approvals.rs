//! `approvals` domain handler: global auto-approve toggle and per-tool
//! permission overrides (`always_allow`, `ask_each_time`, `disabled`).
//!
//! Never reads or writes `orch_cli_approvals` (runtime audit history).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_approvals::{
    AutoApproveSettingInput, AutoApproveSettingKey, AutoApproveSettingStorePort,
    CapabilityPermissionOverride, CapabilityPermissionOverrideInput,
    CapabilityPermissionOverrideKey, CapabilityPermissionOverrideStorePort,
    PersistentApprovalAction, PersistentApprovalPolicyError, PersistentApprovalPolicyInput,
    PersistentApprovalPolicyKey, PersistentApprovalPolicyStorePort,
};
use ironclaw_host_api::capability::GrantConstraints;
use ironclaw_host_api::capability::{EffectKind, PermissionMode};
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_host_api::scope::Principal;
use ironclaw_product_contracts::config_sync::{
    ApprovalsDomainPayload, ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload,
    DomainApplyOutcome, DomainApplyStatus, ToolApprovalOverrideEntry, ToolApprovalSyncState,
};
use ironclaw_product_contracts::operator_tools::{
    RebornOperatorToolCatalog, RebornOperatorToolInfo,
};

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult,
    canonical_json_sha256,
};

pub struct ApprovalsDomainHandler {
    auto_approve: Arc<dyn AutoApproveSettingStorePort>,
    overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
    persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
    tool_catalog: Arc<dyn RebornOperatorToolCatalog>,
    /// Where the overrides and grants a sync made here are remembered, so
    /// one the source later resets can be taken back.
    notes: Arc<dyn super::notes::SyncNotes>,
}

/// One override or grant a pass made here: the state it set, and when the
/// record that holds it was last written, as read back after the pass.
/// `at` is `None` between the note of an intent and the pass's end.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "MadeEntryWire")]
struct MadeEntry {
    state: ToolApprovalSyncState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<DateTime<Utc>>,
}

/// A record as it may be found: as written now, or the bare state an
/// earlier build wrote (read as an entry that was never stamped).
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum MadeEntryWire {
    Stamped {
        state: ToolApprovalSyncState,
        #[serde(default)]
        at: Option<DateTime<Utc>>,
    },
    Bare(ToolApprovalSyncState),
}

impl From<MadeEntryWire> for MadeEntry {
    fn from(wire: MadeEntryWire) -> Self {
        match wire {
            MadeEntryWire::Stamped { state, at } => Self { state, at },
            MadeEntryWire::Bare(state) => Self { state, at: None },
        }
    }
}

/// What sync made here for one other computer, by tool.
type Made = BTreeMap<String, MadeEntry>;

impl ApprovalsDomainHandler {
    /// The record's path, when there is a place to keep one and the ids
    /// can name it.
    fn made_path(ctx: &ConfigSyncContext) -> Option<[String; 4]> {
        let names = [
            "approvals-made".to_string(),
            ctx.caller.tenant_id.as_str().to_string(),
            ctx.caller.user_id.as_str().to_string(),
            ctx.peer_environment_id.clone()?,
        ];
        names
            .iter()
            .all(|name| super::notes::safe_name(name))
            .then_some(names)
    }

    async fn made(&self, ctx: &ConfigSyncContext) -> Result<Made, ConfigSyncError> {
        let Some(path) = Self::made_path(ctx) else {
            return Ok(Made::new());
        };
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        match self.notes.read(&path).await? {
            Some(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| ConfigSyncError::Serialization {
                    domain: ConfigSyncDomain::Approvals,
                    source,
                })
            }
            None => Ok(Made::new()),
        }
    }

    async fn remember(&self, ctx: &ConfigSyncContext, made: &Made) -> Result<(), ConfigSyncError> {
        let Some(path) = Self::made_path(ctx) else {
            return Ok(());
        };
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let bytes = serde_json::to_vec(made).map_err(|source| ConfigSyncError::Serialization {
            domain: ConfigSyncDomain::Approvals,
            source,
        })?;
        self.notes.write(&path, &bytes).await
    }

    pub fn new(
        auto_approve: Arc<dyn AutoApproveSettingStorePort>,
        overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
        persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
        tool_catalog: Arc<dyn RebornOperatorToolCatalog>,
        notes: Arc<dyn super::notes::SyncNotes>,
    ) -> Self {
        Self {
            auto_approve,
            overrides,
            persistent_policies,
            tool_catalog,
            notes,
        }
    }

    fn settings_scope(ctx: &ConfigSyncContext) -> ResourceScope {
        // As the operator tool-permission routes scope them: by tenant and
        // user, never by run axes.
        ResourceScope {
            tenant_id: ctx.caller.tenant_id.clone(),
            user_id: ctx.caller.user_id.clone(),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: ironclaw_host_api::ids::InvocationId::new(),
        }
    }

    fn is_locked(tool: &RebornOperatorToolInfo) -> bool {
        tool.default_permission == PermissionMode::Deny
            || tool.effects.iter().any(|effect| {
                matches!(
                    effect,
                    EffectKind::Financial | EffectKind::ModifyApproval | EffectKind::ModifyBudget
                )
            })
    }

    async fn read_state(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<(bool, Vec<ToolApprovalOverrideEntry>, Option<DateTime<Utc>>), ConfigSyncError>
    {
        let scope = Self::settings_scope(ctx);
        let auto_key = AutoApproveSettingKey::from_resource_scope(&scope);
        let auto_record =
            self.auto_approve
                .get(&auto_key)
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::Approvals,
                    reason: error.to_string(),
                })?;
        let mut latest_updated = auto_record.as_ref().map(|r| r.updated_at);
        let global_auto_approve = auto_record
            .as_ref()
            .map_or(ironclaw_approvals::AUTO_APPROVE_DEFAULT_ENABLED, |r| {
                r.enabled
            });

        let mut map: BTreeMap<String, ToolApprovalSyncState> = BTreeMap::new();

        // Persistent `always_allow` grants first; explicit overrides (`ask_each_time` /
        // `disabled`) take precedence if both exist.
        if self.persistent_policies.supports_scope_listing() {
            let policies = self
                .persistent_policies
                .list_for_scope_action(&scope, PersistentApprovalAction::Dispatch)
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::Approvals,
                    reason: error.to_string(),
                })?;
            for policy in policies {
                if policy.active_grant().is_some() {
                    latest_updated = max_ts(latest_updated, Some(policy.updated_at));
                    map.insert(
                        policy.key.capability_id.as_str().to_string(),
                        ToolApprovalSyncState::AlwaysAllow,
                    );
                }
            }
        }

        if self.overrides.supports_scope_listing() {
            let records = self
                .overrides
                .list_for_scope(&scope)
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::Approvals,
                    reason: error.to_string(),
                })?;
            for record in records {
                latest_updated = max_ts(latest_updated, Some(record.updated_at));
                let state = match record.state {
                    CapabilityPermissionOverride::AskEachTime => ToolApprovalSyncState::AskEachTime,
                    CapabilityPermissionOverride::Disabled => ToolApprovalSyncState::Disabled,
                };
                map.insert(record.key.capability_id.as_str().to_string(), state);
            }
        }

        let tool_overrides = map
            .into_iter()
            .map(|(capability_id, state)| ToolApprovalOverrideEntry {
                capability_id,
                state,
            })
            .collect();
        Ok((global_auto_approve, tool_overrides, latest_updated))
    }

    /// When the record that decides each tool's state was last written:
    /// a change made on this computer since, even back to the same state,
    /// moves it.
    async fn stamps(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<BTreeMap<String, DateTime<Utc>>, ConfigSyncError> {
        let scope = Self::settings_scope(ctx);
        let store = |error: &dyn std::fmt::Display| ConfigSyncError::Store {
            domain: ConfigSyncDomain::Approvals,
            reason: error.to_string(),
        };
        let mut out = BTreeMap::new();
        if self.persistent_policies.supports_scope_listing() {
            for policy in self
                .persistent_policies
                .list_for_scope_action(&scope, PersistentApprovalAction::Dispatch)
                .await
                .map_err(|error| store(&error))?
            {
                if policy.active_grant().is_some() {
                    out.insert(
                        policy.key.capability_id.as_str().to_string(),
                        policy.updated_at,
                    );
                }
            }
        }
        if self.overrides.supports_scope_listing() {
            for record in self
                .overrides
                .list_for_scope(&scope)
                .await
                .map_err(|error| store(&error))?
            {
                out.insert(
                    record.key.capability_id.as_str().to_string(),
                    record.updated_at,
                );
            }
        }
        Ok(out)
    }

    fn hash_state(
        global_auto_approve: bool,
        tool_overrides: &[ToolApprovalOverrideEntry],
    ) -> Result<String, ConfigSyncError> {
        let mut sorted = tool_overrides.to_vec();
        sorted.sort();
        let value = serde_json::json!({
            "global_auto_approve": global_auto_approve,
            "tool_overrides": sorted,
        });
        canonical_json_sha256(ConfigSyncDomain::Approvals, &value)
    }
}

fn max_ts(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for ApprovalsDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        ConfigSyncDomain::Approvals
    }

    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let (global_auto_approve, tool_overrides, updated_at) = self.read_state(ctx).await?;
        let hash = Self::hash_state(global_auto_approve, &tool_overrides)?;
        Ok(ConfigSyncDomainDigest {
            hash,
            updated_at,
            item_count: 1 + tool_overrides.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let (global_auto_approve, tool_overrides, _) = self.read_state(ctx).await?;
        let hash = Self::hash_state(global_auto_approve, &tool_overrides)?;
        Ok(ConfigSyncDomainPayload::Approvals(ApprovalsDomainPayload {
            hash,
            global_auto_approve,
            tool_overrides,
        }))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::Approvals(p) = payload else {
            return Err(ConfigSyncError::validation(
                "approvals",
                "expected approvals payload",
            ));
        };
        let mut seen = BTreeSet::new();
        for entry in &p.tool_overrides {
            CapabilityId::new(entry.capability_id.as_str()).map_err(|error| {
                ConfigSyncError::validation(
                    "approvals.tool_overrides",
                    format!("invalid capability_id `{}`: {error}", entry.capability_id),
                )
            })?;
            if !seen.insert(entry.capability_id.as_str()) {
                return Err(ConfigSyncError::validation(
                    "approvals.tool_overrides",
                    format!("duplicate capability_id `{}`", entry.capability_id),
                ));
            }
        }
        Ok(())
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::Approvals(p) = payload else {
            return Err(ConfigSyncError::validation(
                "approvals",
                "expected approvals payload",
            ));
        };
        let scope = Self::settings_scope(ctx);
        let (current_auto, current_overrides, _) = self.read_state(ctx).await?;
        let current_map: BTreeMap<&str, ToolApprovalSyncState> = current_overrides
            .iter()
            .map(|e| (e.capability_id.as_str(), e.state))
            .collect();
        let desired_map: BTreeMap<&str, ToolApprovalSyncState> = p
            .tool_overrides
            .iter()
            .map(|e| (e.capability_id.as_str(), e.state))
            .collect();

        let tools = self
            .tool_catalog
            .list_operator_tools(&ctx.caller.user_id)
            .await;
        let tools_by_id: BTreeMap<&str, &RebornOperatorToolInfo> = tools
            .iter()
            .map(|t| (t.capability_id.as_str(), t))
            .collect();

        let mut applied_count: u64 = 0;
        let mut skipped_count: u64 = 0;
        let mut warnings = Vec::new();
        // What earlier passes from this computer made here, and what this
        // one leaves standing.
        let made_before = self.made(ctx).await?;
        let stamps_before = self.stamps(ctx).await?;
        // Still as a pass left it: the same state, in a record not written
        // since (an entry noted but never stamped goes by its state).
        let untouched = |cap_id: &str, entry: &MadeEntry| {
            current_map.get(cap_id) == Some(&entry.state)
                && entry
                    .at
                    .is_none_or(|at| stamps_before.get(cap_id) == Some(&at))
        };
        let mut made: BTreeMap<String, ToolApprovalSyncState> = BTreeMap::new();
        // What this pass is about to make is noted before it is made: a
        // grant must never stand on this computer without a note that
        // sync put it there, or it could not be taken back.
        let mut intent = made_before.clone();
        for (&cap_id, &state) in &desired_map {
            if current_map.get(cap_id) != Some(&state) {
                intent.insert(cap_id.to_string(), MadeEntry { state, at: None });
            }
        }
        if intent != made_before {
            self.remember(ctx, &intent).await?;
        }

        // 1. Global auto-approve toggle.
        if current_auto != p.global_auto_approve {
            self.auto_approve
                .set(AutoApproveSettingInput {
                    scope: scope.clone(),
                    enabled: p.global_auto_approve,
                    updated_by: Principal::User(ctx.caller.user_id.clone()),
                })
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::Approvals,
                    reason: error.to_string(),
                })?;
            applied_count += 1;
        } else {
            skipped_count += 1;
        }

        // 2. Upsert incoming per-tool overrides.
        for (&cap_id, &desired_state) in &desired_map {
            if current_map.get(cap_id) == Some(&desired_state) {
                // Still as a pass made it: still sync's to take back.
                if made_before
                    .get(cap_id)
                    .is_some_and(|entry| untouched(cap_id, entry))
                {
                    made.insert(cap_id.to_string(), desired_state);
                }
                skipped_count += 1;
                continue;
            }
            let capability_id = CapabilityId::new(cap_id).map_err(|error| {
                ConfigSyncError::validation(
                    "approvals.tool_overrides",
                    format!("invalid capability_id `{cap_id}`: {error}"),
                )
            })?;
            match desired_state {
                ToolApprovalSyncState::AskEachTime | ToolApprovalSyncState::Disabled => {
                    if let Some(tool) = tools_by_id.get(cap_id) {
                        if Self::is_locked(tool) {
                            warnings.push(format!("skipped locked tool `{cap_id}`"));
                            skipped_count += 1;
                            continue;
                        }
                        let policy_key = PersistentApprovalPolicyKey::new(
                            &scope,
                            PersistentApprovalAction::Dispatch,
                            tool.capability_id.clone(),
                            Principal::Extension(tool.provider.clone()),
                        );
                        match self.persistent_policies.revoke(&policy_key).await {
                            Ok(_) | Err(PersistentApprovalPolicyError::UnknownPolicy) => {}
                            Err(error) => {
                                return Err(ConfigSyncError::Store {
                                    domain: ConfigSyncDomain::Approvals,
                                    reason: error.to_string(),
                                });
                            }
                        }
                    }
                    let override_state = match desired_state {
                        ToolApprovalSyncState::AskEachTime => {
                            CapabilityPermissionOverride::AskEachTime
                        }
                        ToolApprovalSyncState::Disabled => CapabilityPermissionOverride::Disabled,
                        ToolApprovalSyncState::AlwaysAllow => unreachable!(),
                    };
                    self.overrides
                        .set(CapabilityPermissionOverrideInput {
                            scope: scope.clone(),
                            capability_id,
                            state: override_state,
                            updated_by: Principal::User(ctx.caller.user_id.clone()),
                        })
                        .await
                        .map_err(|error| ConfigSyncError::Store {
                            domain: ConfigSyncDomain::Approvals,
                            reason: error.to_string(),
                        })?;
                    made.insert(cap_id.to_string(), desired_state);
                    applied_count += 1;
                }
                ToolApprovalSyncState::AlwaysAllow => {
                    let Some(tool) = tools_by_id.get(cap_id) else {
                        warnings.push(format!(
                            "skipped always_allow for `{cap_id}`: tool not registered on target"
                        ));
                        skipped_count += 1;
                        continue;
                    };
                    if Self::is_locked(tool) {
                        warnings.push(format!("skipped locked tool `{cap_id}`"));
                        skipped_count += 1;
                        continue;
                    }
                    self.persistent_policies
                        .allow(PersistentApprovalPolicyInput {
                            scope: scope.clone(),
                            action: PersistentApprovalAction::Dispatch,
                            capability_id: tool.capability_id.clone(),
                            grantee: Principal::Extension(tool.provider.clone()),
                            approved_by: Principal::User(ctx.caller.user_id.clone()),
                            constraints: GrantConstraints {
                                allowed_effects: tool.effects.as_ref().to_vec(),
                                mounts: Default::default(),
                                network: Default::default(),
                                secrets: Vec::new(),
                                resource_ceiling: None,
                                expires_at: None,
                                max_invocations: None,
                            },
                            source_approval_request_id: None,
                        })
                        .await
                        .map_err(|error| ConfigSyncError::Store {
                            domain: ConfigSyncDomain::Approvals,
                            reason: error.to_string(),
                        })?;
                    self.overrides
                        .clear(&CapabilityPermissionOverrideKey::new(
                            &scope,
                            tool.capability_id.clone(),
                        ))
                        .await
                        .map_err(|error| ConfigSyncError::Store {
                            domain: ConfigSyncDomain::Approvals,
                            reason: error.to_string(),
                        })?;
                    made.insert(cap_id.to_string(), desired_state);
                    applied_count += 1;
                }
            }
        }

        // 3. A tool the source no longer mentions. What this computer set
        // for itself is kept: only an override or grant an earlier pass
        // made here, and that is still as that pass left it, is taken back
        // (the source reset it; left standing, a grant would keep allowing
        // a tool the person has put back to asking).
        for (cap_id, entry) in &made_before {
            if desired_map.contains_key(cap_id.as_str()) || !untouched(cap_id, entry) {
                continue;
            }
            let was = &entry.state;
            // Looked at again the moment before it is taken back: a change
            // made on this computer while the pass ran is this computer's.
            if entry.at.is_some() && self.stamps(ctx).await?.get(cap_id) != entry.at.as_ref() {
                continue;
            }
            let Ok(capability_id) = CapabilityId::new(cap_id.as_str()) else {
                continue;
            };
            match was {
                ToolApprovalSyncState::AskEachTime | ToolApprovalSyncState::Disabled => {
                    self.overrides
                        .clear(&CapabilityPermissionOverrideKey::new(&scope, capability_id))
                        .await
                        .map_err(|error| ConfigSyncError::Store {
                            domain: ConfigSyncDomain::Approvals,
                            reason: error.to_string(),
                        })?;
                    applied_count += 1;
                }
                ToolApprovalSyncState::AlwaysAllow => {
                    let Some(tool) = tools_by_id.get(cap_id.as_str()) else {
                        // Not here to revoke now: still remembered.
                        made.insert(cap_id.clone(), *was);
                        continue;
                    };
                    let policy_key = PersistentApprovalPolicyKey::new(
                        &scope,
                        PersistentApprovalAction::Dispatch,
                        tool.capability_id.clone(),
                        Principal::Extension(tool.provider.clone()),
                    );
                    match self.persistent_policies.revoke(&policy_key).await {
                        Ok(_) | Err(PersistentApprovalPolicyError::UnknownPolicy) => {}
                        Err(error) => {
                            return Err(ConfigSyncError::Store {
                                domain: ConfigSyncDomain::Approvals,
                                reason: error.to_string(),
                            });
                        }
                    }
                    applied_count += 1;
                }
            }
        }
        // Each entry is stamped with its record as it stands now, so a
        // later change on this computer can be told from sync's own.
        let stamps_after = self.stamps(ctx).await?;
        let made: Made = made
            .into_iter()
            .map(|(cap_id, state)| {
                let at = stamps_after.get(&cap_id).copied();
                (cap_id, MadeEntry { state, at })
            })
            .collect();
        if made != intent
            && let Err(error) = self.remember(ctx, &made).await
        {
            // The intent noted above still stands for these entries.
            tracing::debug!(%error, "config sync: approvals record not updated");
            warnings.push("what this sync set could not be fully recorded".to_string());
        }

        let status = if applied_count > 0 {
            DomainApplyStatus::Applied
        } else {
            DomainApplyStatus::SkippedUnchanged
        };
        Ok(DomainApplyResult {
            outcome: DomainApplyOutcome {
                status,
                reason: None,
                applied_count,
                skipped_count,
                warnings,
            },
            restart_required: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_from_an_earlier_build_is_read_as_unstamped() {
        let made: Made = serde_json::from_str(
            r#"{"demo.old":"always_allow","demo.new":{"state":"disabled","at":"2026-11-26T10:00:00Z"},"demo.pending":{"state":"ask_each_time"}}"#,
        )
        .expect("record");
        assert_eq!(made["demo.old"].state, ToolApprovalSyncState::AlwaysAllow);
        assert_eq!(made["demo.old"].at, None);
        assert!(made["demo.new"].at.is_some());
        assert_eq!(made["demo.pending"].at, None);
        // And it is written back in the current form.
        let again: Made =
            serde_json::from_str(&serde_json::to_string(&made).expect("json")).expect("round trip");
        assert_eq!(again, made);
    }
}
