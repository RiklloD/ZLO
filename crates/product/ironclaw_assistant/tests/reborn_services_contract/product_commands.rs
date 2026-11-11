//! Product command palette: listing, executing, and the status/new/stop commands.

use super::*;

#[tokio::test]
async fn status_command_reports_idle_for_a_bound_conversation_without_messages() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let response = ProductSurface::invoke(
        &services,
        caller(),
        ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(PRODUCT_STATUS_COMMAND_OPERATION_ID)
                .expect("status operation"),
            input: serde_json::to_value(ProductStatusCommandInput {
                thread_id: "thread-empty-command-conversation".to_string(),
            })
            .expect("status input"),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect("a newly bound conversation has an idle status");
    let view: CommandResultView =
        serde_json::from_value(response.output).expect("command result view");

    assert_eq!(view.title, "Status");
    assert_eq!(view.fields.len(), 1);
    assert_eq!(view.fields[0].label, "State");
    assert_eq!(view.fields[0].value, "idle");
    assert_eq!(
        view.lines,
        vec!["No assistant activity in this conversation yet."]
    );
}

#[tokio::test]
async fn new_command_preflight_requires_stopping_a_nonterminal_run() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-active-new-preflight").await;
    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-before-new-preflight",
                "thread_id": "thread-active-new-preflight",
                "content": "still working"
            }))
            .expect("submit request"),
        )
        .await
        .expect("submit run");

    let response = ProductSurface::invoke(
        &services,
        caller(),
        ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(PRODUCT_NEW_COMMAND_OPERATION_ID)
                .expect("new operation"),
            input: serde_json::to_value(ProductNewCommandInput {
                thread_id: "thread-active-new-preflight".to_string(),
            })
            .expect("new input"),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect("preflight response");
    let output: ProductNewCommandOutput =
        serde_json::from_value(response.output).expect("new output");

    assert!(!output.can_reset);
    assert_eq!(output.result.title, "Conversation still running");
    assert_eq!(
        output.result.lines,
        vec!["Use /stop first, then try /new again."]
    );
}

// ---------------------------------------------------------------------------
// Command palette: audience-aware `product.commands.list` / `product.commands.execute`.
//
// The WebUI door must enforce the same `required_audience`/`CommandAudience`
// policy the channel command door enforces: a non-admin caller sees and can
// execute only User-audience commands, an env-bearer operator or an active
// admin directory record is treated as admin, and — now that the lifecycle
// execution wiring has landed — an admin caller's Lifecycle command actually
// runs through `services.lifecycle_service.execute(..)` (the same call the
// `product.lifecycle.command` capability op and the channel command door
// use) and its result renders as a `CommandResultView`, never a raw
// rejection. Drives the new facade methods through the generic
// `ProductSurface::invoke` conduit — the same idiom
// `status_command_reports_idle_for_a_bound_conversation_without_messages`
// uses for `PRODUCT_STATUS_COMMAND_OPERATION_ID` — reusing the
// `FakeAdminUsers`/`admin_record` harness from the admin-authorization cluster
// above and the `SetupRecordingLlmConfigService` recording fake from the LLM
// config cluster, rather than standing up new test-only wiring.
// ---------------------------------------------------------------------------

fn command_palette_services(
    admin_users: FakeAdminUsers,
    llm_config: Arc<SetupRecordingLlmConfigService>,
) -> RebornServices {
    session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_admin_user_service(Arc::new(admin_users))
    .with_llm_config_service(llm_config)
}

/// `command_palette_services` plus a wired `LifecycleProductService` fake —
/// for the Lifecycle-family execute/render tests below, which need to prove
/// either that the service was called with the right action (admin) or that
/// it was never called at all (non-admin).
fn command_palette_services_with_lifecycle(
    admin_users: FakeAdminUsers,
    llm_config: Arc<SetupRecordingLlmConfigService>,
    lifecycle_service: Arc<dyn LifecycleProductService>,
) -> RebornServices {
    command_palette_services(admin_users, llm_config)
        .with_lifecycle_product_service(lifecycle_service)
}

async fn list_product_commands_via_invoke(
    services: &RebornServices,
    caller: ProductSurfaceCaller,
) -> Result<RebornProductCommandListResponse, ProductSurfaceError> {
    let response = ProductSurface::invoke(
        services,
        caller,
        ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(PRODUCT_COMMAND_LIST_COMMAND_ID)
                .expect("list operation id"),
            input: serde_json::to_value(EmptyProductCommandInput::default()).expect("empty input"),
            activity_id: ActivityId::new(),
        },
    )
    .await?;
    Ok(serde_json::from_value(response.output).expect("command list response"))
}

async fn execute_product_command_via_invoke(
    services: &RebornServices,
    caller: ProductSurfaceCaller,
    thread_id: &str,
    text: &str,
) -> Result<RebornExecuteProductCommandResponse, ProductSurfaceError> {
    let response = ProductSurface::invoke(
        services,
        caller,
        ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(PRODUCT_COMMAND_EXECUTE_COMMAND_ID)
                .expect("execute operation id"),
            input: serde_json::to_value(RebornExecuteProductCommandRequest {
                thread_id: thread_id.to_string(),
                text: text.to_string(),
            })
            .expect("execute input"),
            activity_id: ActivityId::new(),
        },
    )
    .await?;
    Ok(serde_json::from_value(response.output).expect("execute response"))
}

#[tokio::test]
async fn member_command_list_excludes_admin_audience() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    let response = list_product_commands_via_invoke(&services, caller())
        .await
        .expect("member may list commands");

    assert_eq!(response.commands.len(), 5, "{:?}", response.commands);
    let model = response
        .commands
        .iter()
        .find(|command| command.name == "model")
        .expect("model command listed");
    assert_eq!(model.title, "Model");
    assert_eq!(model.description, "Show or choose your preferred LLM model");
    assert_eq!(model.usage, "/model [use <model> | default]");
    let status = response
        .commands
        .iter()
        .find(|command| command.name == "status")
        .expect("status command listed");
    assert_eq!(status.title, "Status");
    assert_eq!(
        status.description,
        "Show what the assistant is doing in this conversation"
    );
    assert_eq!(status.usage, "/status");
    assert!(
        response.commands.iter().all(|command| matches!(
            command.name.as_str(),
            "model" | "status" | "new" | "stop" | "interrupt"
        )),
        "member list must not include lifecycle commands: {:?}",
        response.commands
    );
}

#[tokio::test]
async fn admin_command_list_includes_lifecycle_family() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Admin,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    let response = list_product_commands_via_invoke(&services, caller())
        .await
        .expect("admin may list commands");

    assert_eq!(response.commands.len(), 15, "{:?}", response.commands);
}

#[tokio::test]
async fn operator_config_caller_lists_admin_commands() {
    let services = command_palette_services(
        FakeAdminUsers::default(),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    let response = list_product_commands_via_invoke(&services, caller().with_operator_config(true))
        .await
        .expect("operator-config caller may list commands");

    assert_eq!(response.commands.len(), 15, "{:?}", response.commands);
}

#[tokio::test]
async fn member_execute_model_set_is_access_denied_without_llm_invoke() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        llm_config.clone(),
    );

    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/model set fake",
    )
    .await
    .expect("access-denied is a rejection, not a transport error");

    assert!(response.result.is_none());
    let rejection = response
        .rejection
        .expect("member model-set must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::AccessDenied);
    assert_eq!(rejection.message, "This command requires an admin account.");
    assert_eq!(
        llm_config.set_active_count(),
        0,
        "the admin gate must reject before the LLM-config seam is ever invoked"
    );
    assert_eq!(
        llm_config.snapshot_count(),
        0,
        "the admin gate must reject before the LLM-config seam is ever invoked"
    );
}

#[tokio::test]
async fn member_execute_model_read_returns_view() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        llm_config,
    );

    let response =
        execute_product_command_via_invoke(&services, caller(), "thread-command-palette", "/model")
            .await
            .expect("member may read model status");

    assert!(response.rejection.is_none());
    let result = response.result.expect("model read must return a view");
    assert_eq!(result.title, "Model");
}

#[tokio::test]
async fn member_model_preference_commands_update_only_the_callers_preference() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = command_palette_services(
        FakeAdminUsers::with([
            admin_record("user-alpha", AdminUserRole::Member, AdminUserStatus::Active),
            admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
        ]),
        llm_config.clone(),
    );
    let alice_tenant_a = caller();
    let bob_tenant_a = caller_for_user("user-beta");
    let alice_tenant_b = ProductSurfaceCaller::new(
        TenantId::new("tenant-beta").expect("valid tenant"),
        UserId::new("user-alpha").expect("valid user"),
        Some(AgentId::new("agent-alpha").expect("valid agent")),
        Some(ProjectId::new("project-alpha").expect("valid project")),
    );

    let selected = execute_product_command_via_invoke(
        &services,
        alice_tenant_a.clone(),
        "thread-command-palette",
        "/model use model-b",
    )
    .await
    .expect("member may set a caller-scoped preference");
    assert!(selected.rejection.is_none(), "{selected:?}");
    assert_eq!(
        selected.result.expect("selection view").title,
        "Model preference updated"
    );

    for isolated_caller in [bob_tenant_a, alice_tenant_b] {
        let isolated = execute_product_command_via_invoke(
            &services,
            isolated_caller,
            "thread-command-palette",
            "/model",
        )
        .await
        .expect("another caller may read its own model status");
        let view = isolated.result.expect("isolated status view");
        assert_eq!(view.fields[0].value, "workspace default");
        assert_eq!(view.fields[1].value, "model-a");
    }

    let selected_status = execute_product_command_via_invoke(
        &services,
        alice_tenant_a.clone(),
        "thread-command-palette",
        "/model",
    )
    .await
    .expect("selecting caller may read its preference")
    .result
    .expect("selected status view");
    assert_eq!(selected_status.fields[0].value, "model-b");
    assert_eq!(selected_status.fields[1].value, "model-b");

    let reset = execute_product_command_via_invoke(
        &services,
        alice_tenant_a,
        "thread-command-palette",
        "/model default",
    )
    .await
    .expect("member may return to the workspace default");
    assert!(reset.rejection.is_none(), "{reset:?}");
    assert_eq!(
        llm_config.user_model_preference_updates(),
        vec![
            (
                "tenant-alpha".to_string(),
                "user-alpha".to_string(),
                Some("model-b".to_string()),
            ),
            ("tenant-alpha".to_string(), "user-alpha".to_string(), None),
        ]
    );
}

#[tokio::test]
async fn member_model_status_marks_a_stale_preference_unavailable() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        llm_config.clone(),
    );

    execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/model use model-b",
    )
    .await
    .expect("member may set an initially allowed preference");
    llm_config.use_user_model_catalog(UserModelCatalog {
        selection_enabled: true,
        workspace_default: Some("model-a".to_string()),
        models: vec!["model-a".to_string()],
        model_entries: Vec::new(),
    });

    let view =
        execute_product_command_via_invoke(&services, caller(), "thread-command-palette", "/model")
            .await
            .expect("member may inspect a stale preference")
            .result
            .expect("model status view");

    assert_eq!(view.fields[0].value, "model-b (unavailable)");
    assert_eq!(view.fields[1].value, "unavailable");
    assert!(
        view.lines
            .iter()
            .any(|line| line
                == "Your saved preference is no longer available. Use `/model default`."),
        "status must explain how to recover: {view:?}"
    );
}

#[tokio::test]
async fn member_execute_unknown_command_help_excludes_admin_names() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    let response =
        execute_product_command_via_invoke(&services, caller(), "thread-command-palette", "/nope")
            .await
            .expect("unknown command is a rejection, not a transport error");

    assert!(response.result.is_none());
    let rejection = response
        .rejection
        .expect("unknown command must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::InvalidRequest);
    // Exact equality (not a substring check): pins that the member help text
    // is EXACTLY the User-audience commands, in registry order — no
    // lifecycle name can sneak in under a future descriptor addition.
    assert_eq!(
        rejection.message,
        "Available commands:\n/interrupt\n/model\n/new\n/status\n/stop"
    );
}

#[tokio::test]
async fn execute_new_creates_a_fresh_thread_and_returns_an_open_thread_effect() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    setup_owned_thread(&services, caller(), "thread-before-new").await;

    let response =
        execute_product_command_via_invoke(&services, caller(), "thread-before-new", "/new")
            .await
            .expect("new command succeeds");

    assert!(response.rejection.is_none());
    assert_eq!(
        response.result.as_ref().map(|result| result.title.as_str()),
        Some("New conversation")
    );
    let effect = response.effect.expect("new returns a navigation effect");
    let ironclaw_assistant::RebornProductCommandEffect::OpenThread {
        thread_id: new_thread_id,
    } = &effect;
    let new_thread_id = new_thread_id.as_str();
    assert_ne!(new_thread_id, "thread-before-new");
    let timeline = services
        .get_timeline(caller(), RebornTimelineRequest::new(new_thread_id))
        .await
        .expect("new thread is durable and caller-owned");
    assert!(timeline.messages.is_empty());
}

#[tokio::test]
async fn execute_interrupt_cancels_the_latest_active_run() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-stop-command").await;
    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-before-stop",
                "thread_id": "thread-stop-command",
                "content": "please keep working"
            }))
            .expect("submit request"),
        )
        .await
        .expect("submit active run");

    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-stop-command",
        "/interrupt",
    )
    .await
    .expect("interrupt command succeeds");

    assert!(response.rejection.is_none());
    assert_eq!(coordinator.cancellation_count(), 1);
    assert_eq!(
        coordinator
            .last_cancellation_scope()
            .expect("cancellation scope")
            .thread_id
            .as_str(),
        "thread-stop-command"
    );
    let result = response.result.expect("interrupt result");
    assert_eq!(result.title, "Interrupt requested");
    assert_eq!(result.fields[0].value, "cancelled");
}

#[tokio::test]
async fn execute_stop_without_an_active_run_is_a_successful_no_op() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        coordinator.clone(),
    );
    setup_owned_thread(&services, caller(), "thread-idle-stop").await;

    let response =
        execute_product_command_via_invoke(&services, caller(), "thread-idle-stop", "/stop")
            .await
            .expect("idle stop is not an error");

    assert!(response.rejection.is_none());
    assert_eq!(coordinator.cancellation_count(), 0);
    let result = response.result.expect("idle stop result");
    assert_eq!(result.title, "Nothing to stop");
    assert_eq!(result.fields[0].value, "idle");
}

#[tokio::test]
async fn member_execute_non_slash_text_is_invalid_request_with_empty_command() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    // "hello" has no leading slash: `parse_product_slash_command` returns
    // `Ok(None)` (ordinary text) — a DIFFERENT parse-stage-failure branch than
    // an unrecognized slash command name (`ProductCommand::Unknown`, covered
    // above). No command token is ever identified, so `command` is empty.
    let response =
        execute_product_command_via_invoke(&services, caller(), "thread-command-palette", "hello")
            .await
            .expect("ordinary text is a rejection, not a transport error");

    assert_eq!(response.command, "");
    assert!(response.result.is_none());
    let rejection = response.rejection.expect("non-slash text must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::InvalidRequest);
}

// PR-2 review fix wave, item 5: the parse-stage `Err(_)` and
// `ProductCommand::from_payload` `Err(_rejection)` arms correctly keep the
// underlying cause off the wire (the leak rule above), but previously logged
// nothing at all — a caller's own malformed command left no server-side
// trail to debug. Both arms must now bind the cause and log it at `debug!`
// (never `info!`/`warn!`, matching the repo's REPL-safe logging-level rule).
#[tokio::test]
#[traced_test]
async fn execute_malformed_slash_text_logs_the_parse_cause_at_debug_level() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    // A bare "/" is empty after the slash — fails at
    // `parse_product_slash_command` itself (`ProductSlashCommandParseError`),
    // a different branch than the non-slash-text case above.
    let response =
        execute_product_command_via_invoke(&services, caller(), "thread-command-palette", "/")
            .await
            .expect("a malformed slash line is a rejection, not a transport error");

    assert_eq!(response.command, "");
    let rejection = response
        .rejection
        .expect("an empty slash command must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::InvalidRequest);
    // The wire response never carries the raw parser error (leak rule
    // above) — but the server-side cause must still be logged, not
    // silently dropped.
    assert!(
        logs_contain("product command parse rejected"),
        "the parse-stage rejection cause must be logged at debug level before mapping to help text"
    );
    assert!(
        logs_contain("slash command is empty"),
        "the bound parser error's own Display message must appear in the log line"
    );
}

#[tokio::test]
#[traced_test]
async fn execute_command_argument_rejection_logs_the_cause_at_debug_level() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );

    // Parses fine (a recognized command word), but `from_payload` rejects it
    // for a missing argument — a different branch than the parse-stage
    // failure above.
    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/model set-provider",
    )
    .await
    .expect("a missing-argument command is a rejection, not a transport error");

    let rejection = response
        .rejection
        .expect("model set-provider with no provider id must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::InvalidRequest);
    assert!(
        logs_contain("product command argument rejection"),
        "the argument-rejection cause must be logged at debug level before mapping to help text"
    );
    // The rejection's `reason` is a `RedactedString` (never logged even in
    // Debug form, by design) — `kind` is the safe categorical signal that
    // must appear instead.
    assert!(
        logs_contain("InvalidRequest"),
        "the rejection kind must appear in the log line"
    );
}

// Mirrors the cross-user-access idiom used throughout this file (e.g.
// `get_run_state_rejects_cross_user_access`: "404 rather than 403 so the
// existence of Alice's thread is not leaked"). `execute_product_status_command`
// resolves thread history through `resolve_thread_history_for_caller`, which
// maps BOTH an unbound thread_id and a thread bound to a different owner to
// the identical `ProductSurfaceErrorCode::NotFound`, coalesced into the idle
// view — so a caller probing another user's thread_id via `/status` gets
// exactly the same answer as probing a thread_id that was never created:
// Alice's thread is never distinguishable from "nothing here" through this
// entry point. Proven here by asserting full response equality against a
// thread_id nobody ever created, not just spot-checking the idle shape.
#[tokio::test]
async fn execute_status_on_foreign_thread_is_indistinguishable_from_unknown() {
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
    );
    create_thread_for(&services, caller(), "thread-alice").await;
    let bob = caller_for_user("user-beta");

    let foreign_thread_response =
        execute_product_command_via_invoke(&services, bob.clone(), "thread-alice", "/status")
            .await
            .expect("cross-user status read settles rather than transport-erroring");
    let never_created_response = execute_product_command_via_invoke(
        &services,
        bob,
        "thread-never-created-by-anyone",
        "/status",
    )
    .await
    .expect("status on an unknown thread settles rather than transport-erroring");

    assert_eq!(
        foreign_thread_response, never_created_response,
        "a foreign thread must be indistinguishable from a nonexistent one"
    );
    assert!(foreign_thread_response.rejection.is_none());
    let result = foreign_thread_response
        .result
        .expect("status must return the idle view");
    assert_eq!(result.title, "Status");
    assert_eq!(
        result
            .fields
            .iter()
            .find(|field| field.label == "State")
            .map(|field| field.value.as_str()),
        Some("idle"),
        "{result:?}"
    );
}

// Companion to the retired "stays listing-only" pin (PR-2 scope): an admin
// caller's Lifecycle command now actually executes through
// `services.lifecycle_service.execute(..)` — the same call the
// `product.lifecycle.command` capability op and the channel command door
// use — and its `LifecycleProductResponse` renders as a `CommandResultView`
// (title + fields + lines) rather than a rejection or a raw JSON blob.
// `extension_list` exercises the list-family shaping: a `Count` field plus
// one readable row per installed extension (id, name, version, and its
// public `LifecyclePublicState`, never the raw internal `InstallationState`
// checkpoint — see `ironclaw_extension_contracts::state`'s "must never expose those
// checkpoints" contract).
#[tokio::test]
async fn admin_execute_lifecycle_command_executes_and_renders_installed_extensions() {
    let lifecycle_service = Arc::new(ListingLifecycleService {
        extension: LifecycleInstalledExtensionSummary {
            summary: extension_summary("github", Vec::new(), None),
            phase: InstallationState::Active,
            install_scope: None,
        },
    });
    let services = command_palette_services_with_lifecycle(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Admin,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
        lifecycle_service,
    );

    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/extension_list",
    )
    .await
    .expect("an executed lifecycle command settles rather than transport-erroring");

    assert!(
        response.rejection.is_none(),
        "admin must not be rejected: {response:?}"
    );
    let view = response
        .result
        .expect("admin must receive an executed lifecycle result view");
    assert_eq!(view.title, "List extensions");
    assert!(
        view.fields
            .iter()
            .any(|field| field.label == "Count" && field.value == "1"),
        "{view:?}"
    );
    assert!(
        view.lines
            .iter()
            .any(|line| line.contains("github") && line.contains("[active]")),
        "list rows must be readable text, not a JSON blob: {view:?}"
    );
}

// A member caller must never reach the lifecycle service at all: the
// audience gate (Lifecycle is `CommandAudience::Admin`) has to reject before
// `execute_product_lifecycle_command` is ever called. `RecordingLifecycleService`
// panics on any action other than `ExtensionActivate`, so a routing bug that
// let a member's `/extension_list` through would fail loudly here even
// before the explicit `actions()` assertion runs.
#[tokio::test]
async fn member_execute_lifecycle_command_is_denied_before_any_lifecycle_service_call() {
    let lifecycle_service = Arc::new(RecordingLifecycleService::new());
    let services = command_palette_services_with_lifecycle(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Member,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
        lifecycle_service.clone(),
    );

    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/extension_list",
    )
    .await
    .expect("access-denied is a rejection, not a transport error");

    assert!(response.result.is_none());
    let rejection = response
        .rejection
        .expect("member lifecycle execution must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::AccessDenied);
    assert_eq!(rejection.message, "This command requires an admin account.");
    assert!(
        lifecycle_service.actions().is_empty(),
        "the admin gate must reject before the lifecycle service is ever invoked"
    );
}

// Mutation family: `/extension_activate` must confirm what happened (which
// extension, whether the activate call itself succeeded, and the resulting
// public lifecycle state), not just echo a bare rejection or the raw
// response struct.
#[tokio::test]
async fn admin_execute_extension_activate_confirms_the_mutation() {
    let lifecycle_service = Arc::new(RecordingLifecycleService::new());
    let services = command_palette_services_with_lifecycle(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Admin,
            AdminUserStatus::Active,
        )]),
        Arc::new(SetupRecordingLlmConfigService::default()),
        lifecycle_service.clone(),
    );

    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/extension_activate github",
    )
    .await
    .expect("an executed lifecycle mutation settles rather than transport-erroring");

    assert!(response.rejection.is_none(), "{response:?}");
    let view = response
        .result
        .expect("admin must receive a mutation confirmation view");
    assert_eq!(view.title, "Activate extension");
    assert!(
        view.fields
            .iter()
            .any(|field| field.label == "Extension" && field.value == "github"),
        "{view:?}"
    );
    assert!(
        view.fields
            .iter()
            .any(|field| field.label == "Activated" && field.value == "yes"),
        "{view:?}"
    );
    assert!(
        view.fields
            .iter()
            .any(|field| field.label == "State" && field.value == "active"),
        "{view:?}"
    );
    assert!(
        view.lines.iter().any(|line| line == "activated"),
        "the service-authored confirmation message must reach the view: {view:?}"
    );
    assert_eq!(
        lifecycle_service.actions(),
        vec![LifecycleProductAction::ExtensionActivate {
            package_ref: lifecycle_package_ref("github")
        }]
    );
}

// PR-2 review fix wave, item 6: an admin caller executing a Lifecycle
// command is exactly the path that used to read the admin directory twice —
// once at the audience gate (Lifecycle requires Admin audience, so the
// `required_audience(&command) == Admin && !is_admin` short-circuit actually
// evaluates `caller_is_command_admin`) and again while building the
// (then listing-only) response in the Lifecycle/Unknown arm. `is_admin` must
// be resolved once at the top of `execute_product_command` and reused, so the
// SAME request reads the directory exactly once — not zero (it must still
// run at least once; this is not a cache), not two. This still holds now
// that the Lifecycle arm actually executes: no lifecycle service is wired
// here (the default `UnsupportedLifecycleProductService` answers), so the
// command settles as an executed-but-unsupported result rather than a
// rejection — the directory-read count is the load-bearing assertion.
#[tokio::test]
async fn admin_execute_lifecycle_command_reads_admin_directory_exactly_once() {
    let admin_users = Arc::new(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Admin,
        AdminUserStatus::Active,
    )]));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_admin_user_service(admin_users.clone())
    .with_llm_config_service(Arc::new(SetupRecordingLlmConfigService::default()));

    let response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/extension_list",
    )
    .await
    .expect("an executed lifecycle command settles rather than transport-erroring");

    assert!(
        response.rejection.is_none(),
        "the unwired default lifecycle service still executes (as unsupported), not a rejection: {response:?}"
    );
    assert!(response.result.is_some());
    assert_eq!(
        admin_users.get_user_calls(),
        1,
        "the admin directory must be read exactly once per execute_product_command call"
    );
}

// FINDING 2 (Task 3 re-review): only an ACTIVE admin record counts on both
// doors — a suspended admin must be treated as a plain member, same contract
// `authorize_admin` enforces for the rest of the admin surface.
#[tokio::test]
async fn suspended_admin_is_treated_as_member_on_both_doors() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = command_palette_services(
        FakeAdminUsers::with([admin_record(
            "user-alpha",
            AdminUserRole::Admin,
            AdminUserStatus::Suspended,
        )]),
        llm_config.clone(),
    );

    let list_response = list_product_commands_via_invoke(&services, caller())
        .await
        .expect("suspended admin may still list (as a member)");
    assert_eq!(
        list_response.commands.len(),
        5,
        "{:?}",
        list_response.commands
    );

    let execute_response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/model set fake",
    )
    .await
    .expect("access-denied is a rejection, not a transport error");

    assert!(execute_response.result.is_none());
    let rejection = execute_response
        .rejection
        .expect("suspended admin model-set must be rejected");
    assert_eq!(rejection.kind, ProductRejectionKind::AccessDenied);
    assert_eq!(
        llm_config.set_active_count(),
        0,
        "the admin gate must reject before the LLM-config seam is ever invoked"
    );
}

// FINDING 3 (Task 3 re-review): the directory-error taxonomy is also the
// DEFAULT composition shape — a `RebornServices` built without
// `with_admin_user_service` (composition simply never wiring the admin
// surface) falls back to `RejectingAdminUserService`, whose `get_user` always
// returns `Err(AdminUserError::Unavailable)`. That must surface as a
// retryable 503, never a silent "not admin" or an opaque 500.
#[tokio::test]
async fn list_commands_surfaces_directory_unavailable_as_retryable_503() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let err = list_product_commands_via_invoke(&services, caller())
        .await
        .expect_err("an unwired admin directory must not silently resolve a role");

    assert_eq!(err.status_code, 503);
    assert!(err.retryable);
}

// CI regression (PR-2 webui command palette): a review-driven change hoisted
// `caller_is_command_admin` to run UNCONDITIONALLY at the top of
// `execute_product_command`, so it read the admin directory even for a
// User-audience command. With no admin directory wired — the same default
// composition/test shape `list_commands_surfaces_directory_unavailable_as_retryable_503`
// above pins for listing — that turned a plain `/status` or bare `/model`
// into a retryable 503, even though neither command's execution path ever
// needs the caller's admin standing. `execute_product_command` must resolve
// `is_admin` LAZILY (`resolve_admin_standing`): a User-audience command that never
// hits the Admin-audience gate or a role-filtered help-text render must never
// query the admin directory at all, so a degraded/unwired directory cannot
// break it — while an Admin-audience command (`/model set ...`) still needs
// the directory and must keep failing closed with the identical retryable
// 503, never a silent "not admin" `AccessDenied` and never a silent success.
//
// This test fails before the fix (both `/status` and bare `/model` 503) and
// passes after it.
#[tokio::test]
async fn execute_user_audience_commands_succeed_without_admin_directory_but_admin_audience_still_fails_closed()
 {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_llm_config_service(Arc::new(SetupRecordingLlmConfigService::default()));

    let status_response = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/status",
    )
    .await
    .expect("a User-audience /status must succeed with no admin directory wired");
    assert!(status_response.rejection.is_none(), "{status_response:?}");
    assert_eq!(
        status_response
            .result
            .expect("status must return a view")
            .title,
        "Status"
    );

    let model_response =
        execute_product_command_via_invoke(&services, caller(), "thread-command-palette", "/model")
            .await
            .expect("a User-audience bare /model read must succeed with no admin directory wired");
    assert!(model_response.rejection.is_none(), "{model_response:?}");
    assert_eq!(
        model_response
            .result
            .expect("model read must return a view")
            .title,
        "Model"
    );

    let admin_audience_error = execute_product_command_via_invoke(
        &services,
        caller(),
        "thread-command-palette",
        "/model set fake",
    )
    .await
    .expect_err(
        "an Admin-audience command must still fail closed on an unavailable admin \
         directory (retryable 503) — never a silent AccessDenied and never a silent success",
    );
    assert_eq!(admin_audience_error.status_code, 503);
    assert!(admin_audience_error.retryable);
}
