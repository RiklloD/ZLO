//! Operator configuration keys, the global auto-approve setting, and the
//! tool-permission model (`admin-config` charter row).

use super::*;

pub(super) const AUTO_APPROVE_CONFIG_KEY: &str = "agent.auto_approve_tools";

pub(super) const TOOL_CONFIG_PREFIX: &str = "tool.";

pub const OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY_ID: &str =
    "builtin.operator_config_set_auto_approve";

pub const OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY_ID);

pub const OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY_ID: &str =
    "builtin.operator_config_set_tool_permission";

pub const OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY_ID);

pub const OPERATOR_CONFIG_SET_KEY_COMMAND_ID: &str = "operator.config.set_key";

pub const OPERATOR_CONFIG_SET_KEY_COMMAND: ProductSurfaceCommandDescriptor<
    RebornOperatorConfigSetProductRequest,
    RebornOperatorConfigGetResponse,
> = ProductSurfaceCommandDescriptor::new(OPERATOR_CONFIG_SET_KEY_COMMAND_ID);

pub const GLOBAL_AUTO_APPROVE_VIEW: ProductView<
    RebornGlobalAutoApproveRequest,
    RebornGlobalAutoApproveResponse,
> = ProductView::unpaginated("global_auto_approve");

#[derive(Clone)]
pub(super) struct RebornOperatorApprovalConfig {
    pub(super) overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
    pub(super) auto_approve: Arc<dyn AutoApproveSettingStorePort>,
    pub(super) persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
    pub(super) tool_catalog: Arc<dyn RebornOperatorToolCatalog>,
}

pub(super) fn operator_config_not_wired_response() -> RebornOperatorConfigListResponse {
    RebornOperatorConfigListResponse {
        entries: Vec::new(),
        precedence: Vec::new(),
        diagnostics: vec![operator_config_surface_not_wired_diagnostic()],
    }
}

pub(super) fn operator_config_unknown_key_error(field: &'static str) -> ProductSurfaceError {
    ProductSurfaceError::validation(field, ProductSurfaceValidationCode::UnknownKey)
}

pub(super) fn operator_config_invalid_value(field: &'static str) -> ProductSurfaceError {
    ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidValue)
}

// `internal_from` logs the backend cause while keeping the service payload
// sanitized, so operator diagnostics survive without leaking over the wire.
pub(super) fn operator_config_store_error(error: impl std::fmt::Display) -> ProductSurfaceError {
    ProductSurfaceError::internal_from(error)
}

pub(super) fn operator_config_capability_forbidden() -> ProductSurfaceError {
    ProductSurfaceError::from_status(ProductSurfaceErrorCode::Forbidden, 403, false)
}

pub(super) fn product_view_requires_operator_config(view_id: &str) -> bool {
    matches!(
        view_id,
        id if id == ADMIN_CONFIGURATION_VIEW.id
            || id == OPERATOR_LOGS_VIEW.id
            || id == LLM_CONFIG_VIEW.id
            || id == OPERATOR_SETUP_VIEW.id
            || id == OPERATOR_DIAGNOSTICS_VIEW.id
            || id == OPERATOR_STATUS_VIEW.id
            || id == INSPECTOR_SNAPSHOT_VIEW.id
            || id == INSPECTOR_PROMPT_VIEW.id
            || id == INSPECTOR_TOOL_VIEW.id
            || id == INSPECTOR_UPDATES_VIEW.id
    )
}

pub(super) fn operator_config_auto_approve_activity_id(
    caller: &ProductSurfaceCaller,
    enabled: bool,
) -> ActivityId {
    let mut seed = Vec::new();
    for segment in [
        "product-surface-operator-config-auto-approve",
        caller.tenant_id.as_str(),
        caller.user_id.as_str(),
        caller.agent_id.as_ref().map(|id| id.as_str()).unwrap_or(""),
        caller
            .project_id
            .as_ref()
            .map(|id| id.as_str())
            .unwrap_or(""),
        if enabled { "enabled" } else { "disabled" },
    ] {
        seed.extend_from_slice(&(segment.len() as u64).to_be_bytes());
        seed.extend_from_slice(segment.as_bytes());
    }
    ActivityId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, &seed))
}

pub(super) fn operator_config_mutation_succeeded(
    resolution: Resolution,
) -> Result<(), ProductSurfaceError> {
    match resolution {
        Resolution::Done(outcome) if outcome.verdict.is_success() => Ok(()),
        Resolution::Done(outcome) => match outcome.verdict.error_kind() {
            Some(FailureKind::InputEncode) => Err(operator_config_invalid_value("value")),
            Some(FailureKind::Authorization | FailureKind::PolicyDenied) => {
                Err(operator_config_capability_forbidden())
            }
            Some(FailureKind::Backend | FailureKind::Transient | FailureKind::Unavailable) => {
                Err(ProductSurfaceError::service_unavailable(true))
            }
            _ => Err(ProductSurfaceError::internal_from(
                "operator config capability returned a non-success result",
            )),
        },
        Resolution::Denied(_) => Err(operator_config_capability_forbidden()),
        Resolution::Blocked(_) | Resolution::Suspended(_) => {
            Err(ProductSurfaceError::service_unavailable(true))
        }
    }
}

pub(super) async fn auto_approve_config_entry(
    config: &RebornOperatorApprovalConfig,
    scope: &ResourceScope,
) -> Result<RebornOperatorConfigEntry, ProductSurfaceError> {
    let operator_scope = operator_tool_permission_scope(scope);
    let key = AutoApproveSettingKey::from_resource_scope(&operator_scope);
    let record = config
        .auto_approve
        .get(&key)
        .await
        .map_err(operator_config_store_error)?;
    let enabled = record
        .as_ref()
        .map_or(AUTO_APPROVE_DEFAULT_ENABLED, |record| record.enabled);
    Ok(RebornOperatorConfigEntry {
        key: AUTO_APPROVE_CONFIG_KEY.to_string(),
        value: serde_json::json!(enabled),
        source: if record.is_some() {
            "override".to_string()
        } else {
            "default".to_string()
        },
        redacted: false,
        mutable: true,
    })
}

pub(super) async fn find_operator_tool(
    config: &RebornOperatorApprovalConfig,
    raw_capability_id: &str,
    caller: &UserId,
) -> Result<RebornOperatorToolInfo, ProductSurfaceError> {
    // Look up within the CALLER-filtered catalog so a foreign user-private
    // tool reads as an unknown key (same masking as list), never disclosing
    // that it exists or letting a member set a permission on it (#5459 P1).
    config
        .tool_catalog
        .list_operator_tools(caller)
        .await
        .into_iter()
        .find(|tool| tool.capability_id.as_str() == raw_capability_id)
        .ok_or_else(|| operator_config_unknown_key_error("key"))
}

pub(super) async fn tool_config_entry(
    config: &RebornOperatorApprovalConfig,
    scope: &ResourceScope,
    tool: &RebornOperatorToolInfo,
) -> Result<RebornOperatorConfigEntry, ProductSurfaceError> {
    let context =
        operator_tool_permission_context(config, scope, std::slice::from_ref(tool)).await?;
    tool_config_entry_with_context(&context, tool).await
}

pub(super) async fn tool_config_entry_with_context(
    context: &OperatorToolPermissionContext,
    tool: &RebornOperatorToolInfo,
) -> Result<RebornOperatorConfigEntry, ProductSurfaceError> {
    let (state, source) = effective_tool_permission(context, tool).await?;
    let default_state = default_tool_permission_state(tool.default_permission);
    let locked = tool_permission_locked(tool);
    let value = serde_json::json!({
        "name": tool.capability_id.as_str(),
        "description": tool.description.as_ref(),
        "state": tool_permission_state_wire(state),
        "default_state": tool_permission_state_wire(if locked && hard_floor_tool(tool) {
            ToolPermissionState::AskEachTime
        } else {
            default_state
        }),
        "locked": locked,
        "effective_source": source,
    });
    Ok(RebornOperatorConfigEntry {
        key: format!("{TOOL_CONFIG_PREFIX}{}", tool.capability_id),
        value,
        source: source.to_string(),
        redacted: false,
        mutable: !locked,
    })
}

pub(super) struct OperatorToolPermissionContext {
    global_auto_approve: bool,
    overrides: HashMap<CapabilityId, ToolPermissionOverride>,
    persistent_active: HashSet<CapabilityId>,
}

pub(super) async fn operator_tool_permission_context(
    config: &RebornOperatorApprovalConfig,
    scope: &ResourceScope,
    tools: &[RebornOperatorToolInfo],
) -> Result<OperatorToolPermissionContext, ProductSurfaceError> {
    let operator_scope = operator_tool_permission_scope(scope);
    let global_auto_approve = config
        .auto_approve
        .is_enabled(&operator_scope)
        .await
        .map_err(operator_config_store_error)?;
    let override_records = try_join_all(
        tools
            .iter()
            .filter(|tool| !tool_permission_locked(tool))
            .map(|tool| {
                let key =
                    ToolPermissionOverrideKey::new(&operator_scope, tool.capability_id.clone());
                async move {
                    config
                        .overrides
                        .get(&key)
                        .await
                        .map(|record| (tool.capability_id.clone(), record))
                        .map_err(operator_config_store_error)
                }
            }),
    )
    .await?;
    let overrides = override_records
        .into_iter()
        .filter_map(|(capability_id, record)| record.map(|record| (capability_id, record.state)))
        .collect::<HashMap<_, _>>();
    let persistent_records = try_join_all(
        tools
            .iter()
            .filter(|tool| {
                !tool_permission_locked(tool) && !overrides.contains_key(&tool.capability_id)
            })
            .map(|tool| {
                let operator_scope = operator_scope.clone();
                async move {
                    persistent_user_policy_active(config, &operator_scope, tool)
                        .await
                        .map(|active| (tool.capability_id.clone(), active))
                }
            }),
    )
    .await?;
    let persistent_active = persistent_records
        .into_iter()
        .filter_map(|(capability_id, active)| active.then_some(capability_id))
        .collect();
    Ok(OperatorToolPermissionContext {
        global_auto_approve,
        overrides,
        persistent_active,
    })
}

pub(super) async fn effective_tool_permission(
    context: &OperatorToolPermissionContext,
    tool: &RebornOperatorToolInfo,
) -> Result<(ToolPermissionState, &'static str), ProductSurfaceError> {
    if tool.default_permission == PermissionMode::Deny {
        return Ok((ToolPermissionState::Disabled, "default"));
    }
    if hard_floor_tool(tool) {
        return Ok((ToolPermissionState::AskEachTime, "locked"));
    }

    if let Some(state) = context.overrides.get(&tool.capability_id) {
        return Ok((state.as_state(), "override"));
    }

    if context.persistent_active.contains(&tool.capability_id) {
        return Ok((ToolPermissionState::AlwaysAllow, "override"));
    }

    if permission_mode_allows_persistent_approval(tool.default_permission) {
        if context.global_auto_approve {
            return Ok((ToolPermissionState::AlwaysAllow, "global"));
        }
        return Ok((ToolPermissionState::AskEachTime, "global"));
    }

    Ok((
        default_tool_permission_state(tool.default_permission),
        "default",
    ))
}

pub(super) async fn persistent_user_policy_active(
    config: &RebornOperatorApprovalConfig,
    operator_scope: &ResourceScope,
    tool: &RebornOperatorToolInfo,
) -> Result<bool, ProductSurfaceError> {
    let key = persistent_user_policy_key(operator_scope, tool);
    Ok(config
        .persistent_policies
        .lookup(&key)
        .await
        .map_err(operator_config_store_error)?
        .and_then(|policy| policy.active_grant())
        .is_some())
}

pub(super) fn persistent_user_policy_key(
    scope: &ResourceScope,
    tool: &RebornOperatorToolInfo,
) -> PersistentApprovalPolicyKey {
    let operator_scope = operator_tool_permission_scope(scope);
    PersistentApprovalPolicyKey::new(
        &operator_scope,
        PersistentApprovalAction::Dispatch,
        tool.capability_id.clone(),
        Principal::Extension(tool.provider.clone()),
    )
}

pub(super) fn operator_tool_permission_scope(scope: &ResourceScope) -> ResourceScope {
    scope.tenant_user_settings_scope()
}

pub(super) fn tool_permission_locked(tool: &RebornOperatorToolInfo) -> bool {
    tool.default_permission == PermissionMode::Deny || hard_floor_tool(tool)
}

pub(super) fn hard_floor_tool(tool: &RebornOperatorToolInfo) -> bool {
    tool.effects.iter().any(|effect| {
        matches!(
            effect,
            EffectKind::Financial | EffectKind::ModifyApproval | EffectKind::ModifyBudget
        )
    })
}

pub(super) fn default_tool_permission_state(permission: PermissionMode) -> ToolPermissionState {
    match permission {
        PermissionMode::Allow | PermissionMode::Ask => ToolPermissionState::AskEachTime,
        PermissionMode::Deny => ToolPermissionState::Disabled,
    }
}

pub(super) fn tool_permission_state_wire(state: ToolPermissionState) -> &'static str {
    match state {
        ToolPermissionState::AlwaysAllow => "always_allow",
        ToolPermissionState::AskEachTime => "ask_each_time",
        ToolPermissionState::Disabled => "disabled",
    }
}

pub(super) enum ToolPermissionUpdate {
    Default,
    State(ToolPermissionState),
}

pub(super) fn parse_tool_permission_state(
    value: &serde_json::Value,
) -> Result<ToolPermissionUpdate, ProductSurfaceError> {
    let raw = value
        .as_str()
        .or_else(|| value.get("state").and_then(serde_json::Value::as_str))
        .ok_or_else(|| operator_config_invalid_value("state"))?;
    match raw {
        "default" => Ok(ToolPermissionUpdate::Default),
        "always_allow" => Ok(ToolPermissionUpdate::State(
            ToolPermissionState::AlwaysAllow,
        )),
        // Backward-compatible read alias from earlier Tools UI payloads. The
        // service always writes the canonical `ask_each_time` wire value.
        "ask_each_time" | "ask" => Ok(ToolPermissionUpdate::State(
            ToolPermissionState::AskEachTime,
        )),
        "disabled" => Ok(ToolPermissionUpdate::State(ToolPermissionState::Disabled)),
        _ => Err(operator_config_invalid_value("state")),
    }
}

pub(super) async fn apply_tool_permission_state(
    config: &RebornOperatorApprovalConfig,
    scope: &ResourceScope,
    actor: &TurnActor,
    tool: &RebornOperatorToolInfo,
    update: ToolPermissionUpdate,
) -> Result<(), ProductSurfaceError> {
    match update {
        ToolPermissionUpdate::Default => {
            let operator_scope = operator_tool_permission_scope(scope);
            match config
                .persistent_policies
                .revoke(&persistent_user_policy_key(&operator_scope, tool))
                .await
            {
                Ok(_) | Err(PersistentApprovalPolicyError::UnknownPolicy) => {}
                Err(error) => return Err(operator_config_store_error(error)),
            }
            config
                .overrides
                .clear(&ToolPermissionOverrideKey::new(
                    &operator_scope,
                    tool.capability_id.clone(),
                ))
                .await
                .map_err(operator_config_store_error)?;
        }
        ToolPermissionUpdate::State(ToolPermissionState::AlwaysAllow) => {
            let operator_scope = operator_tool_permission_scope(scope);
            config
                .persistent_policies
                .allow(PersistentApprovalPolicyInput {
                    scope: operator_scope.clone(),
                    action: PersistentApprovalAction::Dispatch,
                    capability_id: tool.capability_id.clone(),
                    grantee: Principal::Extension(tool.provider.clone()),
                    approved_by: Principal::User(actor.user_id.clone()),
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
                .map_err(operator_config_store_error)?;
            config
                .overrides
                .clear(&ToolPermissionOverrideKey::new(
                    &operator_scope,
                    tool.capability_id.clone(),
                ))
                .await
                .map_err(operator_config_store_error)?;
        }
        ToolPermissionUpdate::State(state @ ToolPermissionState::AskEachTime)
        | ToolPermissionUpdate::State(state @ ToolPermissionState::Disabled) => {
            let operator_scope = operator_tool_permission_scope(scope);
            let override_state = match state {
                ToolPermissionState::AskEachTime => ToolPermissionOverride::AskEachTime,
                ToolPermissionState::Disabled => ToolPermissionOverride::Disabled,
                ToolPermissionState::AlwaysAllow => unreachable!(),
            };
            match config
                .persistent_policies
                .revoke(&persistent_user_policy_key(&operator_scope, tool))
                .await
            {
                Ok(_) | Err(PersistentApprovalPolicyError::UnknownPolicy) => {}
                Err(error) => return Err(operator_config_store_error(error)),
            }
            config
                .overrides
                .set(ToolPermissionOverrideInput {
                    scope: operator_scope.clone(),
                    capability_id: tool.capability_id.clone(),
                    state: override_state,
                    updated_by: Principal::User(actor.user_id.clone()),
                })
                .await
                .map_err(operator_config_store_error)?;
        }
    }
    Ok(())
}

pub(super) fn operator_config_surface_not_wired_diagnostic() -> RebornOperatorConfigDiagnostic {
    RebornOperatorConfigDiagnostic {
        key: "*".to_string(),
        severity: RebornOperatorConfigDiagnosticSeverity::Error,
        reason_code: "operator_config_service_not_wired".to_string(),
        message: "Operator config diagnostics are available, but the effective config service is not wired yet.".to_string(),
        owning_area: RebornOperatorArea::Config,
        remediation: "Use bootstrap config, environment variables, or existing CLI setup until the operator config service is enabled.".to_string(),
    }
}

pub(super) fn operator_config_validation_diagnostics(
    keys: Vec<String>,
) -> Vec<RebornOperatorConfigDiagnostic> {
    let keys = if keys.is_empty() {
        vec!["*".to_string()]
    } else {
        keys
    };

    keys.into_iter()
        .map(operator_config_key_diagnostic)
        .collect()
}

pub(super) fn operator_config_key_diagnostic(key: String) -> RebornOperatorConfigDiagnostic {
    let normalized = key.to_ascii_lowercase();
    let is_secret = ["api_key", "credential", "password", "secret", "token"]
        .iter()
        .any(|marker| normalized.contains(marker));

    let (reason_code, message, remediation) = if key == "*" {
        (
            "operator_config_service_not_wired",
            "Operator config validation is available, but the effective config service is not wired yet.",
            "Use bootstrap config, environment variables, or existing CLI setup until the operator config service is enabled.",
        )
    } else if is_secret {
        (
            "operator_config_secret_not_wired",
            "Secret-backed operator config is not writable through the operator API yet.",
            "Store secrets through the configured secret provider or bootstrap environment until the operator secrets flow is enabled.",
        )
    } else if normalized.starts_with("deprecated.") || normalized.starts_with("legacy.") {
        (
            "operator_config_deprecated",
            "This operator config key is deprecated and is not applied by the Reborn runtime.",
            "Move the setting to the current config key before relying on operator-managed startup.",
        )
    } else if normalized.starts_with("bootstrap.") {
        (
            "operator_config_immutable",
            "Bootstrap config is immutable from the browser operator API.",
            "Change this setting in bootstrap config and restart the host process.",
        )
    } else if matches!(
        normalized.as_str(),
        "provider.default" | "model.default" | "profile.default"
    ) {
        (
            "operator_config_not_wired",
            "This parsed operator config key is not wired into runtime behavior yet.",
            "Keep using the existing setup path for this setting until effective config persistence is enabled.",
        )
    } else {
        (
            "operator_config_unknown_key",
            "This operator config key is not recognized by the current Reborn runtime.",
            "Remove the key or rename it to a documented operator config key.",
        )
    };

    RebornOperatorConfigDiagnostic {
        key,
        severity: RebornOperatorConfigDiagnosticSeverity::Error,
        reason_code: reason_code.to_string(),
        message: message.to_string(),
        owning_area: RebornOperatorArea::Config,
        remediation: remediation.to_string(),
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub async fn global_auto_approve_enabled(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<bool, ProductSurfaceError> {
        let Some(config) = &self.operator_approval_config else {
            return Ok(false);
        };
        let scope = caller_resource_scope(&caller);
        let operator_scope = operator_tool_permission_scope(&scope);
        config
            .auto_approve
            .is_enabled(&operator_scope)
            .await
            .map_err(|error| {
                tracing::debug!(
                    tenant_id = %caller.tenant_id,
                    user_id = %caller.user_id,
                    error = %error,
                    "failed to read global auto-approve setting"
                );
                operator_config_store_error(error)
            })
    }

    pub async fn set_operator_config_key(
        &self,
        caller: ProductSurfaceCaller,
        key: String,
        request: RebornOperatorConfigSetRequest,
    ) -> Result<RebornOperatorConfigGetResponse, ProductSurfaceError> {
        let Some(config) = &self.operator_approval_config else {
            let _ = (caller, key, request);
            return Err(ProductSurfaceError::service_unavailable(false));
        };
        let scope = caller_resource_scope(&caller);
        if key == AUTO_APPROVE_CONFIG_KEY {
            let enabled = request
                .value
                .as_bool()
                .ok_or_else(|| operator_config_invalid_value("value"))?;
            let resolution = self
                .invoke_json_capability(
                    caller.clone(),
                    OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY,
                    serde_json::json!({ "enabled": enabled }),
                    operator_config_auto_approve_activity_id(&caller, enabled),
                )
                .await?;
            operator_config_mutation_succeeded(resolution)?;
            return self
                .build_operator_config_key_view(caller, serde_json::json!({ "key": key }))
                .await;
        }

        let actor = caller.actor();
        let entry = if let Some(capability_id) = key.strip_prefix(TOOL_CONFIG_PREFIX) {
            let tool = find_operator_tool(config, capability_id, &scope.user_id).await?;
            if tool_permission_locked(&tool) {
                return Err(operator_config_invalid_value("state"));
            }
            let state = parse_tool_permission_state(&request.value)?;
            apply_tool_permission_state(config, &scope, &actor, &tool, state).await?;
            tool_config_entry(config, &scope, &tool).await?
        } else {
            return Err(operator_config_unknown_key_error("key"));
        };
        Ok(RebornOperatorConfigGetResponse { entry })
    }
}
