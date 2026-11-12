//! Thread listing (including needs-approval filtering) and the notification inbox.

use super::*;

struct ThreadScopedApprovalInteractionService {
    pending_thread_ids: HashSet<ThreadId>,
}

#[async_trait]
impl ApprovalInteractionService for ThreadScopedApprovalInteractionService {
    async fn list_pending(
        &self,
        request: ListPendingApprovalsRequest,
    ) -> Result<ListPendingApprovalsResponse, ProductSurfaceFailure> {
        if !self.pending_thread_ids.contains(&request.scope.thread_id) {
            return Ok(ListPendingApprovalsResponse { approvals: vec![] });
        }
        let approval_request_id = ApprovalRequestId::new();
        Ok(ListPendingApprovalsResponse {
            approvals: vec![PendingApprovalInteractionView {
                scope: ApprovalInteractionScope::from_turn(&request.scope, &request.actor),
                run_id: TurnRunId::new(),
                gate_ref: approval_gate_ref(approval_request_id).expect("approval gate ref"),
                approval_request_id,
                summary: "Approval required".to_string(),
                action: ApprovalInteractionActionView::Dispatch {
                    capability_id: CapabilityId::new("demo.echo").expect("capability id"),
                },
            }],
        })
    }

    async fn resolve(
        &self,
        _request: ResolveApprovalInteractionRequest,
    ) -> Result<ResolveApprovalInteractionResponse, ProductSurfaceFailure> {
        panic!("resolve is not used by thread approval filter tests")
    }
}

struct ActorFallbackApprovalInteractionService {
    pending_thread_id: ThreadId,
    tenant_id: TenantId,
    owner_user_id: UserId,
    agent_id: AgentId,
    project_id: Option<ProjectId>,
}

struct FixedLegacyApprovalInteractionService {
    pending: PendingApprovalInteractionView,
}

#[async_trait]
impl ApprovalInteractionService for FixedLegacyApprovalInteractionService {
    async fn list_pending(
        &self,
        request: ListPendingApprovalsRequest,
    ) -> Result<ListPendingApprovalsResponse, ProductSurfaceFailure> {
        if request.scope.thread_id != self.pending.scope.thread_id
            || request.scope.tenant_id != self.pending.scope.tenant_id
            || request.scope.agent_id != self.pending.scope.agent_id
            || request.scope.project_id != self.pending.scope.project_id
            || request.scope.has_explicit_thread_owner()
            || request.actor.user_id != self.pending.scope.user_id
        {
            return Ok(ListPendingApprovalsResponse { approvals: vec![] });
        }
        Ok(ListPendingApprovalsResponse {
            approvals: vec![self.pending.clone()],
        })
    }

    async fn resolve(
        &self,
        _request: ResolveApprovalInteractionRequest,
    ) -> Result<ResolveApprovalInteractionResponse, ProductSurfaceFailure> {
        panic!("resolve is not used by legacy notification migration tests")
    }
}

#[async_trait]
impl ApprovalInteractionService for ActorFallbackApprovalInteractionService {
    async fn list_pending(
        &self,
        request: ListPendingApprovalsRequest,
    ) -> Result<ListPendingApprovalsResponse, ProductSurfaceFailure> {
        let is_expected_scope = request.scope.thread_id == self.pending_thread_id
            && request.scope.tenant_id == self.tenant_id
            && request.scope.agent_id.as_ref() == Some(&self.agent_id)
            && request.scope.project_id == self.project_id
            && !request.scope.has_explicit_thread_owner()
            && request.actor.user_id == self.owner_user_id;
        if !is_expected_scope {
            return Ok(ListPendingApprovalsResponse { approvals: vec![] });
        }
        let approval_request_id = ApprovalRequestId::new();
        Ok(ListPendingApprovalsResponse {
            approvals: vec![PendingApprovalInteractionView {
                scope: ApprovalInteractionScope::from_turn(&request.scope, &request.actor),
                run_id: TurnRunId::new(),
                gate_ref: approval_gate_ref(approval_request_id).expect("approval gate ref"),
                approval_request_id,
                summary: "Approval required".to_string(),
                action: ApprovalInteractionActionView::Dispatch {
                    capability_id: CapabilityId::new("demo.echo").expect("capability id"),
                },
            }],
        })
    }

    async fn resolve(
        &self,
        _request: ResolveApprovalInteractionRequest,
    ) -> Result<ResolveApprovalInteractionResponse, ProductSurfaceFailure> {
        panic!("resolve is not used by actor-fallback approval list tests")
    }
}

async fn setup_ownerless_trigger_thread(
    thread_service: &Arc<InMemorySessionThreadService>,
    caller: &ProductSurfaceCaller,
    thread_id: &str,
) -> ThreadId {
    let tid = ThreadId::new(thread_id).expect("valid trigger thread id");
    let mut scope = trigger_thread_scope_for(caller);
    scope.owner_user_id = None;
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope,
            thread_id: Some(tid.clone()),
            created_by_actor_id: "system".to_string(),
            title: Some("Legacy trigger thread".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-gate-automation",
            )),
        })
        .await
        .expect("legacy trigger thread stored");
    tid
}

async fn query_threads<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    mut request: ProductListThreadsRequest,
) -> Result<RebornListThreadsResponse, ProductSurfaceError> {
    let cursor = request.cursor.take();
    let page =
        query_product_surface_page(services, caller, THREADS_VIEW.query(request, cursor)?).await?;
    THREADS_VIEW.decode_page(page)
}

// Regression for the missing-error-path-test review (Medium): the
// new `list_threads` service path must fail closed until a backend
// override for `list_threads_for_scope` is wired. The default
// `SessionThreadService` impl returns `Backend(...)`, and the
// service is supposed to translate that into a retryable
// `service_unavailable` (HTTP 503) — never an empty thread list
// that pretends the caller owns nothing. This test pins the wire
// contract so a future regression that quietly returns Ok([]) on a
// missing backend would break the test, not silently mislead
// callers.
#[tokio::test]
async fn list_threads_unimplemented_backend_returns_service_unavailable() {
    // `ScopeMismatchThreadStub` is reused here because it
    // intentionally does NOT override the trait's default
    // `list_threads_for_scope` impl, so the service sees the
    // unimplemented-enumeration error path. The in-memory backend
    // grew a real enumeration impl (standalone needed working
    // sidebar listing), so it can no longer stand in for a backend
    // without enumeration support.
    let services = session_services(
        Arc::new(ScopeMismatchThreadStub),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let error = query_threads(&services, caller(), ProductListThreadsRequest::default())
        .await
        .expect_err(
            "list_threads must fail closed when the SessionThreadService backend \
             does not implement list_threads_for_scope",
        );
    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(error.status_code, 503);
    assert!(
        error.retryable,
        "Backend errors are retryable so the browser can re-poll once a v2-aware \
         backend overrides list_threads_for_scope",
    );

    // Confirm the wire shape is the snake_case enum the WebUi handler maps
    // to its `error` field; matching on the variant alone would still pass
    // if someone changed `#[serde(rename_all = ...)]` to PascalCase.
    let json = serde_json::to_value(&error).expect("serialize");
    assert_eq!(
        json["code"], "unavailable",
        "wire code must be snake_case `unavailable`; got: {json}"
    );
    assert_eq!(json["retryable"], true);
}

#[tokio::test]
async fn a_repeated_notification_mutation_reports_that_nothing_changed() {
    use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
    use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
    use ironclaw_host_api::path::{MountAlias, VirtualPath};
    use ironclaw_notifications::{
        NotificationAction, NotificationId, NotificationInboxStore, NotificationInboxStorePort,
        NotificationInitialState, NotificationKind, NotificationRecipient, NotificationSeverity,
        NotificationSource, PublishNotificationRequest,
    };

    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/notifications").expect("alias"),
        VirtualPath::new("/engine/test/mutation-response").expect("target"),
        MountPermissions::read_write_list_delete(),
    )])
    .expect("mount view");
    let inbox = Arc::new(NotificationInboxStore::new(
        Arc::new(ScopedFilesystem::with_fixed_view(
            Arc::new(InMemoryBackend::new()),
            mounts,
        )),
        ironclaw_notifications::NOTIFICATION_INBOX_MAX_RECORDS,
    ));

    let actor = caller();
    let thread_id = ThreadId::new("thread-mutation-response").expect("thread");
    inbox
        .publish(PublishNotificationRequest {
            id: NotificationId::new("notification-settled").expect("id"),
            recipient: NotificationRecipient {
                tenant_id: actor.tenant_id.clone(),
                user_id: actor.user_id.clone(),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::Warning,
            source: NotificationSource {
                thread_id: Some(thread_id.clone()),
                turn_run_id: None,
                lifecycle_ref: None,
                credential_providers: Vec::new(),
            },
            action: NotificationAction::OpenThread { thread_id },
            initial_state: NotificationInitialState::Open,
            occurred_at: Utc::now(),
        })
        .await
        .expect("seed a notification");

    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_notification_inbox(inbox.clone());

    async fn mark_read(services: &RebornServices) -> ProductNotificationMutationResponse {
        let response = ProductSurface::invoke(
            services,
            caller(),
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(NOTIFICATIONS_MARK_READ_COMMAND_ID)
                    .expect("mark-read operation"),
                input: serde_json::to_value(ProductNotificationMutationRequest {
                    notification_id: "notification-settled".to_string(),
                })
                .expect("mutation input"),
                activity_id: ActivityId::new(),
            },
        )
        .await
        .expect("mark-read succeeds");
        serde_json::from_value(response.output).expect("mutation response")
    }

    // The command must report what the store changed. Answering `true` for the
    // repeat would hand the client evidence of a durable write that never
    // happened.
    assert!(
        mark_read(&services).await.updated,
        "the first mark-read changes durable state"
    );
    assert!(
        !mark_read(&services).await.updated,
        "a notification that is already read reports no change"
    );

    let all_read = ProductSurface::invoke(
        &services,
        caller(),
        ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(NOTIFICATIONS_MARK_ALL_READ_COMMAND_ID)
                .expect("mark-all-read operation"),
            input: serde_json::json!({}),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect("mark-all-read succeeds");
    let all_read: ProductNotificationMutationResponse =
        serde_json::from_value(all_read.output).expect("mark-all-read response");
    assert!(
        !all_read.updated,
        "nothing is unread, so mark-all-read reports no change"
    );
}

#[tokio::test]
#[traced_test]
async fn notifications_unwired_backend_returns_retryable_service_unavailable() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let error = ProductSurface::query(
        &services,
        caller(),
        ironclaw_product_contracts::surface::ProductSurfaceQueryRequest {
            view_id: NOTIFICATIONS_VIEW.id.to_string(),
            input: json!({}),
            cursor: None,
            limit: None,
        },
    )
    .await
    .expect_err("an unwired notification backend must fail visibly");

    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(error.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(error.status_code, 503);
    assert!(error.retryable);
    assert!(
        logs_contain("notification inbox backend unavailable at the product boundary"),
        "the product boundary must log a fixed diagnostic category for inbox backend failures"
    );
    assert!(
        !logs_contain("notification inbox store is not configured"),
        "the bound backend reason carries filesystem, CAS, and serde text, so it must never \
         reach the logs; log the fixed category instead"
    );
}

#[tokio::test]
async fn list_threads_hides_automation_trigger_threads() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let caller = caller();
    let visible_thread_id = ThreadId::new("thread-visible").expect("visible thread id");
    let automation_thread_id = ThreadId::new("thread-automation").expect("automation thread id");
    let malformed_metadata_thread_id =
        ThreadId::new("thread-malformed-metadata").expect("malformed metadata thread id");

    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(visible_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("Visible chat".to_string()),
            metadata_json: Some(json!({ "source": "webui" }).to_string()),
        })
        .await
        .expect("visible thread");
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(automation_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("Automation run".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-scheduled-summary",
            )),
        })
        .await
        .expect("automation thread");
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(malformed_metadata_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("Malformed metadata chat".to_string()),
            metadata_json: Some(format!(
                r#"{{"source":"{AUTOMATION_TRIGGER_THREAD_SOURCE_TAG}""#
            )),
        })
        .await
        .expect("malformed metadata thread");

    let response = query_threads(&services, caller, ProductListThreadsRequest::default())
        .await
        .expect("list threads");
    let thread_ids = response
        .threads
        .iter()
        .map(|thread| thread.thread_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(thread_ids.len(), 2);
    assert!(thread_ids.contains(&visible_thread_id));
    assert!(thread_ids.contains(&malformed_metadata_thread_id));
    assert!(
        !thread_ids.contains(&automation_thread_id),
        "automation trigger threads should be accessible by direct id but hidden from the chat list",
    );
}

#[tokio::test]
async fn list_threads_needs_approval_returns_only_automation_threads_with_pending_approval() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let caller = caller();
    let pending_thread_id = ThreadId::new("thread-pending").expect("pending thread id");
    let automation_pending_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-automation-pending").await;
    let approval_service = Arc::new(ThreadScopedApprovalInteractionService {
        pending_thread_ids: [
            pending_thread_id.clone(),
            automation_pending_thread_id.clone(),
        ]
        .into_iter()
        .collect(),
    });
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service_with_trigger_thread(
        automation_pending_thread_id.clone(),
        &caller,
    ))
    .with_approval_interactions(approval_service);

    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(pending_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("Normal chat pending approval".to_string()),
            metadata_json: Some(json!({ "source": "webui" }).to_string()),
        })
        .await
        .expect("normal pending thread");

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default().set_needs_approval(true),
    )
    .await
    .expect("list approval threads");
    let thread_ids = response
        .threads
        .iter()
        .map(|thread| thread.thread_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        thread_ids,
        vec![automation_pending_thread_id],
        "notification thread filter must exclude non-automation approval threads",
    );
}

#[tokio::test]
async fn list_threads_needs_approval_queries_pending_with_run_scope_shape() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let caller = caller();
    let automation_pending_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-automation-run-scope").await;
    let trigger_scope = trigger_run_thread_scope_for(&caller);
    let approval_service = Arc::new(ActorFallbackApprovalInteractionService {
        pending_thread_id: automation_pending_thread_id.clone(),
        tenant_id: caller.tenant_id.clone(),
        owner_user_id: trigger_scope.creator_user_id.clone(),
        agent_id: caller.agent_id.clone().expect("agent id"),
        project_id: caller.project_id.clone(),
    });
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service_with_trigger_thread(
        automation_pending_thread_id.clone(),
        &caller,
    ))
    .with_approval_interactions(approval_service);

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default().set_needs_approval(true),
    )
    .await
    .expect("list approval threads");
    let thread_ids = response
        .threads
        .iter()
        .map(|thread| thread.thread_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        thread_ids,
        vec![automation_pending_thread_id],
        "notification approval lookup must use the same actor-fallback turn scope shape as blocked run state",
    );
}

#[tokio::test]
async fn list_threads_needs_approval_uses_bounded_run_candidates() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let caller = caller();
    let automation_pending_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-automation-bounded").await;
    let trigger_scope = trigger_run_thread_scope_for(&caller);
    let approval_service = Arc::new(ActorFallbackApprovalInteractionService {
        pending_thread_id: automation_pending_thread_id.clone(),
        tenant_id: caller.tenant_id.clone(),
        owner_user_id: trigger_scope.creator_user_id.clone(),
        agent_id: caller.agent_id.clone().expect("agent id"),
        project_id: caller.project_id.clone(),
    });
    let automation_service =
        automation_service_with_trigger_thread(automation_pending_thread_id.clone(), &caller);
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone())
    .with_approval_interactions(approval_service);

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default().set_needs_approval(true),
    )
    .await
    .expect("list approval threads");

    assert_eq!(response.threads.len(), 1);
    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(list_calls[0].limit, 20);
    assert_eq!(list_calls[0].run_limit, 20);
    assert!(list_calls[0].include_completed);
    assert_eq!(
        automation_service.resolve_calls(),
        vec![automation_pending_thread_id],
        "notification lookup should still resolve the thread id once to recover the true trigger creator scope",
    );
}

#[tokio::test]
async fn list_threads_needs_approval_finds_legacy_ownerless_automation_thread() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let caller = caller();
    let automation_pending_thread_id =
        setup_ownerless_trigger_thread(&thread_service, &caller, "thread-automation-ownerless")
            .await;
    let trigger_scope = trigger_run_thread_scope_for(&caller);
    let approval_service = Arc::new(ActorFallbackApprovalInteractionService {
        pending_thread_id: automation_pending_thread_id.clone(),
        tenant_id: caller.tenant_id.clone(),
        owner_user_id: trigger_scope.creator_user_id.clone(),
        agent_id: caller.agent_id.clone().expect("agent id"),
        project_id: caller.project_id.clone(),
    });
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service_with_trigger_thread(
        automation_pending_thread_id.clone(),
        &caller,
    ))
    .with_approval_interactions(approval_service);

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default().set_needs_approval(true),
    )
    .await
    .expect("list approval threads");
    let thread_ids = response
        .threads
        .iter()
        .map(|thread| thread.thread_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        thread_ids,
        vec![automation_pending_thread_id],
        "notification approval lookup must include legacy ownerless automation run threads",
    );
}

#[tokio::test]
async fn notifications_backfill_open_legacy_automation_approval_once() {
    use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
    use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
    use ironclaw_host_api::path::{MountAlias, VirtualPath};
    use ironclaw_notifications::{
        ListNotificationsRequest, NotificationId, NotificationInboxStore,
        NotificationInboxStorePort, NotificationMutationRequest, NotificationRecipient,
    };

    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let thread_id =
        setup_ownerless_trigger_thread(&thread_service, &caller, "thread-legacy-gate-backfill")
            .await;
    let trigger_scope = trigger_run_thread_scope_for(&caller);
    let run_id = automation_run_id();
    let approval_request_id = ApprovalRequestId::new();
    let gate_ref = approval_gate_ref(approval_request_id).expect("approval gate ref");
    let approval_turn_scope = TurnScope::new(
        caller.tenant_id.clone(),
        caller.agent_id.clone(),
        caller.project_id.clone(),
        thread_id.clone(),
    );
    let approval_service = Arc::new(FixedLegacyApprovalInteractionService {
        pending: PendingApprovalInteractionView {
            scope: ApprovalInteractionScope::from_turn(
                &approval_turn_scope,
                &TurnActor::new(trigger_scope.creator_user_id.clone()),
            ),
            run_id,
            gate_ref: gate_ref.clone(),
            approval_request_id,
            summary: "Approval required".to_string(),
            action: ApprovalInteractionActionView::Dispatch {
                capability_id: CapabilityId::new("demo.echo").expect("capability id"),
            },
        },
    });
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/notifications").expect("alias"),
        VirtualPath::new("/engine/test/legacy-gate-backfill").expect("target"),
        MountPermissions::read_write_list_delete(),
    )])
    .expect("mount view");
    let inbox = Arc::new(NotificationInboxStore::new(
        Arc::new(ScopedFilesystem::with_fixed_view(
            Arc::new(InMemoryBackend::new()),
            mounts,
        )),
        ironclaw_notifications::NOTIFICATION_INBOX_MAX_RECORDS,
    ));
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service_with_trigger_thread(
        thread_id.clone(),
        &caller,
    ))
    .with_approval_interactions(approval_service.clone())
    .with_notification_inbox(inbox.clone());

    async fn query_notifications(
        services: &RebornServices,
        caller: ProductSurfaceCaller,
    ) -> ProductListNotificationsResponse {
        let page = ProductSurface::query(
            services,
            caller,
            ironclaw_product_contracts::surface::ProductSurfaceQueryRequest {
                view_id: NOTIFICATIONS_VIEW.id.to_string(),
                input: json!({}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .expect("list notifications");
        let payload = page.items.into_iter().next().expect("notification payload");
        serde_json::from_value(payload).expect("notification response")
    }

    let first = query_notifications(&services, caller.clone()).await;
    let second = query_notifications(&services, caller.clone()).await;
    assert_eq!(first.notifications.len(), 1);
    assert_eq!(second.notifications.len(), 1, "backfill is idempotent");
    let notification = &second.notifications[0];
    let lifecycle_key =
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, gate_ref.as_str().as_bytes());
    assert_eq!(
        notification.id,
        format!("run:{run_id}:approval:{lifecycle_key}"),
        "the migration must use the producer's stable id so normal gate resolution settles it",
    );
    assert_eq!(notification.kind, ProductNotificationKind::ApprovalRequired);
    assert_eq!(notification.thread_id, Some(thread_id.to_string()));
    assert_eq!(notification.turn_run_id, Some(run_id.to_string()));
    assert!(notification.read_at.is_none());
    assert!(notification.resolved_at.is_none());

    let read_at = "2030-01-01T00:00:00Z".parse().expect("read time");
    inbox
        .mark_read(NotificationMutationRequest {
            recipient: NotificationRecipient {
                tenant_id: caller.tenant_id.clone(),
                user_id: caller.user_id.clone(),
            },
            notification_id: NotificationId::new(notification.id.clone()).expect("notification id"),
            occurred_at: read_at,
        })
        .await
        .expect("mark migrated notification read");
    let before_restart = inbox
        .list(ListNotificationsRequest {
            recipient: NotificationRecipient {
                tenant_id: caller.tenant_id.clone(),
                user_id: caller.user_id.clone(),
            },
            limit: 10,
            cursor: None,
            include_archived: true,
        })
        .await
        .expect("read durable state before restart")
        .notifications
        .into_iter()
        .next()
        .expect("migrated notification before restart");

    let restarted = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service_with_trigger_thread(thread_id, &caller))
        .with_approval_interactions(approval_service)
        .with_notification_inbox(inbox.clone());
    let after_restart_page = query_notifications(&restarted, caller.clone()).await;
    assert_eq!(
        after_restart_page.notifications.len(),
        1,
        "a restart backfill must not duplicate the stable notification id",
    );
    let after_restart = inbox
        .list(ListNotificationsRequest {
            recipient: NotificationRecipient {
                tenant_id: caller.tenant_id,
                user_id: caller.user_id,
            },
            limit: 10,
            cursor: None,
            include_archived: true,
        })
        .await
        .expect("read durable state after restart")
        .notifications
        .into_iter()
        .next()
        .expect("migrated notification after restart");
    assert_eq!(after_restart.read_at, Some(read_at));
    assert_eq!(after_restart.resolved_at, before_restart.resolved_at);
    assert_eq!(after_restart.archived_at, before_restart.archived_at);
    assert_eq!(after_restart.updated_at, before_restart.updated_at);
}

#[tokio::test]
async fn notification_list_survives_a_failed_legacy_approval_backfill_once() {
    use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
    use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
    use ironclaw_host_api::path::{MountAlias, VirtualPath};
    use ironclaw_notifications::{
        NotificationAction, NotificationId, NotificationInboxStore, NotificationInboxStorePort,
        NotificationInitialState, NotificationKind, NotificationRecipient, NotificationSeverity,
        NotificationSource, PublishNotificationRequest,
    };

    let caller = caller();
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/notifications").expect("alias"),
        VirtualPath::new("/engine/test/legacy-backfill-failure").expect("target"),
        MountPermissions::read_write_list_delete(),
    )])
    .expect("mount view");
    let inbox = Arc::new(NotificationInboxStore::new(
        Arc::new(ScopedFilesystem::with_fixed_view(
            Arc::new(InMemoryBackend::new()),
            mounts,
        )),
        ironclaw_notifications::NOTIFICATION_INBOX_MAX_RECORDS,
    ));
    let thread_id = ThreadId::new("thread-existing-notification").expect("thread");
    inbox
        .publish(PublishNotificationRequest {
            id: NotificationId::new("notification-existing").expect("notification id"),
            recipient: NotificationRecipient {
                tenant_id: caller.tenant_id.clone(),
                user_id: caller.user_id.clone(),
            },
            kind: NotificationKind::RunCompleted,
            severity: NotificationSeverity::Success,
            source: NotificationSource {
                thread_id: Some(thread_id.clone()),
                turn_run_id: None,
                lifecycle_ref: None,
                credential_providers: Vec::new(),
            },
            action: NotificationAction::OpenThread { thread_id },
            initial_state: NotificationInitialState::Resolved,
            occurred_at: Utc::now(),
        })
        .await
        .expect("seed healthy durable inbox");

    let automation_service = Arc::new(StaticAutomationService::new(vec![]).with_list_error(
        ProductSurfaceError {
            code: ProductSurfaceErrorCode::Unavailable,
            kind: ProductSurfaceErrorKind::ServiceUnavailable,
            status_code: 503,
            retryable: true,
            field: None,
            validation_code: None,
        },
    ));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone())
    .with_notification_inbox(inbox);

    for _ in 0..2 {
        let page = ProductSurface::query(
            &services,
            caller.clone(),
            ironclaw_product_contracts::surface::ProductSurfaceQueryRequest {
                view_id: NOTIFICATIONS_VIEW.id.to_string(),
                input: json!({}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .expect("a migration failure must not hide the healthy durable inbox");
        let response: ProductListNotificationsResponse =
            serde_json::from_value(page.items.into_iter().next().expect("notification payload"))
                .expect("notification response");
        assert_eq!(response.notifications.len(), 1);
        assert_eq!(response.notifications[0].id, "notification-existing");
    }
    assert_eq!(
        automation_service.list_calls().len(),
        1,
        "a failed best-effort migration is attempted once per process scope",
    );
}

#[tokio::test]
async fn list_threads_needs_approval_uses_automation_name_when_thread_title_missing() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let caller = caller();
    let automation_pending_thread_id =
        ThreadId::new("thread-automation-titleless").expect("valid trigger thread id");
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: trigger_thread_scope_for(&caller),
            thread_id: Some(automation_pending_thread_id.clone()),
            created_by_actor_id: "system".to_string(),
            title: None,
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-gate-automation",
            )),
        })
        .await
        .expect("titleless trigger thread stored");
    let trigger_scope = trigger_run_thread_scope_for(&caller);
    let approval_service = Arc::new(ActorFallbackApprovalInteractionService {
        pending_thread_id: automation_pending_thread_id.clone(),
        tenant_id: caller.tenant_id.clone(),
        owner_user_id: trigger_scope.creator_user_id.clone(),
        agent_id: caller.agent_id.clone().expect("agent id"),
        project_id: caller.project_id.clone(),
    });
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service_with_trigger_thread(
        automation_pending_thread_id.clone(),
        &caller,
    ))
    .with_approval_interactions(approval_service);

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default().set_needs_approval(true),
    )
    .await
    .expect("list approval threads");

    assert_eq!(response.threads.len(), 1);
    assert_eq!(
        response.threads[0].title.as_deref(),
        Some("Gate test automation"),
        "notification approval thread fallback title should use the automation name",
    );
}

#[tokio::test]
async fn list_threads_needs_approval_checks_candidate_automation_thread() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let caller = caller();
    let automation_pending_thread_id =
        setup_trigger_thread(&thread_service, &caller, "thread-automation-candidate").await;
    let approval_service = Arc::new(ThreadScopedApprovalInteractionService {
        pending_thread_ids: [automation_pending_thread_id.clone()].into_iter().collect(),
    });
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(Arc::new(
        StaticAutomationService::new(Vec::new()).with_resolve_scope_for_thread(
            automation_pending_thread_id.clone(),
            trigger_run_thread_scope_for(&caller),
        ),
    ))
    .with_approval_interactions(approval_service);

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default()
            .set_needs_approval(true)
            .set_candidate_thread_id(automation_pending_thread_id.as_str()),
    )
    .await
    .expect("list approval threads");
    let thread_ids = response
        .threads
        .iter()
        .map(|thread| thread.thread_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        thread_ids,
        vec![automation_pending_thread_id],
        "candidate thread must still be automation-authorized before entering notifications",
    );
}

#[tokio::test]
async fn list_threads_breaks_out_when_cursor_does_not_advance_for_automation_threads() {
    let caller = caller();
    let scope = thread_scope_for(&caller);
    let automation_thread = |thread_id: &str| SessionThreadRecord {
        scope: scope.clone(),
        thread_id: ThreadId::new(thread_id).expect("automation thread id"),
        created_by_actor_id: caller.user_id.as_str().to_string(),
        title: Some(format!("Automation run {thread_id}")),
        metadata_json: Some(automation_trigger_thread_metadata_json(
            "trigger-scheduled-summary",
        )),
        goal: None,
        created_at: None,
        updated_at: None,
    };
    let stalled_cursor = "cursor-stalled".to_string();
    let thread_service = Arc::new(ScriptedThreadService::list_pages(vec![
        ListThreadsForScopeResponse {
            threads: vec![automation_thread("thread-automation-stall-1")],
            next_cursor: Some(stalled_cursor.clone()),
        },
        ListThreadsForScopeResponse {
            threads: vec![automation_thread("thread-automation-stall-2")],
            next_cursor: Some(stalled_cursor.clone()),
        },
    ]));
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let response = tokio::time::timeout(
        Duration::from_secs(1),
        query_threads(
            &services,
            caller,
            ProductListThreadsRequest::default().set_limit(2),
        ),
    )
    .await
    .expect("list_threads should terminate when backend cursor stalls")
    .expect("list threads");

    assert!(
        response.threads.is_empty(),
        "automation trigger threads must stay hidden even when every fetched page is filtered",
    );
    assert_eq!(
        response.next_cursor, None,
        "stalled cursor must be cleared so callers do not keep replaying the same filtered page",
    );
    let list_requests = thread_service.list_requests();
    assert_eq!(
        list_requests.len(),
        2,
        "service should fetch the stalled page once and then break on the repeated cursor",
    );
    assert_eq!(list_requests[0].cursor, None);
    assert_eq!(list_requests[1].cursor.as_deref(), Some("cursor-stalled"));
}

#[tokio::test]
async fn list_threads_caps_filtered_pages_when_automation_threads_dominate() {
    let caller = caller();
    let scope = thread_scope_for(&caller);
    let automation_thread = |index: usize| SessionThreadRecord {
        scope: scope.clone(),
        thread_id: ThreadId::new(format!("thread-automation-budget-{index:02}"))
            .expect("automation thread id"),
        created_by_actor_id: caller.user_id.as_str().to_string(),
        title: Some(format!("Automation run {index}")),
        metadata_json: Some(automation_trigger_thread_metadata_json(
            "trigger-scheduled-summary",
        )),
        goal: None,
        created_at: None,
        updated_at: None,
    };
    let responses = (0..20)
        .map(|index| ListThreadsForScopeResponse {
            threads: vec![automation_thread(index)],
            next_cursor: Some(format!("cursor-{index:02}")),
        })
        .collect::<Vec<_>>();
    let thread_service = Arc::new(ScriptedThreadService::list_pages(responses));
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let response = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default().set_limit(1),
    )
    .await
    .expect("list threads");

    assert!(
        response.threads.is_empty(),
        "automation trigger threads must stay hidden when filter pages are exhausted",
    );
    assert_eq!(
        response.next_cursor, None,
        "filter page budget exhaustion must clear the cursor so callers do not keep scanning",
    );
    let list_requests = thread_service.list_requests();
    assert_eq!(
        list_requests.len(),
        20,
        "service must enforce a hard cap on filtered backend pages",
    );
    assert!(
        list_requests
            .iter()
            .all(|request| request.limit == Some(50)),
        "service should use a fixed candidate page size instead of shrinking toward one"
    );
}

#[tokio::test]
async fn list_threads_skips_hidden_automation_threads_when_filling_page() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let caller = caller();
    let automation_thread_id = ThreadId::new("thread-a-automation").expect("automation thread id");
    let first_visible_thread_id =
        ThreadId::new("thread-b-visible").expect("first visible thread id");
    let second_visible_thread_id =
        ThreadId::new("thread-c-visible").expect("second visible thread id");

    // Threads list newest-activity first, so create them oldest → newest:
    // second visible, then first visible, then the automation thread last.
    // That yields a candidate order of [automation, first, second], so the
    // service has to skip the leading hidden automation thread while filling
    // the first page — the behavior under test. Waiting past each stamp
    // keeps the `created_at` order strict regardless of clock resolution.
    let second = thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(second_visible_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("Second visible chat".to_string()),
            metadata_json: Some(json!({ "source": "webui" }).to_string()),
        })
        .await
        .expect("second visible thread");
    wait_until_after(second.updated_at.expect("activity stamp")).await;
    let first = thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(first_visible_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("First visible chat".to_string()),
            metadata_json: Some(json!({ "source": "webui" }).to_string()),
        })
        .await
        .expect("first visible thread");
    wait_until_after(first.updated_at.expect("activity stamp")).await;
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&caller),
            thread_id: Some(automation_thread_id.clone()),
            created_by_actor_id: caller.user_id.as_str().to_string(),
            title: Some("Automation run".to_string()),
            metadata_json: Some(automation_trigger_thread_metadata_json(
                "trigger-scheduled-summary",
            )),
        })
        .await
        .expect("automation thread");

    let first_page = query_threads(
        &services,
        caller.clone(),
        ProductListThreadsRequest::default().set_limit(1),
    )
    .await
    .expect("list first visible page");
    assert_eq!(
        first_page
            .threads
            .iter()
            .map(|thread| thread.thread_id.clone())
            .collect::<Vec<_>>(),
        vec![first_visible_thread_id],
    );
    assert_eq!(first_page.next_cursor.as_deref(), Some("thread-b-visible"));

    let second_page = query_threads(
        &services,
        caller,
        ProductListThreadsRequest::default()
            .set_limit(1)
            .set_cursor(first_page.next_cursor.expect("first visible page cursor")),
    )
    .await
    .expect("list second visible page");
    assert_eq!(
        second_page
            .threads
            .iter()
            .map(|thread| thread.thread_id.clone())
            .collect::<Vec<_>>(),
        vec![second_visible_thread_id],
    );
    assert_eq!(second_page.next_cursor, None);
}
