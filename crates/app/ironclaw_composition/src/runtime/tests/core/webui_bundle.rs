//! WebUI bundle and product-surface wiring: workspace mounts, attachments,
//! notification inbox, lifecycle setup, outbound targets, automations,
//! diagnostics, and approval/auth gate routing.

// arch-exempt: large_file, move-only split of runtime/tests/core.rs keeps the WebUI bundle scenarios together; a finer grouping is a follow-up, plan #4471
use super::*;

#[tokio::test]
async fn standalone_runtime_maps_workspace_to_configured_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    std::fs::write(
        workspace_root.path().join("workspace-sentinel.txt"),
        "visible through /workspace",
    )
    .expect("write sentinel");
    let gateway = Arc::new(WorkspaceListingGateway::default());
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway.clone();
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-workspace-owner",
            root.path().join("standalone"),
        )
        .with_local_runtime_workspace_root(workspace_root.path().to_path_buf())
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-workspace-tenant".to_string(),
        agent_id: "runtime-workspace-agent".to_string(),
        source_binding_id: "runtime-workspace-source".to_string(),
        reply_target_binding_id: "runtime-workspace-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "list workspace"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    assert_eq!(reply.text.as_deref(), Some("workspace ok"));
    let request_count = {
        let requests = gateway
            .requests
            .lock()
            .expect("workspace gateway requests lock poisoned");
        requests.len()
    };
    assert_eq!(
        request_count, 2,
        "workspace listing should require initial request plus tool-result follow-up"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_webui_bundle_reuses_thread_and_turn_services() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "webui projection ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(with_test_authenticated_session_channel(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    ))
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-tenant".to_string(),
        agent_id: "runtime-webui-agent".to_string(),
        source_binding_id: "runtime-webui-source".to_string(),
        reply_target_binding_id: "runtime-webui-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let runtime_turn_coordinator = runtime.product_turn_coordinator();
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-tenant").unwrap(),
        UserId::new("runtime-webui-owner").unwrap(),
        Some(AgentId::new("runtime-webui-agent").unwrap()),
        None,
    );
    let created = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        CREATE_THREAD_COMMAND,
        ProductCreateThreadRequest {
            client_action_id: Some("create-webui-stream-thread".to_string()),
            requested_thread_id: None,
            project_id: None,
        },
    )
    .await
    .expect("create webui thread");
    let submitted = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        SUBMIT_TURN_COMMAND,
        ProductSubmitTurnRequest {
            extension_id: Some(TEST_SESSION_EXTENSION_ID.to_string()),
            client_action_id: Some("send-webui-stream-message".to_string()),
            thread_id: Some(created.thread.thread_id.to_string()),
            content: Some("hello webui stream".to_string()),
            attachments: Vec::new(),
            model: None,
            mention_roots: Vec::new(),
        },
    )
    .await
    .expect("submit webui turn");
    let RebornSubmitTurnResponse::Submitted { run_id, .. } = submitted else {
        panic!("webui submit should start a run");
    };
    let stream = tokio::time::timeout(Duration::from_secs(3), async {
        let mut after_cursor = None;
        loop {
            let stream = stream_product_events(
                bundle.as_ref(),
                caller.clone(),
                RebornStreamEventsRequest {
                    thread_id: created.thread.thread_id.to_string(),
                    after_cursor: after_cursor.clone(),
                },
            )
            .await
            .expect("webui event stream");
            if stream.events.iter().any(|event| {
                matches!(
                    event.payload(),
                    ProductOutboundPayload::ProjectionSnapshot { state }
                        | ProductOutboundPayload::ProjectionUpdate { state }
                        if state.items.iter().any(|item| matches!(
                            item,
                            ProductProjectionItem::RunStatus {
                                run_id: seen,
                                status,
                                ..
                            }
                                if *seen == run_id && status == "completed"
                        ))
                )
            }) {
                break stream;
            }
            after_cursor = stream
                .events
                .last()
                .map(|event| event.projection_cursor().clone());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("completed webui projection should appear");

    let _api = bundle.clone();
    assert!(Arc::ptr_eq(
        &runtime_turn_coordinator,
        &runtime.product_turn_coordinator()
    ));
    assert!(
        stream.events.iter().all(|event| matches!(
            event.payload(),
            ProductOutboundPayload::CapabilityActivity(_)
                | ProductOutboundPayload::CapabilityDisplayPreview(_)
                | ProductOutboundPayload::ProjectionSnapshot { .. }
                | ProductOutboundPayload::ProjectionUpdate { .. }
        )),
        "product surface should expose only projection stream events"
    );
    assert_eq!(runtime.readiness().state, RebornReadinessState::DevOnly);

    runtime.shutdown().await.expect("runtime shutdown");
}

/// Caller-level regression for the production attachment-landing path:
/// drives `RebornRuntime::webui_workspace_filesystem()` — the exact method
/// `runtime.product_surface`/`build_openai_compat_route_mount` call — through
/// a real `ProjectScopedAttachmentLander`, then reads the landed bytes back
/// through the same `ProjectScopedAttachmentReader` production wires
/// `attachment_read_port` with. The C-ATTACH integration tests exercise the
/// shared `RebornRuntimeStores::read_write_workspace_filesystem` recipe via the
/// `standalone_attachment_test_support_for_test` seam, but never call through
/// this `RebornRuntime` wrapper itself; this closes that gap so a future
/// regression in the wrapper (not just the shared recipe) fails a test
/// instead of only breaking WebUI/OpenAI-compatible attachment landing in
/// production.
#[tokio::test]
async fn webui_workspace_filesystem_lands_attachment_with_read_write_mount() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "attachment mount ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-attachment-mount-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-attachment-mount-tenant".to_string(),
        agent_id: "runtime-attachment-mount-agent".to_string(),
        source_binding_id: "runtime-attachment-mount-source".to_string(),
        reply_target_binding_id: "runtime-attachment-mount-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let read_write_filesystem = runtime
        .webui_workspace_filesystem()
        .expect("standalone runtime composes a read-write webui workspace filesystem");
    // The read port reads the same durable bytes the lander writes; production's
    // `attachment_read_port` uses the read-only workspace view, but the read side
    // is byte-identical over the read-write view (the reader never writes), so
    // this test resolves the same authority a vision-capable model would.
    let read_port =
        ironclaw_assistant::ProjectScopedAttachmentReader::new(Arc::clone(&read_write_filesystem));
    let lander = ironclaw_attachments::ProjectScopedAttachmentLander::new(read_write_filesystem);

    let thread_scope = ThreadScope {
        tenant_id: TenantId::new("runtime-attachment-mount-tenant").unwrap(),
        agent_id: AgentId::new("runtime-attachment-mount-agent").unwrap(),
        project_id: None,
        owner_user_id: Some(UserId::new("runtime-attachment-mount-owner").unwrap()),
        mission_id: None,
    };
    let refs = ironclaw_attachments::InboundAttachmentLander::land(
        &lander,
        &thread_scope,
        "msg-attachment-mount",
        vec![ironclaw_host_api::attachment::InboundAttachment {
            id: "att-0".to_string(),
            mime_type: "image/png".to_string(),
            filename: Some("mount-check.png".to_string()),
            bytes: b"attachment-mount-bytes".to_vec(),
        }],
    )
    .await
    .expect("landing through the production webui workspace filesystem succeeds");
    let storage_key = refs[0]
        .storage_key
        .as_deref()
        .expect("landed attachment carries a storage_key");

    let read_back = ironclaw_loop_host::LoopAttachmentReadPort::read_attachment_bytes(
        &read_port,
        &thread_scope.to_resource_scope(),
        storage_key,
    )
    .await
    .expect("reading the landed attachment back through the read port succeeds");

    assert_eq!(read_back, b"attachment-mount-bytes".to_vec());

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn production_channel_host_lands_attachment_with_read_write_mount() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "channel attachment mount ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-channel-attachment-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-channel-attachment-tenant".to_string(),
        agent_id: "runtime-channel-attachment-agent".to_string(),
        source_binding_id: "runtime-channel-attachment-source".to_string(),
        reply_target_binding_id: "runtime-channel-attachment-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    assert!(
        runtime._channel_host_assembly.is_some(),
        "local-dev runtime composes the production channel host"
    );
    let lander = runtime
        .channel_workflow_factory()
        .as_ref()
        .expect("the production channel host is built over a workflow factory")
        .inbound_attachment_lander();
    let thread_scope = ThreadScope {
        tenant_id: TenantId::new("runtime-channel-attachment-tenant").unwrap(),
        agent_id: AgentId::new("runtime-channel-attachment-agent").unwrap(),
        project_id: None,
        owner_user_id: Some(UserId::new("runtime-channel-attachment-owner").unwrap()),
        mission_id: None,
    };

    let refs = lander
        .land(
            &thread_scope,
            "msg-channel-attachment-mount",
            vec![ironclaw_host_api::attachment::InboundAttachment {
                id: "att-0".to_string(),
                mime_type: "image/png".to_string(),
                filename: Some("channel-mount-check.png".to_string()),
                bytes: b"channel-attachment-mount-bytes".to_vec(),
            }],
        )
        .await
        .expect("production channel-host attachment lander has write authority");

    assert_eq!(refs.len(), 1);
    // A single-user deployment keeps the shared workspace root, so an inbound
    // attachment stays where its host aliases and browser look for it.
    assert_landed_under(
        &runtime,
        &refs[0],
        "/projects/workspace",
        "standalone channel attachment",
    )
    .await;
    lander
        .rollback(&thread_scope, &refs)
        .await
        .expect("production channel-host attachment lander has batch rollback authority");
    runtime.shutdown().await.expect("runtime shutdown");
}

/// Assert the landed attachment's bytes are physically readable under
/// `expected_root` on the composed filesystem — the placement claim the
/// scoped-view read ports cannot make, because they resolve through the very
/// mount view under test.
async fn assert_landed_under(
    runtime: &crate::RebornRuntime,
    landed: &ironclaw_common::AttachmentRef,
    expected_root: &str,
    label: &str,
) {
    use ironclaw_filesystem::RootFilesystem;

    let storage_key = landed
        .storage_key
        .as_deref()
        .expect("landed attachment carries a storage_key");
    let relative = storage_key
        .strip_prefix("/workspace/")
        .expect("attachment storage keys are workspace-scoped");
    let path = ironclaw_host_api::path::VirtualPath::new(format!("{expected_root}/{relative}"))
        .expect("composed attachment path");
    assert!(
        runtime.extension_filesystem.read_file(&path).await.is_ok(),
        "{label} should be readable at {path:?}"
    );
}

async fn query_webui_extension_setup(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    package_id: &str,
) -> RebornSetupExtensionResponse {
    let page = query_product_surface_page(
        api,
        caller,
        ironclaw_product_contracts::views::RebornViewQuery {
            view_id: ironclaw_assistant::EXTENSION_SETUP_VIEW.id.to_string(),
            params: serde_json::json!({ "package_id": package_id }),
            cursor: None,
        },
    )
    .await
    .expect("setup extension lifecycle projection");
    serde_json::from_value(page.payload).expect("setup extension payload")
}

async fn query_product_surface_page(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    query: RebornViewQuery,
) -> Result<RebornViewPage, ProductSurfaceError> {
    let page = ironclaw_product_contracts::surface::ProductSurface::query(
        api,
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

#[tokio::test]
async fn production_product_surface_uses_the_durable_notification_inbox() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    let gateway: Arc<dyn HostManagedModelGateway> = Arc::new(RecordingGateway {
        reply: "notification composition".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let runtime_input = || {
        RebornRuntimeInput::from_build_input(
            crate::deployment::local_filesystem_build_input(
                "runtime-notification-owner",
                storage_root.clone(),
            )
            .with_runtime_policy(standalone_runtime_policy()),
        )
        .with_identity(RebornRuntimeIdentity {
            tenant_id: "runtime-notification-tenant".to_string(),
            agent_id: "runtime-notification-agent".to_string(),
            source_binding_id: "runtime-notification-source".to_string(),
            reply_target_binding_id: "runtime-notification-reply".to_string(),
        })
        .with_model_gateway_override(Arc::clone(&gateway))
    };
    let runtime = tokio::time::timeout(
        PRODUCTION_SHAPED_BUILD_TIMEOUT,
        build_reborn_runtime(runtime_input()),
    )
    .await
    .expect("runtime build does not time out")
    .expect("runtime builds");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-notification-tenant").expect("tenant"),
        UserId::new("runtime-notification-owner").expect("user"),
        Some(AgentId::new("runtime-notification-agent").expect("agent")),
        None,
    );
    let thread_id = ThreadId::new("runtime-notification-thread").expect("thread");
    let turn_run_id = TurnRunId::new();
    tokio::time::timeout(
        RUNTIME_POLL_TIMEOUT,
        runtime
            .notification_inbox
            .publish(ironclaw_notifications::PublishNotificationRequest {
                id: ironclaw_notifications::NotificationId::new("runtime-notification-1")
                    .expect("notification id"),
                recipient: ironclaw_notifications::NotificationRecipient {
                    tenant_id: caller.tenant_id.clone(),
                    user_id: caller.user_id.clone(),
                },
                kind: ironclaw_notifications::NotificationKind::ApprovalRequired,
                severity: ironclaw_notifications::NotificationSeverity::Warning,
                source: ironclaw_notifications::NotificationSource {
                    thread_id: Some(thread_id.clone()),
                    turn_run_id: Some(turn_run_id),
                    lifecycle_ref: Some(
                        ironclaw_notifications::LifecycleRef::new("runtime-notification-gate")
                            .expect("lifecycle ref"),
                    ),
                    credential_providers: Vec::new(),
                },
                action: ironclaw_notifications::NotificationAction::OpenThread { thread_id },
                initial_state: ironclaw_notifications::NotificationInitialState::Open,
                occurred_at: Utc::now(),
            }),
    )
    .await
    .expect("notification publish does not time out")
    .expect("persist notification through production store");

    let bundle = runtime.product_surface(None).expect("product surface");
    let initial_page = tokio::time::timeout(
        RUNTIME_POLL_TIMEOUT,
        query_product_surface_page(
            bundle.as_ref(),
            caller.clone(),
            RebornViewQuery {
                view_id: NOTIFICATIONS_VIEW.id.to_string(),
                params: serde_json::json!({ "limit": 10 }),
                cursor: None,
            },
        ),
    )
    .await
    .expect("notification list does not time out")
    .expect("list notification through production product surface");
    let initial: ProductListNotificationsResponse =
        serde_json::from_value(initial_page.payload).expect("notification response");
    assert_eq!(initial.notifications.len(), 1);
    assert_eq!(initial.notifications[0].id, "runtime-notification-1");
    assert_eq!(
        initial.notifications[0].turn_run_id,
        Some(turn_run_id.to_string())
    );
    assert_eq!(initial.unread_count, 1);

    let malformed_limit = query_product_surface_page(
        bundle.as_ref(),
        caller.clone(),
        RebornViewQuery {
            view_id: NOTIFICATIONS_VIEW.id.to_string(),
            params: serde_json::json!({ "limit": "ten" }),
            cursor: None,
        },
    )
    .await
    .expect_err("a malformed notification limit is rejected at the product boundary");
    assert_eq!(
        malformed_limit.code,
        ProductSurfaceErrorCode::InvalidRequest
    );
    assert_eq!(malformed_limit.kind, ProductSurfaceErrorKind::Validation);

    let invalid_limit = query_product_surface_page(
        bundle.as_ref(),
        caller.clone(),
        RebornViewQuery {
            view_id: NOTIFICATIONS_VIEW.id.to_string(),
            params: serde_json::json!({ "limit": 0 }),
            cursor: None,
        },
    )
    .await
    .expect_err("an invalid notification limit is rejected at the product boundary");
    assert_eq!(invalid_limit.code, ProductSurfaceErrorCode::InvalidRequest);

    let missing = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        NOTIFICATIONS_MARK_READ_COMMAND,
        ProductNotificationMutationRequest {
            notification_id: "runtime-notification-missing".to_string(),
        },
    )
    .await
    .expect_err("a missing notification is a typed product miss");
    assert_eq!(missing.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(missing.kind, ProductSurfaceErrorKind::NotFound);

    let foreign_caller = ProductSurfaceCaller::new(
        caller.tenant_id.clone(),
        UserId::new("runtime-notification-foreign").expect("foreign user"),
        caller.agent_id.clone(),
        caller.project_id.clone(),
    );
    let foreign_mutation = invoke_product_command(
        bundle.as_ref(),
        foreign_caller,
        NOTIFICATIONS_MARK_READ_COMMAND,
        ProductNotificationMutationRequest {
            notification_id: "runtime-notification-1".to_string(),
        },
    )
    .await
    .expect_err("a foreign caller cannot mutate the owner's notification");
    assert_eq!(foreign_mutation.code, ProductSurfaceErrorCode::NotFound);

    let owner_after_foreign_attempt = runtime
        .notification_inbox
        .list(ironclaw_notifications::ListNotificationsRequest {
            recipient: ironclaw_notifications::NotificationRecipient {
                tenant_id: caller.tenant_id.clone(),
                user_id: caller.user_id.clone(),
            },
            limit: 10,
            cursor: None,
            include_archived: true,
        })
        .await
        .expect("owner notification survives foreign mutation attempt");
    assert_eq!(owner_after_foreign_attempt.notifications.len(), 1);
    assert!(
        owner_after_foreign_attempt.notifications[0]
            .read_at
            .is_none()
    );

    tokio::time::timeout(
        RUNTIME_POLL_TIMEOUT,
        invoke_product_command(
            bundle.as_ref(),
            caller.clone(),
            NOTIFICATIONS_MARK_READ_COMMAND,
            ProductNotificationMutationRequest {
                notification_id: "runtime-notification-1".to_string(),
            },
        ),
    )
    .await
    .expect("mark-read command does not time out")
    .expect("mark durable notification read");
    tokio::time::timeout(
        RUNTIME_POLL_TIMEOUT,
        invoke_product_command(
            bundle.as_ref(),
            caller.clone(),
            NOTIFICATIONS_ARCHIVE_COMMAND,
            ProductNotificationMutationRequest {
                notification_id: "runtime-notification-1".to_string(),
            },
        ),
    )
    .await
    .expect("archive command does not time out")
    .expect("archive durable notification");

    let visible_page = tokio::time::timeout(
        RUNTIME_POLL_TIMEOUT,
        query_product_surface_page(
            bundle.as_ref(),
            caller.clone(),
            RebornViewQuery {
                view_id: NOTIFICATIONS_VIEW.id.to_string(),
                params: serde_json::json!({ "limit": 10 }),
                cursor: None,
            },
        ),
    )
    .await
    .expect("post-archive notification list does not time out")
    .expect("list notifications after archive");
    let visible: ProductListNotificationsResponse =
        serde_json::from_value(visible_page.payload).expect("notification response");
    assert!(visible.notifications.is_empty());
    assert_eq!(visible.unread_count, 0);

    drop(bundle);
    tokio::time::timeout(RUNTIME_SEND_TIMEOUT, runtime.shutdown())
        .await
        .expect("runtime shutdown does not time out")
        .expect("runtime shuts down");

    let reopened = tokio::time::timeout(
        PRODUCTION_SHAPED_BUILD_TIMEOUT,
        build_reborn_runtime(runtime_input()),
    )
    .await
    .expect("reopened runtime build does not time out")
    .expect("reopened runtime builds");
    let persisted = tokio::time::timeout(
        RUNTIME_POLL_TIMEOUT,
        reopened
            .notification_inbox
            .list(ironclaw_notifications::ListNotificationsRequest {
                recipient: ironclaw_notifications::NotificationRecipient {
                    tenant_id: caller.tenant_id.clone(),
                    user_id: caller.user_id.clone(),
                },
                limit: 10,
                cursor: None,
                include_archived: true,
            }),
    )
    .await
    .expect("reopened notification list does not time out")
    .expect("read persisted archive state after restart");
    assert_eq!(persisted.notifications.len(), 1);
    assert!(persisted.notifications[0].read_at.is_some());
    assert!(persisted.notifications[0].archived_at.is_some());

    let fire_identity = ironclaw_triggers::TriggerFireIdentity::new(
        caller.tenant_id.clone(),
        ironclaw_triggers::TriggerId::new(),
        Utc::now(),
    );
    let route_thread_id = fire_identity.route_thread_id().as_str().to_string();
    let settlement = ironclaw_triggers::TriggerFailedFireSettlement {
        fire: ironclaw_triggers::TriggerFire {
            identity: fire_identity,
            creator_user_id: caller.user_id.clone(),
            agent_id: caller.agent_id.clone(),
            project_id: None,
            prompt: "scheduled report".to_string(),
            execution_policy: None,
        },
        reason: ironclaw_triggers::TriggerPollerFailureReason::InvalidMaterialization,
    };
    let hook =
        ironclaw_extension_host::channel_triggered_delivery::GenericTriggeredRunDeliveryHook::new(
            None,
            Some(Arc::new(
                ironclaw_assistant::PreSubmitFailureInboxNotifier::new(Arc::clone(
                    &reopened.notification_inbox,
                )),
            )),
            reopened
                .triggered_run_delivery_store_for_test()
                .expect("triggered delivery store"),
        );
    ironclaw_extension_host::channel_triggered_delivery::PostSubmitDeliveryHook::on_trigger_failed_before_submit(
        &hook,
        settlement,
    )
    .await;

    let reopened_bundle = reopened.product_surface(None).expect("product surface");
    let failure_page = query_product_surface_page(
        reopened_bundle.as_ref(),
        caller,
        RebornViewQuery {
            view_id: NOTIFICATIONS_VIEW.id.to_string(),
            params: serde_json::json!({ "limit": 10 }),
            cursor: None,
        },
    )
    .await
    .expect("list settled pre-submit failure");
    let failure = failure_page
        .payload
        .get("notifications")
        .and_then(serde_json::Value::as_array)
        .and_then(|notifications| notifications.first())
        .expect("pre-submit failure notification");
    assert_eq!(failure["action"]["kind"], "none");
    assert!(
        failure["thread_id"].is_null(),
        "a fire that failed before submission has no canonical thread"
    );
    assert!(
        !failure.to_string().contains(&route_thread_id),
        "the trigger route key must not escape as a canonical thread identity"
    );
    drop(reopened_bundle);
    tokio::time::timeout(RUNTIME_SEND_TIMEOUT, reopened.shutdown())
        .await
        .expect("reopened runtime shutdown does not time out")
        .expect("reopened runtime shuts down");
}

#[tokio::test]
async fn runtime_replay_resolves_historical_terminal_approval_notification() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    let gateway: Arc<dyn HostManagedModelGateway> = Arc::new(RecordingGateway {
        reply: "historical approval replay".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let runtime_input = || {
        RebornRuntimeInput::from_build_input(
            crate::deployment::local_filesystem_build_input(
                "runtime-approval-replay-owner",
                storage_root.clone(),
            )
            .with_runtime_policy(standalone_runtime_policy()),
        )
        .with_identity(RebornRuntimeIdentity {
            tenant_id: "runtime-approval-replay-tenant".to_string(),
            agent_id: "runtime-approval-replay-agent".to_string(),
            source_binding_id: "runtime-approval-replay-source".to_string(),
            reply_target_binding_id: "runtime-approval-replay-reply".to_string(),
        })
        .with_model_gateway_override(Arc::clone(&gateway))
    };
    let seed_runtime = tokio::time::timeout(
        PRODUCTION_SHAPED_BUILD_TIMEOUT,
        build_reborn_runtime(runtime_input()),
    )
    .await
    .expect("seed runtime build does not time out")
    .expect("seed runtime builds");
    let filesystem = Arc::clone(&seed_runtime.extension_filesystem);
    tokio::time::timeout(RUNTIME_SEND_TIMEOUT, seed_runtime.shutdown())
        .await
        .expect("seed runtime shutdown does not time out")
        .expect("seed runtime shuts down");

    let tenant_id = TenantId::new("runtime-approval-replay-tenant").expect("tenant");
    let user_id = UserId::new("runtime-approval-replay-owner").expect("user");
    let agent_id = AgentId::new("runtime-approval-replay-agent").expect("agent");
    let thread_id = ThreadId::new("runtime-approval-replay-thread").expect("thread");
    let run_id = TurnRunId::new();
    let recipient = ironclaw_notifications::NotificationRecipient {
        tenant_id: tenant_id.clone(),
        user_id: user_id.clone(),
    };
    let seeded_inbox =
        crate::notification_store_assembly::build_notification_inbox(Arc::clone(&filesystem));
    seeded_inbox
        .publish(ironclaw_notifications::PublishNotificationRequest {
            id: ironclaw_notifications::NotificationId::new("runtime-approval-replay-notification")
                .expect("notification id"),
            recipient: recipient.clone(),
            kind: ironclaw_notifications::NotificationKind::ApprovalRequired,
            severity: ironclaw_notifications::NotificationSeverity::Warning,
            source: ironclaw_notifications::NotificationSource {
                thread_id: Some(thread_id.clone()),
                turn_run_id: Some(run_id),
                lifecycle_ref: Some(
                    ironclaw_notifications::LifecycleRef::new("gate:runtime-approval-replay")
                        .expect("approval lifecycle ref"),
                ),
                credential_providers: Vec::new(),
            },
            action: ironclaw_notifications::NotificationAction::OpenThread {
                thread_id: thread_id.clone(),
            },
            initial_state: ironclaw_notifications::NotificationInitialState::Open,
            occurred_at: Utc::now(),
        })
        .await
        .expect("seed unresolved approval notification");

    let process_store = ironclaw_processes::ProcessJournalStore::new(
        crate::wrap_process_journal_scoped(Arc::clone(&filesystem)),
    );
    let process_id = ironclaw_host_api::ids::ProcessId::from_uuid(run_id.as_uuid());
    let process_scope = ResourceScope {
        tenant_id,
        user_id: user_id.clone(),
        agent_id: Some(agent_id),
        project_id: None,
        mission_id: None,
        thread_id: Some(thread_id),
        invocation_id: InvocationId::new(),
    };
    ironclaw_processes::ProcessSubmissionPort::submit_process(
        &process_store,
        ironclaw_processes::SubmitProcessRequest {
            process_id,
            process_kind: ironclaw_processes::ProcessKind::AgentTurn,
            scope: process_scope.clone(),
            exclusive_within_scope: true,
            operation_id: None,
            owner_user_id: Some(user_id),
            concurrency_class: None,
            parent_process_id: None,
            root_process_id: None,
            spawn_tree_descendant_cap: None,
            dependency: None,
            checkpoint_ref: None,
            input: None,
            created_at: Utc::now(),
            metadata: serde_json::json!({
                "agent_turn": {
                    "product_context": {
                        "origin": "scheduled_trigger"
                    }
                }
            }),
        },
    )
    .await
    .expect("seed historical process submission");
    let claimed = ironclaw_processes::ProcessTransitionPort::claim_next_processes(
        &process_store,
        ironclaw_processes::ClaimProcessesRequest {
            worker_id: ironclaw_processes::ProcessWorkerId::from_trusted(
                "runtime-approval-replay-worker",
            ),
            scope_filter: Some(process_scope),
            process_id_filter: Some(process_id),
            process_kind_filter: Some(ironclaw_processes::ProcessKind::AgentTurn),
            max_processes: 1,
        },
    )
    .await
    .expect("claim historical process")
    .pop()
    .expect("historical process is claimable");
    ironclaw_processes::ProcessTransitionPort::fail_process(
        &process_store,
        ironclaw_processes::FailProcessRequest {
            process_id,
            worker_id: claimed.worker_id,
            lease_token: claimed.lease_token,
            failure: ironclaw_host_api::turn::SanitizedFailure::from_trusted_static(
                "historical_runtime_failure",
            ),
            recovery: ironclaw_processes::ProcessFailureRecovery::Terminal,
            checkpoint_ref: None,
            metadata: None,
        },
    )
    .await
    .expect("seed historical terminal process commit");
    let unresolved = seeded_inbox
        .list(ironclaw_notifications::ListNotificationsRequest {
            recipient: recipient.clone(),
            limit: 10,
            cursor: None,
            include_archived: true,
        })
        .await
        .expect("list seeded approval notification");
    assert_eq!(unresolved.notifications.len(), 1);
    assert!(unresolved.notifications[0].resolved_at.is_none());
    drop(process_store);
    drop(seeded_inbox);
    drop(filesystem);

    let replay_runtime = tokio::time::timeout(
        PRODUCTION_SHAPED_BUILD_TIMEOUT,
        build_reborn_runtime(runtime_input()),
    )
    .await
    .expect("replay runtime build does not time out")
    .expect("replay runtime builds");
    tokio::time::timeout(RUNTIME_POLL_TIMEOUT, async {
        loop {
            let page = replay_runtime
                .notification_inbox
                .list(ironclaw_notifications::ListNotificationsRequest {
                    recipient: recipient.clone(),
                    limit: 10,
                    cursor: None,
                    include_archived: true,
                })
                .await
                .expect("list replayed approval notification");
            if page
                .notifications
                .iter()
                .find(|notification| {
                    notification.kind == ironclaw_notifications::NotificationKind::ApprovalRequired
                        && notification.source.turn_run_id == Some(run_id)
                })
                .is_some_and(|notification| notification.resolved_at.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("composition replay resolves the historical approval notification");
    tokio::time::timeout(RUNTIME_SEND_TIMEOUT, replay_runtime.shutdown())
        .await
        .expect("replay runtime shutdown does not time out")
        .expect("replay runtime shuts down");
}

async fn stream_product_events(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    request: RebornStreamEventsRequest,
) -> Result<RebornStreamEventsResponse, ProductSurfaceError> {
    let response = ironclaw_product_contracts::surface::ProductSurface::stream_events(
        api,
        caller,
        ironclaw_product_contracts::surface::ProductSurfaceStreamRequest {
            stream_id: Some(request.thread_id),
            after_cursor: request
                .after_cursor
                .map(|cursor| cursor.as_str().to_string()),
        },
    )
    .await?;
    let events = response
        .events
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(ProductSurfaceError::internal_from)?;
    Ok(RebornStreamEventsResponse { events })
}

async fn submit_webui_extension_setup(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    package_id: &str,
    request: ProductSetupExtensionRequest,
) -> RebornSetupExtensionResponse {
    let mut input = serde_json::to_value(request).expect("setup request serializes");
    input
        .as_object_mut()
        .expect("setup request serializes as object")
        .insert(
            "extension_id".to_string(),
            serde_json::Value::String(package_id.to_string()),
        );
    let resolution = invoke_product_capability(
        api,
        caller.clone(),
        ironclaw_assistant::EXTENSION_SETUP_SUBMIT_CAPABILITY_ID,
        input,
    )
    .await
    .expect("submit extension setup");
    match resolution {
        Resolution::Done(outcome) if outcome.verdict.is_success() => {}
        other => panic!("extension setup submit did not succeed: {other:?}"),
    }
    query_webui_extension_setup(api, caller, package_id).await
}

async fn install_webui_extension_for_setup(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    package_id: &str,
) {
    let resolution = invoke_product_capability(
        api,
        caller,
        ironclaw_assistant::EXTENSION_INSTALL_CAPABILITY_ID,
        serde_json::json!({ "extension_id": package_id }),
    )
    .await
    .expect("install extension before setup");
    assert!(
        matches!(resolution, Resolution::Done(_) | Resolution::Blocked(_)),
        "install should either complete or park on setup-required credentials: {resolution:?}"
    );
}

#[tokio::test]
async fn standalone_webui_bundle_uses_lifecycle_product_service_for_setup_extension() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "webui lifecycle ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-lifecycle-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-lifecycle-tenant".to_string(),
        agent_id: "runtime-webui-lifecycle-agent".to_string(),
        source_binding_id: "runtime-webui-lifecycle-source".to_string(),
        reply_target_binding_id: "runtime-webui-lifecycle-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-lifecycle-tenant").unwrap(),
        UserId::new("runtime-webui-lifecycle-owner").unwrap(),
        Some(AgentId::new("runtime-webui-lifecycle-agent").unwrap()),
        None,
    );

    let setup = query_webui_extension_setup(bundle.as_ref(), caller.clone(), "github").await;

    assert_eq!(setup.package_ref.id.as_str(), "github");
    // The setup route is caller-visible: an installed extension whose required
    // credential the caller has not supplied is `setup_needed`, not a raw
    // internal checkpoint (§6.1).
    assert_eq!(setup.phase, LifecyclePublicState::SetupNeeded);
    assert!(setup.blockers.is_empty());
    assert_eq!(setup.secrets.len(), 1);
    assert_eq!(setup.secrets[0].name, "github_runtime_token");
    assert_eq!(setup.secrets[0].provider, "github");
    assert!(!setup.secrets[0].optional);
    assert!(!setup.secrets[0].provided);
    assert!(matches!(
        setup.secrets[0].setup,
        RebornExtensionCredentialSetup::ManualToken
    ));
    let google_setup =
        query_webui_extension_setup(bundle.as_ref(), caller.clone(), "google-calendar").await;
    let expected_google_scopes = [
        GOOGLE_CALENDAR_EVENTS_SCOPE.to_string(),
        GOOGLE_CALENDAR_READONLY_SCOPE.to_string(),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    let google_secret = google_setup
        .secrets
        .iter()
        .find(|secret| match &secret.setup {
            RebornExtensionCredentialSetup::OAuth { scopes, .. } => {
                secret.provider == "google"
                    && scopes
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                        == expected_google_scopes
            }
            _ => false,
        })
        .expect("Google Calendar setup should include its OAuth credential");
    assert_eq!(google_secret.provider, "google");
    assert!(!google_secret.provided);
    let RebornExtensionCredentialSetup::OAuth { scopes, .. } = &google_secret.setup else {
        panic!("Google setup secret should use OAuth")
    };
    assert_eq!(
        scopes
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        expected_google_scopes
    );
    let google_setup_json = serde_json::to_value(google_secret).expect("serialize setup secret");
    assert_eq!(google_setup_json["setup"]["kind"], "oauth");
    assert!(
        matches!(
            setup.payload.as_ref(),
            Some(LifecycleProductPayload::ExtensionList { extensions, count })
                if *count == 1
                    && extensions.len() == 1
                    && extensions[0].summary.package_ref.id.as_str() == "github"
                    && extensions[0].summary.credential_requirements.len() == 1
        ),
        "local product surface should use the lifecycle product service package projection"
    );
    assert!(
        !setup.blockers.iter().any(|blocker| matches!(
            blocker,
            LifecycleReadinessBlocker::Runtime { ref_id: Some(ref_id) }
                if ref_id.as_str() == "reborn_lifecycle_service_unwired"
        )),
        "local product surface must not fall back to the default unwired service"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_webui_bundle_exposes_outbound_delivery_targets_view() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "webui outbound ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-outbound-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-outbound-tenant".to_string(),
        agent_id: "runtime-webui-outbound-agent".to_string(),
        source_binding_id: "runtime-webui-outbound-source".to_string(),
        reply_target_binding_id: "runtime-webui-outbound-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-outbound-tenant").unwrap(),
        UserId::new("runtime-webui-outbound-owner").unwrap(),
        Some(AgentId::new("runtime-webui-outbound-agent").unwrap()),
        None,
    );

    let targets_page = query_product_surface_page(
        bundle.as_ref(),
        caller,
        ironclaw_product_contracts::views::RebornViewQuery {
            view_id: ironclaw_assistant::OUTBOUND_DELIVERY_TARGETS_VIEW
                .id
                .to_string(),
            params: serde_json::json!({}),
            cursor: None,
        },
    )
    .await
    .expect("outbound target listing uses composed service");
    let targets: ironclaw_assistant::RebornOutboundDeliveryTargetListResponse =
        serde_json::from_value(targets_page.payload).expect("outbound targets payload");
    // Behavior change (route_current stack deletion): the host no longer seeds a
    // `builtin:web_app` pseudo-target, so a runtime with no channel extension
    // active composes an EMPTY catalog. "Keep it in the app" is now the absence
    // of a delivery call, not a destination the model can address. The view must
    // still resolve and project a well-formed (empty) catalog rather than error.
    assert!(
        targets.targets.is_empty(),
        "with no channel extension active the composed catalog must be empty; saw {:?}",
        targets
            .targets
            .iter()
            .map(|option| option.target.target_id.as_str())
            .collect::<Vec<_>>()
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_webui_bundle_invokes_skill_install_with_scoped_mounts() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "webui skill ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-skill-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-skill-tenant".to_string(),
        agent_id: "runtime-webui-skill-agent".to_string(),
        source_binding_id: "runtime-webui-skill-source".to_string(),
        reply_target_binding_id: "runtime-webui-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-skill-tenant").unwrap(),
        UserId::new("runtime-webui-skill-owner").unwrap(),
        Some(AgentId::new("runtime-webui-skill-agent").unwrap()),
        None,
    );

    let installed = invoke_product_capability(
        bundle.as_ref(),
        caller.clone(),
        ironclaw_assistant::SKILL_INSTALL_CAPABILITY_ID,
        serde_json::json!({
            "name": "product-surface-skill",
            "content": "---\nname: product-surface-skill\n---\n# Product Surface\n"
        }),
    )
    .await
    .expect("skill install uses product capability path");
    match installed {
        Resolution::Done(outcome) if outcome.verdict.is_success() => {}
        other => panic!("skill install did not succeed: {other:?}"),
    }
    let skills_page = query_product_surface_page(
        bundle.as_ref(),
        caller,
        ironclaw_product_contracts::views::RebornViewQuery {
            view_id: ironclaw_assistant::SKILLS_VIEW.id.to_string(),
            params: serde_json::json!({}),
            cursor: None,
        },
    )
    .await
    .expect("skill list uses product view");
    let skills: RebornSkillListResponse =
        serde_json::from_value(skills_page.payload).expect("skills payload");
    assert!(
        skills
            .skills
            .iter()
            .any(|skill| skill.name == "product-surface-skill")
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn webui_route_rejects_list_automations_without_agent_binding() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use ironclaw_webui::webui_v2::{
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER, WebUiV2State, webui_v2_router,
    };
    use tower::ServiceExt;

    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-no-agent-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-no-agent-tenant".to_string(),
        agent_id: "runtime-webui-no-agent-agent".to_string(),
        source_binding_id: "runtime-webui-no-agent-source".to_string(),
        reply_target_binding_id: "runtime-webui-no-agent-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller_without_agent = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-no-agent-tenant").unwrap(),
        UserId::new("runtime-webui-no-agent-owner").unwrap(),
        None,
        None,
    );
    let router = webui_v2_router(WebUiV2State::new(
        bundle,
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER,
    ))
    .layer(axum::Extension(caller_without_agent));

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/webchat/v2/automations")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn webui_operator_diagnostics_route_exposes_composed_readiness_evidence() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use ironclaw_webui::webui_v2::{
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER, WebUiV2Capabilities, WebUiV2State, webui_v2_router,
    };
    use tower::ServiceExt;

    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-diagnostics-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-diagnostics-tenant".to_string(),
        agent_id: "runtime-webui-diagnostics-agent".to_string(),
        source_binding_id: "runtime-webui-diagnostics-source".to_string(),
        reply_target_binding_id: "runtime-webui-diagnostics-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-diagnostics-tenant").unwrap(),
        UserId::new("runtime-webui-diagnostics-owner").unwrap(),
        Some(AgentId::new("runtime-webui-diagnostics-agent").unwrap()),
        None,
    )
    .with_operator_config(true);
    let router = webui_v2_router(WebUiV2State::new(
        bundle,
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER,
    ))
    .layer(axum::Extension(caller))
    .layer(axum::Extension(WebUiV2Capabilities {
        operator_webui_config: true,
    }));

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/webchat/v2/operator/diagnostics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("diagnostics json");
    assert!(
        json["operator_status"]["checks"]
            .as_array()
            .expect("status checks")
            .iter()
            .any(|check| check["id"] == "readiness_composition_profile"
                && check["status"] == "blocked"
                && check["summary"]
                    .as_str()
                    .is_some_and(|summary| summary.contains("reason=dev-only-profile"))),
        "diagnostics route should expose readiness-derived status checks: {json}"
    );
    assert!(
        json["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["reason_code"]
                == "operator_doctor_readiness_composition_profile_blocked"
                && diagnostic["key"] == "readiness_composition_profile"),
        "diagnostics route should expose readiness-derived doctor diagnostics: {json}"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn runtime_product_surface_without_local_runtime_still_lists_automations_from_core_store() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-no-host-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-no-host-tenant".to_string(),
        agent_id: "runtime-webui-no-host-agent".to_string(),
        source_binding_id: "runtime-webui-no-host-source".to_string(),
        reply_target_binding_id: "runtime-webui-no-host-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-no-host-tenant").unwrap(),
        UserId::new("runtime-webui-no-host-owner").unwrap(),
        Some(AgentId::new("runtime-webui-no-host-agent").unwrap()),
        None,
    );

    let response = query_product_surface_page(
        bundle.as_ref(),
        caller,
        ironclaw_product_contracts::views::RebornViewQuery {
            view_id: ironclaw_assistant::AUTOMATIONS_VIEW.id.to_string(),
            params: serde_json::to_value(ProductListAutomationsRequest::default())
                .expect("automation list params"),
            cursor: None,
        },
    )
    .await
    .expect("automation service reads the core trigger repository");

    let automations: ironclaw_assistant::RebornListAutomationsResponse =
        serde_json::from_value(response.payload).expect("automations payload");
    assert!(automations.automations.is_empty());
    assert!(!automations.scheduler_enabled);
    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_webui_setup_extension_stores_and_rotates_runtime_credentials() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "webui lifecycle ok".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-credential-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-credential-tenant".to_string(),
        agent_id: "runtime-webui-credential-agent".to_string(),
        source_binding_id: "runtime-webui-credential-source".to_string(),
        reply_target_binding_id: "runtime-webui-credential-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-credential-tenant").unwrap(),
        UserId::new("runtime-webui-credential-owner").unwrap(),
        Some(AgentId::new("runtime-webui-credential-agent").unwrap()),
        None,
    );
    install_webui_extension_for_setup(bundle.as_ref(), caller.clone(), "github").await;
    let first = submit_webui_extension_setup(
        bundle.as_ref(),
        caller.clone(),
        "github",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(serde_json::json!({
                "secrets": {
                    "github_runtime_token": "ghp_first_token"
                },
                "fields": {}
            })),
        },
    )
    .await;
    assert_eq!(first.secrets.len(), 1);
    assert!(first.secrets[0].provided);
    let first_credential_ref = first.secrets[0]
        .credential_ref
        .clone()
        .expect("credential ref");

    let second = submit_webui_extension_setup(
        bundle.as_ref(),
        caller,
        "github",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(serde_json::json!({
                "secrets": {
                    "github_runtime_token": "ghp_second_token"
                },
                "fields": {}
            })),
        },
    )
    .await;
    assert_eq!(second.secrets.len(), 1);
    assert!(second.secrets[0].provided);
    assert_eq!(
        second.secrets[0].credential_ref.as_deref(),
        Some(first_credential_ref.as_str()),
        "reconfigure should rotate the existing account instead of creating a duplicate"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_webui_bundle_routes_approval_gates_into_interaction_service() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-approval-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-approval-tenant".to_string(),
        agent_id: "runtime-webui-approval-agent".to_string(),
        source_binding_id: "runtime-webui-approval-source".to_string(),
        reply_target_binding_id: "runtime-webui-approval-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-approval-tenant").unwrap(),
        UserId::new("runtime-webui-approval-owner").unwrap(),
        Some(AgentId::new("runtime-webui-approval-agent").unwrap()),
        None,
    );
    let created = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        CREATE_THREAD_COMMAND,
        ProductCreateThreadRequest {
            client_action_id: Some("create-webui-approval-thread".to_string()),
            requested_thread_id: None,
            project_id: None,
        },
    )
    .await
    .expect("create thread");
    let gate_ref = approval_gate_ref(ApprovalRequestId::new()).expect("approval gate");

    let err = invoke_product_command::<_, ironclaw_assistant::RebornResolveGateResponse>(
        bundle.as_ref(),
        caller,
        RESOLVE_GATE_COMMAND,
        ProductResolveGateRequest {
            client_action_id: Some("resolve-webui-approval-gate".to_string()),
            thread_id: Some(created.thread.thread_id.to_string()),
            run_id: Some(TurnRunId::new().to_string()),
            gate_ref: Some(gate_ref.as_str().to_string()),
            resolution: Some("approved".to_string()),
            always: None,
            credential_ref: None,
            answer: None,
            feedback: None,
        },
    )
    .await
    .expect_err("missing approval gate should reach approval interaction service");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.kind, ProductSurfaceErrorKind::NotFound);
    assert_eq!(err.status_code, 404);
    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_webui_bundle_routes_auth_gates_into_interaction_service() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-webui-auth-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-auth-tenant".to_string(),
        agent_id: "runtime-webui-auth-agent".to_string(),
        source_binding_id: "runtime-webui-auth-source".to_string(),
        reply_target_binding_id: "runtime-webui-auth-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-auth-tenant").unwrap(),
        UserId::new("runtime-webui-auth-owner").unwrap(),
        Some(AgentId::new("runtime-webui-auth-agent").unwrap()),
        None,
    );
    let created = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        CREATE_THREAD_COMMAND,
        ProductCreateThreadRequest {
            client_action_id: Some("create-webui-auth-thread".to_string()),
            requested_thread_id: None,
            project_id: None,
        },
    )
    .await
    .expect("create thread");

    let err = invoke_product_command::<_, ironclaw_assistant::RebornResolveGateResponse>(
        bundle.as_ref(),
        caller,
        RESOLVE_GATE_COMMAND,
        ProductResolveGateRequest {
            client_action_id: Some("resolve-webui-auth-gate".to_string()),
            thread_id: Some(created.thread.thread_id.to_string()),
            run_id: Some(TurnRunId::new().to_string()),
            gate_ref: Some("gate:hook-auth-missing".to_string()),
            resolution: Some("declined".to_string()),
            always: None,
            credential_ref: None,
            answer: None,
            feedback: None,
        },
    )
    .await
    .expect_err("missing auth gate should reach auth interaction service");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.kind, ProductSurfaceErrorKind::BlockedAuthentication);
    assert_eq!(err.status_code, 404);
    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_webui_bundle_records_selectable_filesystem_skill_context() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    seed_user_skill(
        &storage_root,
        "runtime-webui-skill-tenant",
        "runtime-webui-skill-user",
        "webui-helper",
        skill_md(
            "webui-helper",
            "webui helper description",
            "WEBUI_HELPER_PROMPT_SENTINEL",
        ),
    )
    .await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "webui skill context ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(with_test_authenticated_session_channel(
        crate::deployment::local_filesystem_build_input("runtime-webui-skill-owner", storage_root)
            .with_runtime_policy(standalone_runtime_policy()),
    ))
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-webui-skill-tenant".to_string(),
        agent_id: "runtime-webui-skill-agent".to_string(),
        source_binding_id: "runtime-webui-skill-source".to_string(),
        reply_target_binding_id: "runtime-webui-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    let webui_user_id = UserId::new("runtime-webui-skill-user").unwrap();
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-webui-skill-tenant").unwrap(),
        webui_user_id.clone(),
        Some(AgentId::new("runtime-webui-skill-agent").unwrap()),
        None,
    );
    let created = invoke_product_command(
        bundle.as_ref(),
        caller.clone(),
        CREATE_THREAD_COMMAND,
        ProductCreateThreadRequest {
            client_action_id: Some("create-webui-skill-thread".to_string()),
            requested_thread_id: None,
            project_id: None,
        },
    )
    .await
    .expect("create thread");
    let submitted = invoke_product_command(
        bundle.as_ref(),
        caller,
        SUBMIT_TURN_COMMAND,
        ProductSubmitTurnRequest {
            extension_id: Some(TEST_SESSION_EXTENSION_ID.to_string()),
            client_action_id: Some("send-webui-skill-message".to_string()),
            thread_id: Some(created.thread.thread_id.to_string()),
            content: Some("$webui-helper please help".to_string()),
            attachments: Vec::new(),
            model: None,
            mention_roots: Vec::new(),
        },
    )
    .await
    .expect("submit turn");
    let RebornSubmitTurnResponse::Submitted {
        thread_id,
        accepted_message_ref,
        ..
    } = submitted
    else {
        panic!("webui submit should start a run");
    };
    let resolved_run_profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .expect("resolve run profile");
    let source = runtime
        .webui_skill_activation_source()
        .expect("webui skill activation source");
    let turn_scope = TurnScope::new_with_owner(
        TenantId::new("runtime-webui-skill-tenant").unwrap(),
        Some(AgentId::new("runtime-webui-skill-agent").unwrap()),
        None,
        thread_id.clone(),
        Some(webui_user_id.clone()),
    );
    let context = LoopRunContext::new(
        turn_scope,
        TurnId::new(),
        TurnRunId::new(),
        resolved_run_profile,
    )
    .with_accepted_message_ref(accepted_message_ref)
    .with_actor(TurnActor::new(webui_user_id));
    let selected = source
        .load_skill_context_candidates(&context)
        .await
        .expect("webui-recorded skill context should load");
    let combined_skill_context = selected
        .iter()
        .map(|candidate| candidate.loaded_skill_md().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    // Default `listing` injection: the explicitly-mentioned skill loads its
    // full body; the bundled system skills collapse into one additional
    // `available-skills` listing candidate (description-only).
    assert!(combined_skill_context.contains("webui helper description"));
    assert!(combined_skill_context.contains("WEBUI_HELPER_PROMPT_SENTINEL"));
    let listing = selected
        .iter()
        .filter_map(|candidate| candidate.discoverable_metadata())
        .find(|(name, _)| *name == "available-skills")
        .map(|(_, listing)| listing.to_string())
        .expect("available-skills listing candidate");
    assert!(
        !listing.contains("WEBUI_HELPER_PROMPT_SENTINEL"),
        "listing must not carry skill bodies"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}
