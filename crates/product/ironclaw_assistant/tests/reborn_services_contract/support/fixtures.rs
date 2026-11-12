//! Shared caller, scope, and seeding fixtures.

use crate::*;

pub(crate) fn caller() -> ProductSurfaceCaller {
    caller_for_user("user-alpha")
}

pub(crate) fn session_services(
    thread_service: Arc<dyn SessionThreadService>,
    turn_coordinator: Arc<dyn TurnCoordinator>,
) -> RebornServices {
    ironclaw_assistant::RebornServices::new(thread_service, turn_coordinator)
        .with_session_channel_directory(Arc::new(StaticSessionChannelDirectory {
            session_channels: vec!["web-app"],
        }))
}

pub(crate) fn session_submit_request(
    mut value: serde_json::Value,
) -> Result<ProductSubmitTurnRequest, serde_json::Error> {
    if let Some(object) = value.as_object_mut() {
        object
            .entry("extension_id")
            .or_insert_with(|| json!("web-app"));
    }
    serde_json::from_value(value)
}

/// Wait until the wall clock is strictly past `floor`, so the next thread
/// created/used gets a later activity timestamp — deterministic regardless
/// of clock resolution. Uses async sleep to avoid blocking the test runtime
/// (`std::thread::sleep` would block the tokio executor).
pub(crate) async fn wait_until_after(floor: chrono::DateTime<Utc>) {
    while Utc::now() <= floor {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

pub(crate) fn caller_for_user(user_id: &str) -> ProductSurfaceCaller {
    caller_for_user_with_project(user_id, Some("project-alpha"))
}

pub(crate) fn turn_actor_for_user(user_id: &str) -> TurnActor {
    TurnActor::new(UserId::new(user_id).expect("valid user"))
}

pub(crate) fn caller_with_project(project_id: Option<&str>) -> ProductSurfaceCaller {
    caller_for_user_with_project("user-alpha", project_id)
}

pub(crate) fn caller_for_user_with_project(
    user_id: &str,
    project_id: Option<&str>,
) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("valid tenant"),
        UserId::new(user_id).expect("valid user"),
        Some(AgentId::new("agent-alpha").expect("valid agent")),
        project_id.map(|project_id| ProjectId::new(project_id).expect("valid project")),
    )
}

pub(crate) fn run_id_string() -> String {
    "3d54a1f0-0a7f-4b9c-a350-4258f2fa3e18".to_string()
}

pub(crate) fn automation_run_id() -> TurnRunId {
    TurnRunId::parse("11111111-1111-1111-1111-111111111111").expect("valid automation run id")
}

pub(crate) fn fake_thread_history(owner: &ProductSurfaceCaller, thread_id: &str) -> ThreadHistory {
    let thread_id = ThreadId::new(thread_id).expect("valid thread id");
    let scope = ThreadScope {
        tenant_id: owner.tenant_id.clone(),
        agent_id: owner.agent_id.clone().expect("test caller has agent"),
        project_id: owner.project_id.clone(),
        owner_user_id: Some(owner.user_id.clone()),
        mission_id: None,
    };
    ThreadHistory {
        thread: SessionThreadRecord {
            scope: scope.clone(),
            thread_id: thread_id.clone(),
            created_by_actor_id: owner.user_id.as_str().to_string(),
            title: Some("M2 service contract thread".to_string()),
            metadata_json: None,
            goal: None,
            created_at: None,
            updated_at: None,
        },
        messages: vec![ThreadMessageRecord {
            message_id: ThreadMessageId::new(),
            thread_id,
            sequence: 1,
            kind: MessageKind::User,
            status: MessageStatus::Submitted,
            created_at: None,
            updated_at: None,
            actor_id: Some(owner.user_id.as_str().to_string()),
            source_binding_id: Some("webui-src:test".to_string()),
            reply_target_binding_id: Some("webui-reply:test".to_string()),
            turn_id: Some("turn-test".to_string()),
            turn_run_id: Some(run_id_string()),
            tool_result_ref: None,
            tool_result_provider_call: None,
            content: Some("timeline from fake M2 port".to_string()),
            attachments: Vec::new(),
            redaction_ref: None,
        }],
        summary_artifacts: vec![],
    }
}

pub(crate) fn thread_scope_for(caller: &ProductSurfaceCaller) -> ThreadScope {
    ThreadScope {
        tenant_id: caller.tenant_id.clone(),
        agent_id: caller.agent_id.clone().expect("agent id"),
        project_id: caller.project_id.clone(),
        owner_user_id: Some(caller.user_id.clone()),
        mission_id: None,
    }
}

pub(crate) fn segment(name: &str, value: &str) -> String {
    format!("{name}:{}:{value};", value.len())
}

/// Establish thread ownership for `caller` under `thread_id` so subsequent
/// thread-bound service calls pass the ownership check.
pub(crate) async fn setup_owned_thread(
    services: &RebornServices,
    owner: ProductSurfaceCaller,
    thread_id: &str,
) {
    create_thread_for(services, owner, thread_id).await;
}

pub(crate) fn automation_info(
    trigger_id: &str,
    name: impl Into<String>,
    cron: impl Into<String>,
    last_status: Option<RebornAutomationRunStatus>,
) -> RebornAutomationInfo {
    RebornAutomationInfo {
        automation_id: trigger_id.to_string(),
        name: name.into(),
        source: RebornAutomationSource::Schedule {
            cron: cron.into(),
            timezone: "UTC".to_string(),
        },
        state: RebornAutomationState::Active,
        next_run_at: Some("2026-06-03T09:00:00Z".parse().expect("next run")),
        last_run_at: None,
        last_status,
        recent_runs: vec![RebornAutomationRecentRunInfo {
            run_id: Some(automation_run_id()),
            thread_id: Some(ThreadId::new("thread-listed").expect("valid thread id")),
            fire_slot: Some("2026-06-03T09:00:00Z".parse().expect("fire slot")),
            status: RebornAutomationRecentRunStatus::Ok,
            submitted_at: "2026-06-03T09:00:01Z".parse().expect("submitted at"),
            completed_at: Some("2026-06-03T09:00:42Z".parse().expect("completed at")),
        }],
        is_active: true,
        created_at: Some("2026-06-02T18:00:00Z".parse().expect("created at")),
        active_hold: None,
    }
}

pub(crate) fn scripted_stub_unreachable(method: &str) -> ! {
    panic!("ScriptedThreadService::{method} should not be reached")
}

pub(crate) async fn create_thread_for(
    services: &RebornServices,
    caller: ProductSurfaceCaller,
    thread_id: &str,
) {
    services
        .create_thread(
            caller,
            serde_json::from_value::<ProductCreateThreadRequest>(json!({
                "client_action_id": format!("create-{thread_id}"),
                "requested_thread_id": thread_id
            }))
            .expect("create request"),
        )
        .await
        .expect("create thread");
}

pub(crate) fn sample_reborn_project(project_id: &str) -> RebornProjectInfo {
    RebornProjectInfo {
        project_id: project_id.to_string(),
        name: "Sample".to_string(),
        description: String::new(),
        icon: None,
        color: None,
        metadata: json!({}),
        state: RebornProjectState::Active,
        role: RebornProjectRole::Owner,
        created_at: "1970-01-01T00:00:00Z".parse().expect("created at"),
        updated_at: "1970-01-01T00:00:00Z".parse().expect("updated at"),
    }
}

pub(crate) fn sample_reborn_project_member(user_id: &str) -> RebornProjectMemberInfo {
    RebornProjectMemberInfo {
        user_id: user_id.to_string(),
        role: RebornProjectRole::Editor,
        status: RebornProjectMemberStatus::Active,
        granted_by: "user-alpha".to_string(),
        created_at: "1970-01-01T00:00:00Z".parse().expect("created at"),
        updated_at: "1970-01-01T00:00:00Z".parse().expect("updated at"),
    }
}

/// The `ChannelConnectionService` maps are keyed by `ExtensionId`, so these
/// doubles build the same typed key production does.
pub(crate) fn extension_id(value: &str) -> ExtensionId {
    ExtensionId::new(value).expect("fixture extension id is valid extension vocabulary")
}

pub(crate) async fn seed_submitted_message(
    thread_service: &Arc<InMemorySessionThreadService>,
    scope: &ThreadScope,
    thread_id: &ThreadId,
    run_id: &TurnRunId,
    content: &str,
) {
    let accepted = thread_service
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: scope.clone(),
            thread_id: thread_id.clone(),
            actor_id: scope
                .owner_user_id
                .as_ref()
                .expect("test owner")
                .as_str()
                .to_string(),
            source_binding_id: None,
            reply_target_binding_id: None,
            external_event_id: Some(format!("artifact-{run_id}")),
            content: MessageContent::text(content),
        })
        .await
        .expect("accepted");
    thread_service
        .mark_message_submitted(
            scope,
            thread_id,
            accepted.message_id,
            format!("turn-{run_id}"),
            run_id.to_string(),
        )
        .await
        .expect("submitted");
}

/// External creator user id used in trigger-thread scope tests.
///
/// Trigger threads are stored with the `creator_user_id` of the actor that
/// fired the trigger (e.g. a Slack user), which is intentionally different
/// from the WebUI caller (`"user-alpha"`/`"user-alice"`/`"user-bob"`).
/// Using a distinct value here proves the scope reconstruction uses the
/// stored creator — not the caller — to build the `ThreadScope`.
pub(crate) const TRIGGER_CREATOR_USER_ID: &str = "user-trigger-creator";

/// Build a `ThreadScope` matching how `record_trigger_prompt` actually stores
/// trigger-fired threads: same tenant/agent/project as the trigger record, but
/// `owner_user_id` = the **external creator** (not the WebUI caller).
pub(crate) fn trigger_thread_scope_for(caller: &ProductSurfaceCaller) -> ThreadScope {
    ThreadScope {
        tenant_id: caller.tenant_id.clone(),
        agent_id: caller.agent_id.clone().expect("agent id"),
        project_id: caller.project_id.clone(),
        owner_user_id: Some(
            UserId::new(TRIGGER_CREATOR_USER_ID).expect("valid trigger creator user id"),
        ),
        mission_id: None,
    }
}

/// Build the `TriggerRunThreadScope` that `resolve_run_thread_scope` returns
/// for a trigger whose thread was stored via `trigger_thread_scope_for`.
pub(crate) fn trigger_run_thread_scope_for(caller: &ProductSurfaceCaller) -> TriggerRunThreadScope {
    TriggerRunThreadScope {
        agent_id: caller.agent_id.clone(),
        project_id: caller.project_id.clone(),
        creator_user_id: UserId::new(TRIGGER_CREATOR_USER_ID)
            .expect("valid trigger creator user id"),
    }
}

// Regression tests for the automation-trigger gate/approval interaction
// fallback.  Bug: `resolve_gate`, `cancel_run`, `get_run_state`, and
// `stream_events` all called `resolve_webui_thread_metadata` (user-scoped
// probe only) rather than `resolve_thread_access_for_caller` (user-scoped
// probe + automation fallback). Any gate-approval or auth-submit action on a
// trigger-fired thread therefore returned 404, even when the caller owned the
// automation that produced the thread.

pub(crate) fn automation_service_with_trigger_thread(
    trigger_thread_id: ThreadId,
    caller: &ProductSurfaceCaller,
) -> Arc<StaticAutomationService> {
    Arc::new(
        StaticAutomationService::new(vec![RebornAutomationInfo {
            automation_id: "trigger-gate-automation".to_string(),
            name: "Gate test automation".to_string(),
            source: RebornAutomationSource::Schedule {
                cron: "0 9 * * *".to_string(),
                timezone: "UTC".to_string(),
            },
            state: RebornAutomationState::Active,
            next_run_at: None,
            last_run_at: None,
            last_status: Some(RebornAutomationRunStatus::Ok),
            recent_runs: vec![RebornAutomationRecentRunInfo {
                run_id: Some(automation_run_id()),
                thread_id: Some(trigger_thread_id.clone()),
                fire_slot: None,
                status: RebornAutomationRecentRunStatus::Ok,
                submitted_at: "2026-06-10T09:00:01Z".parse().expect("submitted_at"),
                completed_at: None,
            }],
            is_active: true,
            created_at: None,
            active_hold: None,
        }])
        .with_resolve_scope_for_thread(
            trigger_thread_id.clone(),
            trigger_run_thread_scope_for(caller),
        ),
    )
}

/// Set up a trigger thread stored under the external creator's scope and
/// return the thread_id.  Mirrors `record_trigger_prompt` which sets
/// `owner_user_id = Some(creator_user_id)`.
pub(crate) async fn setup_trigger_thread(
    thread_service: &Arc<InMemorySessionThreadService>,
    caller: &ProductSurfaceCaller,
    thread_id: &str,
) -> ThreadId {
    let tid = ThreadId::new(thread_id).expect("valid trigger thread id");
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: trigger_thread_scope_for(caller),
            thread_id: Some(tid.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Gate test trigger thread".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-gate-automation",
            )),
        })
        .await
        .expect("trigger thread stored");
    tid
}

/// One recorded configure-port save: the target extension id plus the
/// submitted `(handle, value)` pairs.
pub(crate) type RecordedChannelConfigSave = (String, Vec<(String, String)>);

pub(crate) fn services_with_setup_llm_config(
    llm_config: Arc<SetupRecordingLlmConfigService>,
) -> RebornServices {
    session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_llm_config_service(llm_config)
}

pub(crate) type OperatorConfigServices = RebornServices<OperatorConfigAutoApproveInvoker>;

pub(crate) fn operator_config_success_resolution(activity_id: ActivityId) -> Resolution {
    Resolution::Done(Outcome {
        refs: OutcomeRefs {
            result: ResultRef::from_uuid(activity_id.as_uuid()),
            byte_len: 0,
            preview: None,
            preview_meta: ResultPreviewMeta::default(),
            origin: None,
            output_digest: None,
        },
        verdict: ToolVerdict::Success,
        summary: SafeSummary::new("operator config updated")
            .expect("static summary is redaction-safe"),
        progress: ResultProgress::MadeProgress,
        terminate_hint: TerminateHint::Continue,
    })
}

pub(crate) fn operator_config_test_tools() -> Vec<RebornOperatorToolInfo> {
    vec![
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("tool.alpha").expect("capability id"),
            provider: ExtensionId::new("extension.alpha").expect("extension id"),
            description: Arc::from("Alpha tool"),
            default_permission: PermissionMode::Ask,
            effects: Arc::from([EffectKind::ExecuteCode]),
        },
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("tool.default_allow").expect("capability id"),
            provider: ExtensionId::new("extension.default_allow").expect("extension id"),
            description: Arc::from("Default-allow tool"),
            default_permission: PermissionMode::Allow,
            effects: Arc::from([EffectKind::DispatchCapability]),
        },
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("tool.locked").expect("capability id"),
            provider: ExtensionId::new("extension.locked").expect("extension id"),
            description: Arc::from("Locked tool"),
            default_permission: PermissionMode::Deny,
            effects: Arc::from([]),
        },
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("tool.financial").expect("capability id"),
            provider: ExtensionId::new("extension.financial").expect("extension id"),
            description: Arc::from("Financial tool"),
            default_permission: PermissionMode::Ask,
            effects: Arc::from([EffectKind::Financial]),
        },
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("nearai.web_search").expect("capability id"),
            provider: ExtensionId::new("nearai").expect("extension id"),
            description: Arc::from("Search through the NEAR AI MCP server."),
            default_permission: PermissionMode::Ask,
            effects: Arc::from([EffectKind::DispatchCapability]),
        },
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("github.get_repo").expect("capability id"),
            provider: ExtensionId::new("github").expect("extension id"),
            description: Arc::from("Read GitHub repository metadata."),
            default_permission: PermissionMode::Ask,
            effects: Arc::from([EffectKind::DispatchCapability]),
        },
        RebornOperatorToolInfo {
            capability_id: CapabilityId::new("google-calendar.list_events").expect("capability id"),
            provider: ExtensionId::new("google-calendar").expect("extension id"),
            description: Arc::from("List Google Calendar events."),
            default_permission: PermissionMode::Ask,
            effects: Arc::from([EffectKind::DispatchCapability]),
        },
    ]
}

pub(crate) async fn query_product_surface_page<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    query: RebornViewQuery,
) -> Result<RebornViewPage, ProductSurfaceError> {
    let page = services
        .query(
            caller,
            ironclaw_product_contracts::surface::ProductSurfaceQueryRequest {
                view_id: query.view_id,
                input: query.params,
                cursor: query.cursor,
                limit: None,
            },
        )
        .await?;
    let payload = page
        .items
        .into_iter()
        .next()
        .ok_or_else(ProductSurfaceError::internal)?;
    Ok(RebornViewPage {
        payload,
        next_cursor: page.next_cursor,
    })
}

pub(crate) fn skill_info(name: &str) -> RebornSkillInfo {
    RebornSkillInfo {
        name: name.to_string(),
        description: format!("{name} skill"),
        version: "1.0.0".to_string(),
        trust: RebornSkillTrustLevel::Installed,
        source: RebornSkillSourceKind::User,
        source_kind: RebornSkillSourceKind::User,
        keywords: Vec::new(),
        usage_hint: None,
        setup_hint: None,
        bundle_path: None,
        install_source_url: None,
        has_requirements: false,
        has_scripts: false,
        can_edit: true,
        can_delete: true,
        auto_activate: true,
    }
}

pub(crate) fn lifecycle_package_ref(package_id: &str) -> LifecyclePackageRef {
    LifecyclePackageRef::new(LifecyclePackageKind::Extension, package_id)
        .expect("valid package ref")
}

pub(crate) fn extension_summary(
    package_id: &str,
    credential_requirements: Vec<LifecycleExtensionCredentialRequirement>,
    onboarding: Option<LifecycleExtensionOnboarding>,
) -> LifecycleExtensionSummary {
    LifecycleExtensionSummary {
        package_ref: lifecycle_package_ref(package_id),
        name: package_id.to_string(),
        version: "1.0.0".to_string(),
        description: "test extension".to_string(),
        source: LifecycleExtensionSource::HostBundled,
        runtime_kind: LifecycleExtensionRuntimeKind::FirstParty,
        surface_kinds: Vec::new(),
        channel_directions: None,
        channel_connection: None,
        channel_presentation: None,
        visible_capability_ids: vec![format!("{package_id}.read"), format!("{package_id}.write")],
        visible_read_only_capability_ids: Vec::new(),
        credential_requirements,
        onboarding,
    }
}

pub(crate) fn assert_setup_validation(
    err: ProductSurfaceError,
    field: &str,
    code: ProductSurfaceValidationCode,
) {
    assert_eq!(err.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(err.status_code, 400);
    assert_eq!(err.field.as_deref(), Some(field));
    assert_eq!(err.validation_code, Some(code));
}

// ---------------------------------------------------------------------------
// Admin user management: service authorization + last-admin protection.
//
// Drives the service methods through a fake `AdminUserService` port so the
// load-bearing NEW logic — role-based authorization (read every request),
// operator bypass, and last-admin protection — is tested through the caller.
// The composition adapter over the real identity store is thin mapping;
// crate-tier is the reachable tier here because the integration harness does
// not wire the admin service (no token minter in-harness).
// ---------------------------------------------------------------------------

pub(crate) fn admin_record(
    user_id: &str,
    role: AdminUserRole,
    status: AdminUserStatus,
) -> AdminUserRecord {
    AdminUserRecord {
        user_id: UserId::new(user_id).expect("user id"),
        email: None,
        display_name: None,
        status,
        role,
        created_at: "2026-07-07T00:00:00Z".to_string(),
        updated_at: "2026-07-07T00:00:00Z".to_string(),
        created_by: None,
        last_login_at: None,
        metadata: std::collections::BTreeMap::new(),
    }
}
