//! Operator configuration, tool permissions, and operator diagnostics.

use super::*;

struct RecordingOperatorStatusService {
    response: RebornOperatorStatusResponse,
    callers: Mutex<Vec<ProductSurfaceCaller>>,
}

impl RecordingOperatorStatusService {
    fn new(response: RebornOperatorStatusResponse) -> Self {
        Self {
            response,
            callers: Mutex::new(Vec::new()),
        }
    }

    fn callers(&self) -> Vec<ProductSurfaceCaller> {
        self.callers.lock().expect("lock").clone()
    }
}

#[async_trait]
impl OperatorStatusService for RecordingOperatorStatusService {
    async fn status(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornOperatorStatusResponse, ProductSurfaceError> {
        self.callers.lock().expect("lock").push(caller);
        Ok(self.response.clone())
    }
}

struct FailingAllowPersistentApprovalPolicyStore;

#[async_trait]
impl PersistentApprovalPolicyStorePort for FailingAllowPersistentApprovalPolicyStore {
    async fn allow(
        &self,
        _input: PersistentApprovalPolicyInput,
    ) -> Result<PersistentApprovalPolicy, PersistentApprovalPolicyError> {
        Err(PersistentApprovalPolicyError::Filesystem(
            "persistent policy store unavailable".to_string(),
        ))
    }

    async fn lookup(
        &self,
        _key: &PersistentApprovalPolicyKey,
    ) -> Result<Option<PersistentApprovalPolicy>, PersistentApprovalPolicyError> {
        Ok(None)
    }

    async fn revoke(
        &self,
        _key: &PersistentApprovalPolicyKey,
    ) -> Result<PersistentApprovalPolicy, PersistentApprovalPolicyError> {
        Err(PersistentApprovalPolicyError::UnknownPolicy)
    }

    async fn revoke_if_source_approval_request(
        &self,
        _key: &PersistentApprovalPolicyKey,
        _source_approval_request_id: ApprovalRequestId,
    ) -> Result<Option<PersistentApprovalPolicy>, PersistentApprovalPolicyError> {
        Ok(None)
    }
}

#[derive(Debug, Default)]
struct RecordingAutoApproveSettingStore {
    get_keys: Mutex<Vec<AutoApproveSettingKey>>,
}

impl RecordingAutoApproveSettingStore {
    fn get_keys(&self) -> Vec<AutoApproveSettingKey> {
        self.get_keys.lock().expect("lock").clone()
    }
}

#[async_trait]
impl AutoApproveSettingStorePort for RecordingAutoApproveSettingStore {
    async fn set(
        &self,
        input: AutoApproveSettingInput,
    ) -> Result<AutoApproveSettingRecord, CapabilityPermissionStoreError> {
        let key = AutoApproveSettingKey::from_resource_scope(&input.scope);
        let now = Utc::now();
        Ok(AutoApproveSettingRecord {
            key,
            enabled: input.enabled,
            updated_by: input.updated_by,
            created_at: now,
            updated_at: now,
        })
    }

    async fn get(
        &self,
        key: &AutoApproveSettingKey,
    ) -> Result<Option<AutoApproveSettingRecord>, CapabilityPermissionStoreError> {
        self.get_keys.lock().expect("lock").push(key.clone());
        Ok(None)
    }
}

fn services_with_operator_approval_config() -> OperatorConfigServices {
    services_with_operator_approval_config_parts().0
}

fn services_with_operator_approval_config_parts() -> (
    OperatorConfigServices,
    Arc<ironclaw_approvals::PersistentApprovalPolicyStore<ironclaw_filesystem::InMemoryBackend>>,
) {
    let persistent_policies = Arc::new(
        ironclaw_approvals::test_support::in_memory_backed_persistent_approval_policy_store(),
    );
    let policy_store: Arc<dyn PersistentApprovalPolicyStorePort> = persistent_policies.clone();
    let services = services_with_operator_approval_config_policy_store(policy_store);
    (services, persistent_policies)
}

fn services_with_operator_approval_config_policy_store(
    persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
) -> OperatorConfigServices {
    services_with_operator_approval_config_stores(
        Arc::new(ironclaw_approvals::test_support::in_memory_backed_auto_approve_setting_store()),
        persistent_policies,
    )
}

fn services_with_operator_approval_config_stores(
    auto_approve: Arc<dyn AutoApproveSettingStorePort>,
    persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
) -> OperatorConfigServices {
    let overrides: Arc<dyn CapabilityPermissionOverrideStorePort> = Arc::new(
        ironclaw_approvals::test_support::in_memory_backed_capability_permission_override_store(),
    );
    let tools = Arc::new(operator_config_test_tools());
    RebornServices::new_with_product_ports(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
        OperatorConfigAutoApproveInvoker {
            auto_approve: Arc::clone(&auto_approve),
            overrides: Arc::clone(&overrides),
            persistent_policies: Arc::clone(&persistent_policies),
            tools: Arc::clone(&tools),
        },
        ironclaw_assistant::UnavailableRebornViewProvider,
    )
    .with_operator_approval_config(
        overrides,
        auto_approve,
        persistent_policies.clone(),
        Arc::new(StaticOperatorToolCatalogForTest {
            tools: tools.as_ref().clone(),
        }),
    )
}

fn operator_config_entry_value<'a>(
    response: &'a ironclaw_assistant::RebornOperatorConfigListResponse,
    key: &str,
) -> &'a serde_json::Value {
    &response
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .unwrap_or_else(|| panic!("{key} entry"))
        .value
}

async fn query_operator_config_list<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
) -> RebornOperatorConfigListResponse {
    let page = query_product_surface_page(
        services,
        caller,
        RebornViewQuery {
            view_id: OPERATOR_CONFIG_LIST_VIEW.id.to_string(),
            params: json!({}),
            cursor: None,
        },
    )
    .await
    .expect("operator config list view");
    serde_json::from_value(page.payload).expect("operator config list payload")
}

async fn query_operator_config_key<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    key: &str,
) -> Result<RebornOperatorConfigGetResponse, ProductSurfaceError> {
    let page = query_product_surface_page(
        services,
        caller,
        RebornViewQuery {
            view_id: OPERATOR_CONFIG_KEY_VIEW.id.to_string(),
            params: json!({ "key": key }),
            cursor: None,
        },
    )
    .await?;
    serde_json::from_value(page.payload).map_err(ProductSurfaceError::internal_from)
}

fn operator_policy_scope_for_test(tenant_id: &str, user_id: &str) -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new(tenant_id).expect("tenant id"),
        user_id: UserId::new(user_id).expect("user id"),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

#[tokio::test]
async fn operator_config_reads_provider_grantee_policies_as_always_allow() {
    let (services, persistent_policies) = services_with_operator_approval_config_parts();
    let operator_scope = operator_policy_scope_for_test("tenant-alpha", "user-alpha");

    for (capability_id, provider) in [
        ("nearai.web_search", "nearai"),
        ("github.get_repo", "github"),
        ("google-calendar.list_events", "google-calendar"),
    ] {
        persistent_policies
            .allow(PersistentApprovalPolicyInput {
                scope: operator_scope.clone(),
                action: PersistentApprovalAction::Dispatch,
                capability_id: CapabilityId::new(capability_id).expect("capability id"),
                grantee: Principal::Extension(ExtensionId::new(provider).expect("extension id")),
                approved_by: Principal::User(UserId::new("user-alpha").expect("user id")),
                constraints: ironclaw_host_api::capability::GrantConstraints {
                    allowed_effects: vec![EffectKind::DispatchCapability],
                    mounts: Default::default(),
                    network: Default::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
                source_approval_request_id: Some(ApprovalRequestId::new()),
            })
            .await
            .expect("seed provider-grantee always-allow policy");
    }

    let config = query_operator_config_list(&services, caller()).await;
    for capability_id in [
        "nearai.web_search",
        "github.get_repo",
        "google-calendar.list_events",
    ] {
        let value = operator_config_entry_value(&config, &format!("tool.{capability_id}"));
        assert_eq!(value["state"], "always_allow");
        assert_eq!(value["effective_source"], "override");
    }
}

#[tokio::test]
async fn global_auto_approve_enabled_scopes_read_by_caller_tenant_and_user() {
    let auto_approve = Arc::new(RecordingAutoApproveSettingStore::default());
    let services = services_with_operator_approval_config_stores(
        auto_approve.clone(),
        Arc::new(
            ironclaw_approvals::test_support::in_memory_backed_persistent_approval_policy_store(),
        ),
    );
    let caller = ProductSurfaceCaller::new(
        TenantId::new("tenant-scope").expect("tenant"),
        UserId::new("user-scope").expect("user"),
        Some(AgentId::new("agent-scope").expect("agent")),
        Some(ProjectId::new("project-scope").expect("project")),
    );

    let enabled = services
        .global_auto_approve_enabled(caller)
        .await
        .expect("global auto approve read");

    assert!(
        enabled,
        "unset auto-approve should resolve through the default"
    );
    let keys = auto_approve.get_keys();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].tenant_id.as_str(), "tenant-scope");
    assert_eq!(keys[0].user_id.as_str(), "user-scope");
}

#[tokio::test]
async fn operator_config_reads_and_writes_auto_approve_and_tool_permissions() {
    let (services, persistent_policies) = services_with_operator_approval_config_parts();

    let initial = query_operator_config_list(&services, caller()).await;
    assert_eq!(
        operator_config_entry_value(&initial, "agent.auto_approve_tools"),
        &json!(true)
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.alpha")["state"],
        "always_allow"
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.alpha")["effective_source"],
        "global"
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.default_allow")["state"],
        "always_allow",
        "follow-global tools auto-approve while global auto-approve defaults on"
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.default_allow")["effective_source"],
        "global"
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.locked")["state"],
        "disabled"
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.financial")["state"],
        "ask_each_time"
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.financial")["locked"],
        true
    );
    assert_eq!(
        operator_config_entry_value(&initial, "tool.tool.financial")["effective_source"],
        "locked"
    );

    let financial_error = services
        .set_operator_config_key(
            caller(),
            "tool.tool.financial".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "always_allow" }),
            },
        )
        .await
        .expect_err("hard-floor tool cannot be made always-allow");
    assert_eq!(
        financial_error.code,
        ProductSurfaceErrorCode::InvalidRequest
    );
    assert_eq!(financial_error.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(
        financial_error.validation_code,
        Some(ProductSurfaceValidationCode::InvalidValue)
    );

    services
        .set_operator_config_key(
            caller(),
            "agent.auto_approve_tools".to_string(),
            RebornOperatorConfigSetRequest { value: json!(true) },
        )
        .await
        .expect("enable global auto approve");

    let globally_allowed = query_operator_config_key(&services, caller(), "tool.tool.alpha")
        .await
        .expect("tool config");
    assert_eq!(globally_allowed.entry.value["state"], "always_allow");
    assert_eq!(globally_allowed.entry.value["effective_source"], "global");

    let default_allow_global =
        query_operator_config_key(&services, caller(), "tool.tool.default_allow")
            .await
            .expect("default-allow tool config");
    assert_eq!(default_allow_global.entry.value["state"], "always_allow");
    assert_eq!(
        default_allow_global.entry.value["effective_source"],
        "global"
    );

    let ask_override = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "ask_each_time" }),
            },
        )
        .await
        .expect("ask each time override");
    assert_eq!(ask_override.entry.value["state"], "ask_each_time");
    assert_eq!(ask_override.entry.value["effective_source"], "override");

    let follows_global = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "default" }),
            },
        )
        .await
        .expect("clear tool override");
    assert_eq!(follows_global.entry.value["state"], "always_allow");
    assert_eq!(follows_global.entry.value["effective_source"], "global");

    let disabled = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "disabled" }),
            },
        )
        .await
        .expect("disable tool");
    assert_eq!(disabled.entry.value["state"], "disabled");
    assert_eq!(disabled.entry.value["effective_source"], "override");

    let allowed = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "always_allow" }),
            },
        )
        .await
        .expect("always allow tool");
    assert_eq!(allowed.entry.value["state"], "always_allow");
    assert_eq!(allowed.entry.value["effective_source"], "override");

    let operator_scope = operator_policy_scope_for_test("tenant-alpha", "user-alpha");
    let extension_key = PersistentApprovalPolicyKey::new(
        &operator_scope,
        PersistentApprovalAction::Dispatch,
        CapabilityId::new("tool.alpha").expect("capability id"),
        Principal::Extension(ExtensionId::new("extension.alpha").expect("extension id")),
    );
    assert!(
        persistent_policies
            .lookup(&extension_key)
            .await
            .expect("lookup extension-grantee policy")
            .and_then(|policy| policy.active_grant())
            .is_some(),
        "settings-page always_allow must write the provider extension grantee"
    );

    let user_key = PersistentApprovalPolicyKey::new(
        &operator_scope,
        PersistentApprovalAction::Dispatch,
        CapabilityId::new("tool.alpha").expect("capability id"),
        Principal::User(UserId::new("user-alpha").expect("user id")),
    );
    assert!(
        persistent_policies
            .lookup(&user_key)
            .await
            .expect("lookup user-grantee policy")
            .is_none(),
        "settings-page always_allow must not write a user-grantee policy"
    );

    let reset = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "default" }),
            },
        )
        .await
        .expect("clear persistent always-allow");
    assert_eq!(reset.entry.value["state"], "always_allow");
    assert_eq!(reset.entry.value["effective_source"], "global");
    assert!(
        persistent_policies
            .lookup(&extension_key)
            .await
            .expect("lookup extension-grantee policy after reset")
            .and_then(|policy| policy.active_grant())
            .is_none(),
        "default must clear the settings-page persistent always-allow policy"
    );
}

#[tokio::test]
async fn operator_config_reads_are_available_as_product_views() {
    let services = services_with_operator_approval_config();

    let list_page = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: OPERATOR_CONFIG_LIST_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("operator config list view");
    let list: RebornOperatorConfigListResponse =
        serde_json::from_value(list_page.payload).expect("operator config list payload");
    assert_eq!(
        operator_config_entry_value(&list, "agent.auto_approve_tools"),
        &json!(true)
    );

    let key_page = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: OPERATOR_CONFIG_KEY_VIEW.id.to_string(),
                params: json!({ "key": "tool.tool.alpha" }),
                cursor: None,
            },
        )
        .await
        .expect("operator config key view");
    let key: RebornOperatorConfigGetResponse =
        serde_json::from_value(key_page.payload).expect("operator config key payload");
    assert_eq!(key.entry.key, "tool.tool.alpha");
    assert_eq!(key.entry.value["state"], "always_allow");
    assert_eq!(key.entry.value["effective_source"], "global");

    let validate_page = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: OPERATOR_CONFIG_VALIDATE_VIEW.id.to_string(),
                params: json!({ "keys": ["provider.default", "legacy.old"] }),
                cursor: None,
            },
        )
        .await
        .expect("operator config validate view");
    let validate: RebornOperatorConfigValidateResponse =
        serde_json::from_value(validate_page.payload).expect("operator config validate payload");
    assert!(!validate.valid);
    assert!(validate.diagnostics.iter().any(|diagnostic| {
        diagnostic.reason_code == "operator_config_deprecated" && diagnostic.key == "legacy.old"
    }));
}

#[tokio::test]
async fn operator_config_is_scoped_by_tenant_and_user() {
    let services = services_with_operator_approval_config();
    let alice_tenant_a = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("alice").expect("user"),
        Some(AgentId::new("agent-alpha").expect("agent")),
        Some(ProjectId::new("project-alpha").expect("project")),
    );
    let bob_tenant_a = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("bob").expect("user"),
        Some(AgentId::new("agent-alpha").expect("agent")),
        Some(ProjectId::new("project-alpha").expect("project")),
    );
    let alice_tenant_a_other_project = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new("alice").expect("user"),
        Some(AgentId::new("agent-beta").expect("agent")),
        Some(ProjectId::new("project-beta").expect("project")),
    );
    let alice_tenant_b = ProductSurfaceCaller::new(
        TenantId::new("tenant-beta").expect("tenant"),
        UserId::new("alice").expect("user"),
        Some(AgentId::new("agent-alpha").expect("agent")),
        Some(ProjectId::new("project-alpha").expect("project")),
    );

    services
        .set_operator_config_key(
            alice_tenant_a.clone(),
            "agent.auto_approve_tools".to_string(),
            // Set the non-default (off) so isolation is provable: other
            // users/tenants must still read the default (on), not this value.
            RebornOperatorConfigSetRequest {
                value: json!(false),
            },
        )
        .await
        .expect("alice disables auto approve in tenant alpha");
    services
        .set_operator_config_key(
            alice_tenant_a.clone(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "disabled" }),
            },
        )
        .await
        .expect("alice disables tool in tenant alpha");

    let alice_alpha = query_operator_config_key(&services, alice_tenant_a, "tool.tool.alpha")
        .await
        .expect("alice alpha tool config");
    assert_eq!(alice_alpha.entry.value["state"], "disabled");
    assert_eq!(alice_alpha.entry.value["effective_source"], "override");

    let alice_alpha_other_project =
        query_operator_config_list(&services, alice_tenant_a_other_project).await;
    assert_eq!(
        operator_config_entry_value(&alice_alpha_other_project, "agent.auto_approve_tools"),
        &json!(false),
        "auto-approve settings are scoped by tenant/user, not agent/project"
    );
    assert_eq!(
        operator_config_entry_value(&alice_alpha_other_project, "tool.tool.alpha")["state"],
        "disabled",
        "tool overrides are scoped by tenant/user, not agent/project"
    );
    assert_eq!(
        operator_config_entry_value(&alice_alpha_other_project, "tool.tool.alpha")["effective_source"],
        "override"
    );

    for caller in [bob_tenant_a, alice_tenant_b] {
        let config = query_operator_config_list(&services, caller).await;
        assert_eq!(
            operator_config_entry_value(&config, "agent.auto_approve_tools"),
            &json!(true),
            "auto-approve must not leak across user or tenant"
        );
        assert_eq!(
            operator_config_entry_value(&config, "tool.tool.alpha")["state"],
            "always_allow",
            "tool override must not leak across user or tenant (follows global default-on)"
        );
        assert_eq!(
            operator_config_entry_value(&config, "tool.tool.alpha")["effective_source"],
            "global"
        );
    }
}

#[tokio::test]
async fn operator_config_preserves_override_when_always_allow_policy_write_fails() {
    let services = services_with_operator_approval_config_policy_store(Arc::new(
        FailingAllowPersistentApprovalPolicyStore,
    ));

    let ask_override = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "ask_each_time" }),
            },
        )
        .await
        .expect("ask each time override");
    assert_eq!(ask_override.entry.value["state"], "ask_each_time");
    assert_eq!(ask_override.entry.value["effective_source"], "override");

    let error = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "always_allow" }),
            },
        )
        .await
        .expect_err("persistent policy write failure");
    assert_eq!(error.code, ProductSurfaceErrorCode::Internal);

    let preserved = query_operator_config_key(&services, caller(), "tool.tool.alpha")
        .await
        .expect("tool config after failed always_allow");
    assert_eq!(preserved.entry.value["state"], "ask_each_time");
    assert_eq!(
        preserved.entry.value["effective_source"], "override",
        "failed policy writes must not clear the existing per-tool override"
    );
}

#[tokio::test]
async fn operator_config_reports_unknown_keys_distinct_from_invalid_values() {
    let services = services_with_operator_approval_config();

    let unknown_key = query_operator_config_key(&services, caller(), "tool.tool.missing")
        .await
        .expect_err("unknown tool key");
    assert_eq!(
        unknown_key.validation_code,
        Some(ProductSurfaceValidationCode::UnknownKey)
    );

    let invalid_value = services
        .set_operator_config_key(
            caller(),
            "tool.tool.alpha".to_string(),
            RebornOperatorConfigSetRequest {
                value: json!({ "state": "sometimes" }),
            },
        )
        .await
        .expect_err("invalid tool state");
    assert_eq!(
        invalid_value.validation_code,
        Some(ProductSurfaceValidationCode::InvalidValue)
    );
}

#[tokio::test]
async fn operator_diagnostics_aggregates_status_setup_and_config_reasons() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let status_service = Arc::new(RecordingOperatorStatusService::new(
        RebornOperatorStatusResponse {
            generated_at: Utc::now(),
            overall: RebornOperatorStatusState::Blocked,
            checks: vec![
                RebornOperatorStatusCheck {
                    id: "storage".to_string(),
                    status: RebornOperatorStatusState::Blocked,
                    severity: RebornOperatorStatusSeverity::Critical,
                    summary: "storage backend is unavailable".to_string(),
                    remediation: Some("repair storage configuration".to_string()),
                },
                RebornOperatorStatusCheck {
                    id: "provider_model".to_string(),
                    status: RebornOperatorStatusState::Ready,
                    severity: RebornOperatorStatusSeverity::Info,
                    summary: "provider and model are configured".to_string(),
                    remediation: None,
                },
                RebornOperatorStatusCheck {
                    id: "sk-secret-token".to_string(),
                    status: RebornOperatorStatusState::Blocked,
                    severity: RebornOperatorStatusSeverity::Critical,
                    summary: "failed with sk-test1234567890 at /home/alice/.env".to_string(),
                    remediation: Some(
                        "inspect /home/alice/.ssh/id_ed25519 and credential token".to_string(),
                    ),
                },
                RebornOperatorStatusCheck {
                    id: "workspace_path".to_string(),
                    status: RebornOperatorStatusState::Blocked,
                    severity: RebornOperatorStatusSeverity::Warning,
                    summary: "artifact staged at /workspace/ironclaw/.env".to_string(),
                    remediation: Some("remove /workspace/ironclaw/secrets.env".to_string()),
                },
            ],
        },
    ));
    let services = services_with_setup_llm_config(llm_config.clone())
        .with_operator_status_service(status_service.clone());
    let diagnostics_caller =
        caller_for_user_with_project("user-diagnostics", Some("project-diagnostics"))
            .with_operator_config(true);

    let page = services
        .query(
            diagnostics_caller.clone(),
            RebornViewQuery {
                view_id: OPERATOR_DIAGNOSTICS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("operator diagnostics");
    let response: RebornOperatorCommandPlaneResponse =
        serde_json::from_value(page.payload).expect("operator diagnostics payload");

    assert_eq!(response.area.as_str(), "diagnostics");
    assert_eq!(response.status, RebornOperatorSurfaceStatus::Unavailable);
    assert!(response.operator_status.is_some());
    let reason_codes = response
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.reason_code.as_str())
        .collect::<Vec<_>>();
    assert!(reason_codes.contains(&"operator_doctor_storage_blocked"));
    assert!(reason_codes.contains(&"operator_doctor_status_blocked"));
    assert!(reason_codes.contains(&"operator_doctor_workspace_path_blocked"));
    assert!(reason_codes.contains(&"operator_config_service_not_wired"));
    assert!(!reason_codes.contains(&"operator_doctor_provider_model_ready"));
    let rendered = serde_json::to_string(&response).expect("serialize diagnostics");
    assert!(!rendered.contains("sk-"));
    assert!(!rendered.contains("/home/"));
    assert!(!rendered.contains("/workspace/"));
    assert!(!rendered.contains(".ssh"));
    assert!(!rendered.contains("credential token"));
    assert!(
        response.diagnostics.iter().any(|diagnostic| {
            diagnostic.reason_code == "operator_doctor_status_blocked"
                && diagnostic.key == "[redacted operator status detail]"
                && diagnostic.message == "[redacted operator status detail]"
                && diagnostic.remediation == "[redacted operator status detail]"
        }),
        "sensitive status-derived diagnostic details should be redacted"
    );
    assert!(
        response.diagnostics.iter().any(|diagnostic| {
            diagnostic.reason_code == "operator_doctor_workspace_path_blocked"
                && diagnostic.key == "workspace_path"
                && diagnostic.message == "[redacted operator status detail]"
                && diagnostic.remediation == "[redacted operator status detail]"
        }),
        "/workspace/ status-derived diagnostic details should be redacted"
    );
    assert_eq!(status_service.callers(), vec![diagnostics_caller.clone()]);
    assert_eq!(llm_config.snapshot_callers(), vec![diagnostics_caller]);
}

#[tokio::test]
async fn operator_command_plane_reads_are_available_as_product_views() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services =
        services_with_setup_llm_config(llm_config).with_operator_status_service(Arc::new(
            StaticOperatorStatusService::new(RebornOperatorStatusResponse {
                generated_at: Utc::now(),
                overall: RebornOperatorStatusState::Ready,
                checks: Vec::new(),
            }),
        ));

    let operator = caller().with_operator_config(true);
    let diagnostics_page = services
        .query(
            operator.clone(),
            RebornViewQuery {
                view_id: OPERATOR_DIAGNOSTICS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("operator diagnostics view");
    let diagnostics: RebornOperatorCommandPlaneResponse =
        serde_json::from_value(diagnostics_page.payload).expect("diagnostics payload");
    assert_eq!(diagnostics.area.as_str(), "diagnostics");
    assert_eq!(diagnostics.status, RebornOperatorSurfaceStatus::Unavailable);
    assert!(
        diagnostics
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.reason_code == "operator_config_service_not_wired")
    );

    let status_page = services
        .query(
            operator,
            RebornViewQuery {
                view_id: OPERATOR_STATUS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("operator status view");
    let status: RebornOperatorCommandPlaneResponse =
        serde_json::from_value(status_page.payload).expect("status payload");
    assert_eq!(status.area.as_str(), "status");
    assert_eq!(status.status, RebornOperatorSurfaceStatus::Available);
    assert!(status.operator_status.is_some());
}

#[tokio::test]
async fn llm_config_snapshot_is_available_as_product_view() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());
    let snapshot_caller =
        caller_for_user_with_project("user-llm", Some("project-llm")).with_operator_config(true);

    let page = services
        .query(
            snapshot_caller.clone(),
            RebornViewQuery {
                view_id: LLM_CONFIG_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("llm config view");
    let snapshot: LlmConfigSnapshot =
        serde_json::from_value(page.payload).expect("llm config payload");

    assert_eq!(
        snapshot
            .active
            .as_ref()
            .map(|active| active.provider_id.as_str()),
        Some("openai")
    );
    assert_eq!(
        snapshot
            .active
            .as_ref()
            .and_then(|active| active.model.as_deref()),
        Some("gpt-5-mini")
    );
    assert_eq!(llm_config.snapshot_callers(), vec![snapshot_caller]);
}

#[tokio::test]
async fn operator_only_product_views_require_operator_config() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .query(
            caller_for_user_with_project("user-llm", Some("project-llm")),
            RebornViewQuery {
                view_id: LLM_CONFIG_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect_err("non-operator caller cannot query operator-only product views");

    assert_eq!(err.code, ProductSurfaceErrorCode::Forbidden);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ParticipantDenied);
    assert_eq!(
        llm_config.snapshot_callers(),
        Vec::<ProductSurfaceCaller>::new()
    );
}

#[tokio::test]
async fn operator_diagnostics_reports_setup_service_absence_without_failing_route() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_operator_status_service(Arc::new(StaticOperatorStatusService::new(
        RebornOperatorStatusResponse {
            generated_at: Utc::now(),
            overall: RebornOperatorStatusState::Ready,
            checks: Vec::new(),
        },
    )));

    let page = services
        .query(
            caller().with_operator_config(true),
            RebornViewQuery {
                view_id: OPERATOR_DIAGNOSTICS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("operator diagnostics");
    let response: RebornOperatorCommandPlaneResponse =
        serde_json::from_value(page.payload).expect("operator diagnostics payload");

    assert_eq!(response.area.as_str(), "diagnostics");
    assert!(response.diagnostics.iter().any(|diagnostic| {
        diagnostic.reason_code == "operator_setup_service_not_wired"
            && diagnostic.severity == RebornOperatorConfigDiagnosticSeverity::Error
    }));
}
