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
}

impl ApprovalsDomainHandler {
    pub fn new(
        auto_approve: Arc<dyn AutoApproveSettingStorePort>,
        overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
        persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
        tool_catalog: Arc<dyn RebornOperatorToolCatalog>,
    ) -> Self {
        Self {
            auto_approve,
            overrides,
            persistent_policies,
            tool_catalog,
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
                    applied_count += 1;
                }
            }
        }

        // 3. Clear overrides/grants that were removed on the source (reverts to default).
        for (&cap_id, &prev_state) in &current_map {
            if desired_map.contains_key(cap_id) {
                continue;
            }
            let capability_id =
                CapabilityId::new(cap_id).map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::Approvals,
                    reason: error.to_string(),
                })?;
            match prev_state {
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
                    if let Some(tool) = tools_by_id.get(cap_id) {
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
