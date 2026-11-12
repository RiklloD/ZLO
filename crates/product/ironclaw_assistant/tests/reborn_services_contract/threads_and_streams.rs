//! Timeline and stream contracts, thread delete, and timeline paging.

use super::*;

struct BlockingSubmitCoordinator {
    submit_entered: AtomicBool,
    submit_released: AtomicBool,
    entered_submit: Notify,
    release_submit: Notify,
    run_id: TurnRunId,
}

impl BlockingSubmitCoordinator {
    fn new() -> Self {
        Self {
            submit_entered: AtomicBool::new(false),
            submit_released: AtomicBool::new(false),
            entered_submit: Notify::new(),
            release_submit: Notify::new(),
            run_id: TurnRunId::new(),
        }
    }

    async fn wait_for_submit(&self) {
        while !self.submit_entered.load(Ordering::Acquire) {
            self.entered_submit.notified().await;
        }
    }

    fn release_submit(&self) {
        self.submit_released.store(true, Ordering::Release);
        self.release_submit.notify_waiters();
    }
}

#[async_trait]
impl TurnCoordinator for BlockingSubmitCoordinator {
    async fn prepare_turn(&self, _scope: TurnScope) -> Result<TurnRunId, TurnError> {
        Ok(TurnRunId::new())
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.submit_entered.store(true, Ordering::Release);
        self.entered_submit.notify_waiters();
        while !self.submit_released.load(Ordering::Acquire) {
            self.release_submit.notified().await;
        }
        Ok(SubmitTurnResponse::Accepted {
            turn_id: TurnId::new(),
            run_id: self.run_id,
            status: TurnStatus::Queued,
            resolved_run_profile_id: RunProfileId::default_profile(),
            resolved_run_profile_version: RunProfileVersion::new(1),
            event_cursor: EventCursor(23),
            accepted_message_ref: request.accepted_message_ref,
        })
    }

    async fn resume_turn(
        &self,
        _request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        panic!("resume_turn is not used by delete submit serialization tests")
    }

    async fn retry_turn(
        &self,
        _request: ironclaw_turns::RetryTurnRequest,
    ) -> Result<ironclaw_turns::RetryTurnResponse, TurnError> {
        panic!("retry_turn is not used by delete submit serialization tests")
    }

    async fn cancel_run(&self, _request: CancelRunRequest) -> Result<CancelRunResponse, TurnError> {
        panic!("cancel_run is not used by delete submit serialization tests")
    }

    async fn get_run_state(&self, request: GetRunStateRequest) -> Result<TurnRunState, TurnError> {
        Ok(TurnRunState {
            scope: request.scope,
            actor: Some(turn_actor_for_user("user-alpha")),
            turn_id: TurnId::new(),
            run_id: request.run_id,
            status: TurnStatus::Queued,
            accepted_message_ref: AcceptedMessageRef::new("msg:blocked-submit").expect("valid ref"),
            resolved_run_profile_id: RunProfileId::default_profile(),
            resolved_run_profile_version: RunProfileVersion::new(1),
            output_contract: ironclaw_host_api::output::OutputContract::AssistantMessage,
            allow_steering: true,
            resolved_model_route: None,
            model_usage: None,
            execution_outcome: None,
            received_at: Utc::now(),
            checkpoint_id: None,
            gate_ref: None,
            blocked_activity_id: None,
            credential_requirements: Vec::new(),
            failure: None,
            event_cursor: EventCursor(29),
            product_context: None,
            resume_disposition: None,
        })
    }
}

struct SubscribingProjectionStream {
    event: ProductOutboundEnvelope,
    subscriptions: Mutex<Vec<ProjectionSubscriptionRequest>>,
    drain_count: Mutex<usize>,
}

impl SubscribingProjectionStream {
    fn new(event: ProductOutboundEnvelope) -> Self {
        Self {
            event,
            subscriptions: Mutex::new(Vec::new()),
            drain_count: Mutex::new(0),
        }
    }

    fn subscription_count(&self) -> usize {
        self.subscriptions.lock().expect("lock").len()
    }

    fn drain_count(&self) -> usize {
        *self.drain_count.lock().expect("lock")
    }
}

#[async_trait]
impl ProjectionStream for SubscribingProjectionStream {
    async fn drain(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError> {
        *self.drain_count.lock().expect("lock") += 1;
        Ok(Vec::new())
    }

    fn supports_subscription(&self) -> bool {
        true
    }

    async fn subscribe(
        &self,
        request: ProjectionSubscriptionRequest,
    ) -> Result<ProjectionStreamSubscription, ProductAdapterError> {
        self.subscriptions.lock().expect("lock").push(request);
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Ok(self.event.clone()))
            .await
            .expect("test subscription receiver remains open");
        Ok(ProjectionStreamSubscription::new(receiver))
    }
}

struct ControlledSubscribingProjectionStream {
    receiver: Mutex<Option<mpsc::Receiver<Result<ProductOutboundEnvelope, ProductAdapterError>>>>,
}

impl ControlledSubscribingProjectionStream {
    fn new() -> (
        Self,
        mpsc::Sender<Result<ProductOutboundEnvelope, ProductAdapterError>>,
    ) {
        let (sender, receiver) = mpsc::channel(2);
        (
            Self {
                receiver: Mutex::new(Some(receiver)),
            },
            sender,
        )
    }
}

#[async_trait]
impl ProjectionStream for ControlledSubscribingProjectionStream {
    async fn drain(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError> {
        Ok(Vec::new())
    }

    fn supports_subscription(&self) -> bool {
        true
    }

    async fn subscribe(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<ProjectionStreamSubscription, ProductAdapterError> {
        let receiver = self
            .receiver
            .lock()
            .expect("lock")
            .take()
            .expect("controlled stream subscribes once");
        Ok(ProjectionStreamSubscription::new(receiver))
    }
}

fn keep_alive_outbound(cursor: &str) -> ProductOutboundEnvelope {
    ProductOutboundEnvelope::new(
        ProductAdapterId::new("webui_v2").expect("adapter id"),
        AdapterInstallationId::new("install:alpha").expect("installation id"),
        ProductOutboundTarget::new(
            ReplyTargetBindingRef::new("reply:test").expect("reply target"),
            ExternalConversationRef::new(None, "conversation:test", None, None)
                .expect("conversation ref"),
            None,
        ),
        ProjectionCursor::new(cursor).expect("projection cursor"),
        ProductOutboundPayload::KeepAlive,
    )
}

/// Lighter-weight projection stream used by the timeline drain
/// regressions: counts calls without retaining the request shape. Kept
/// alongside `RecordingProjectionStream` because some sites only need
/// the count and the leaner stub keeps those tests focused.
#[derive(Default)]
struct SpyProjectionStream {
    drain_count: Mutex<usize>,
}

impl SpyProjectionStream {
    fn drain_count(&self) -> usize {
        *self.drain_count.lock().expect("lock")
    }
}

#[async_trait]
impl ProjectionStream for SpyProjectionStream {
    async fn drain(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError> {
        *self.drain_count.lock().expect("lock") += 1;
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn m2_service_timeline_contract_uses_fake_thread_port_with_authenticated_scope() {
    let web_caller = caller();
    let expected_tenant_id = web_caller.tenant_id.clone();
    let expected_agent_id = web_caller.agent_id.clone().expect("test caller has agent");
    let expected_project_id = web_caller.project_id.clone();
    let expected_user_id = web_caller.user_id.clone();
    let thread_service = Arc::new(ScriptedThreadService::history(fake_thread_history(
        &web_caller,
        "thread-alpha",
    )));
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let timeline = services
        .get_timeline(
            web_caller.clone(),
            RebornTimelineRequest::new("thread-alpha"),
        )
        .await
        .expect("timeline is served by fake M2 thread port");

    assert_eq!(timeline.thread.thread_id.as_str(), "thread-alpha");
    assert_eq!(timeline.messages.len(), 1);
    assert_eq!(
        timeline.messages[0].content.as_deref(),
        Some("timeline from fake M2 port")
    );

    let requests = thread_service.history_requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.thread_id.as_str(), "thread-alpha");
    assert_eq!(request.scope.tenant_id, expected_tenant_id);
    assert_eq!(request.scope.agent_id, expected_agent_id);
    assert_eq!(request.scope.project_id, expected_project_id);
    assert_eq!(request.scope.owner_user_id, Some(expected_user_id));
}

#[tokio::test]
async fn m2_service_stream_contract_uses_fake_projection_port_with_authenticated_scope() {
    let web_caller = caller();
    let event_stream = Arc::new(RecordingProjectionStream::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_event_stream(event_stream.clone());
    create_thread_for(&services, web_caller.clone(), "thread-alpha").await;
    let after_cursor = ProjectionCursor::new("cursor-alpha").expect("cursor");

    let response = services
        .stream_events(
            web_caller.clone(),
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: Some(after_cursor.clone()),
            },
        )
        .await
        .expect("stream is served by fake M2 projection port");

    assert!(response.events.is_empty());
    let requests = event_stream.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.actor.user_id, web_caller.user_id);
    assert_eq!(request.scope.tenant_id, web_caller.tenant_id);
    assert_eq!(request.scope.agent_id, web_caller.agent_id);
    assert_eq!(request.scope.project_id, web_caller.project_id);
    assert_eq!(request.scope.thread_id.as_str(), "thread-alpha");
    assert_eq!(
        request.after_cursor.as_ref().map(ProjectionCursor::as_str),
        Some(after_cursor.as_str())
    );
}

#[tokio::test]
async fn product_surface_subscription_stays_open_instead_of_polling_drain() {
    let web_caller = caller();
    let expected = keep_alive_outbound("cursor-live");
    let event_stream = Arc::new(SubscribingProjectionStream::new(expected.clone()));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_event_stream(event_stream.clone());
    create_thread_for(&services, web_caller.clone(), "thread-alpha").await;

    let mut response = ProductSurface::stream_events(
        &services,
        web_caller.clone(),
        ProductSurfaceStreamRequest {
            stream_id: Some("thread-alpha".to_string()),
            after_cursor: None,
        },
    )
    .await
    .expect("subscription opens");
    let subscription = response
        .subscription
        .take()
        .expect("stream response carries its live continuation");

    assert_eq!(
        response.events,
        vec![serde_json::to_value(expected).expect("outbound event encodes")]
    );
    assert_eq!(event_stream.subscription_count(), 1);
    assert_eq!(
        event_stream.drain_count(),
        0,
        "a supported live subscription must not fall back to an empty poll"
    );
    drop(subscription);
    let drained = services
        .stream_events(
            web_caller,
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect("one-shot compatibility path drains");

    assert!(drained.events.is_empty());
    assert_eq!(event_stream.subscription_count(), 1);
    assert_eq!(
        event_stream.drain_count(),
        1,
        "the compatibility method retains its one-shot drain contract"
    );
}

#[tokio::test(start_paused = true)]
async fn product_surface_subscription_bounds_a_silent_first_event() {
    let web_caller = caller();
    let (event_stream, _sender) = ControlledSubscribingProjectionStream::new();
    let services = Arc::new(
        session_services(
            Arc::new(InMemorySessionThreadService::default()),
            Arc::new(FakeTurnCoordinator::default()),
        )
        .with_event_stream(Arc::new(event_stream)),
    );
    create_thread_for(&services, web_caller.clone(), "thread-alpha").await;

    let task = tokio::spawn(async move {
        ProductSurface::stream_events(
            services.as_ref(),
            web_caller,
            ProductSurfaceStreamRequest {
                stream_id: Some("thread-alpha".to_string()),
                after_cursor: None,
            },
        )
        .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;

    assert!(
        task.is_finished(),
        "a silent subscription must not suspend one-shot callers indefinitely"
    );
    let response = task
        .await
        .expect("stream task joins")
        .expect("silent subscription returns an empty response");
    assert!(response.events.is_empty());
    assert!(response.subscription.is_none());
}

#[tokio::test(start_paused = true)]
async fn product_surface_subscription_revalidates_visibility_without_blocking_events() {
    let caller = caller();
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let trigger_thread_id = setup_trigger_thread(
        &thread_service,
        &caller,
        "thread-trigger-subscription-revalidation",
    )
    .await;
    let automation_service = Arc::new(RevocableAutomationService::new(
        trigger_thread_id.clone(),
        &caller,
    ));
    let (event_stream, sender) = ControlledSubscribingProjectionStream::new();
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_automation_product_service(automation_service.clone())
        .with_event_stream(Arc::new(event_stream));

    sender
        .send(Ok(keep_alive_outbound("cursor-before-revocation")))
        .await
        .expect("subscription receiver remains open");
    let mut response = ProductSurface::stream_events(
        &services,
        caller,
        ProductSurfaceStreamRequest {
            stream_id: Some(trigger_thread_id.to_string()),
            after_cursor: None,
        },
    )
    .await
    .expect("visible automation thread subscribes");
    assert_eq!(response.events.len(), 1, "first event is authorized");
    let subscription = response
        .subscription
        .take()
        .expect("stream response carries its live continuation");

    tokio::task::yield_now().await;
    automation_service.revoke();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let error = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("revalidation must terminate the subscription")
        .expect("revocation is surfaced")
        .expect_err("revoked visibility must stop the subscription");

    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);
}

#[tokio::test]
async fn get_timeline_rejects_cross_user_access() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .get_timeline(
            caller_for_user("user-beta"),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect_err("cross-user timeline read must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
}

#[tokio::test]
async fn delete_thread_removes_owned_thread() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    create_thread_for(&services, caller(), "thread-alpha").await;

    let response = services
        .delete_thread(
            caller(),
            RebornDeleteThreadRequest {
                thread_id: "thread-alpha".to_string(),
            },
        )
        .await
        .expect("delete owned thread");

    assert_eq!(response.thread_id.as_str(), "thread-alpha");
    assert!(response.deleted);

    let err = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect_err("deleted thread must no longer be readable");
    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
}

#[tokio::test]
async fn delete_thread_rejects_cross_user_access_without_deleting_owner_thread() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let alice = caller();
    create_thread_for(&services, alice.clone(), "thread-alpha").await;

    let err = services
        .delete_thread(
            caller_for_user("user-beta"),
            RebornDeleteThreadRequest {
                thread_id: "thread-alpha".to_string(),
            },
        )
        .await
        .expect_err("cross-user delete must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);

    services
        .get_timeline(
            alice,
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("owner thread must remain after rejected cross-user delete");
}

#[tokio::test]
async fn delete_thread_rejects_thread_with_active_run() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;
    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-before-delete",
                "thread_id": "thread-alpha",
                "content": "keep this run alive"
            }))
            .expect("request"),
        )
        .await
        .expect("submit succeeds");

    let err = services
        .delete_thread(
            caller(),
            RebornDeleteThreadRequest {
                thread_id: "thread-alpha".to_string(),
            },
        )
        .await
        .expect_err("active thread delete must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Busy);
    assert_eq!(err.status_code, 409);
    assert_eq!(coordinator.run_state_request_count(), 1);
    services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("rejected delete must leave thread readable");
}

#[tokio::test]
async fn delete_thread_waits_for_in_flight_submit_before_active_run_check() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(BlockingSubmitCoordinator::new());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let submit_services = services.clone();
    let submit_handle = tokio::spawn(async move {
        submit_services
            .submit_turn(
                caller(),
                session_submit_request(json!({
                    "client_action_id": "send-racing-delete",
                    "thread_id": "thread-alpha",
                    "content": "submit while delete races"
                }))
                .expect("request"),
            )
            .await
    });
    coordinator.wait_for_submit().await;

    let delete_services = services.clone();
    let (delete_done_tx, mut delete_done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = delete_services
            .delete_thread(
                caller(),
                RebornDeleteThreadRequest {
                    thread_id: "thread-alpha".to_string(),
                },
            )
            .await;
        let _ = delete_done_tx.send(result);
    });

    let early_delete = tokio::time::timeout(Duration::from_millis(25), &mut delete_done_rx).await;
    assert!(
        early_delete.is_err(),
        "delete must wait behind the in-flight submit operation"
    );

    coordinator.release_submit();
    submit_handle
        .await
        .expect("submit task joins")
        .expect("submit succeeds");

    let err = delete_done_rx
        .await
        .expect("delete result")
        .expect_err("delete sees submitted active run after waiting");
    assert_eq!(err.code, ProductSurfaceErrorCode::Conflict);
    assert_eq!(err.kind, ProductSurfaceErrorKind::Busy);
    services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("rejected delete must leave thread readable");
}

#[tokio::test]
async fn stream_events_rejects_cross_user_access_before_draining_stream() {
    let stream = Arc::new(SpyProjectionStream::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_event_stream(stream.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let err = services
        .stream_events(
            caller_for_user("user-beta"),
            RebornStreamEventsRequest {
                thread_id: "thread-alpha".to_string(),
                after_cursor: None,
            },
        )
        .await
        .expect_err("cross-user stream read must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
    assert_eq!(stream.drain_count(), 0);
}

/// Seed `count` user messages into the in-memory thread service so the
/// timeline pagination regressions below have real, ordered message
/// rows to slice against.
async fn seed_thread_messages(
    threads: &InMemorySessionThreadService,
    caller: &ProductSurfaceCaller,
    thread_id: &str,
    count: usize,
) {
    let scope = thread_scope_for(caller);
    let parsed_thread_id = ironclaw_host_api::ids::ThreadId::new(thread_id).expect("thread id");
    for index in 0..count {
        threads
            .accept_inbound_message(AcceptInboundMessageRequest {
                scope: scope.clone(),
                thread_id: parsed_thread_id.clone(),
                actor_id: caller.user_id.as_str().to_string(),
                source_binding_id: None,
                reply_target_binding_id: None,
                external_event_id: None,
                content: MessageContent::text(format!("msg-{index}")),
            })
            .await
            .expect("seed message");
    }
}

// Regression for the timeline-pagination review (Medium). Without
// per-response caps a thread with hundreds of messages would force a
// multi-megabyte JSON allocation + serialize per call, since the route
// rate limit only bounds open frequency. `get_timeline` must (a) clamp
// `limit` to a hard ceiling so callers cannot bypass the cap, (b)
// return at most `limit` messages per page, and (c) surface a
// `next_cursor` the browser can echo back to load the page preceding
// it. When the caller has reached the start of the thread, `next_cursor`
// must be `None` so the browser stops asking for more.
#[tokio::test]
async fn get_timeline_pages_messages_with_cursor() {
    let threads = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads.clone(), coordinator);
    let alice = caller();
    setup_owned_thread(&services, alice.clone(), "thread-paginate").await;
    seed_thread_messages(&threads, &alice, "thread-paginate", 25).await;

    let first = services
        .get_timeline(
            alice.clone(),
            RebornTimelineRequest::new("thread-paginate".to_string()).set_limit(10),
        )
        .await
        .expect("first page");
    assert_eq!(
        first.messages.len(),
        10,
        "first page must honor the requested limit"
    );
    // The page is the newest 10 messages — the page boundary sits at
    // the message just *before* the earliest one in this page.
    let earliest_in_first_page = first
        .messages
        .first()
        .expect("first message on page")
        .sequence;
    let next_cursor = first
        .next_cursor
        .as_deref()
        .expect("next_cursor must surface when more messages remain");

    let second = services
        .get_timeline(
            alice.clone(),
            RebornTimelineRequest {
                thread_id: "thread-paginate".to_string(),
                limit: Some(10),
                cursor: Some(next_cursor.to_string()),
            },
        )
        .await
        .expect("second page");
    assert_eq!(second.messages.len(), 10, "second page is fully populated");
    // Every message in page two must be older than every message in
    // page one.
    assert!(
        second
            .messages
            .last()
            .map(|message| message.sequence < earliest_in_first_page)
            .unwrap_or(false),
        "second page must contain messages strictly older than the first"
    );
    assert!(
        second.next_cursor.is_some(),
        "more pages remain after the second page"
    );

    let third = services
        .get_timeline(
            alice,
            RebornTimelineRequest {
                thread_id: "thread-paginate".to_string(),
                limit: Some(10),
                cursor: second.next_cursor.clone(),
            },
        )
        .await
        .expect("third page");
    // Five messages remain (25 - 10 - 10) and the caller has reached
    // the start of the thread, so next_cursor must be None.
    assert_eq!(third.messages.len(), 5);
    assert!(
        third.next_cursor.is_none(),
        "next_cursor must be None once all older messages are exhausted"
    );
}

// Regression: `limit` must be clamped to the service's hard ceiling so a
// caller cannot widen the response by passing a huge value. Without the
// clamp, the per-route rate limit would be the only thing bounding
// per-request response size.
#[tokio::test]
async fn get_timeline_clamps_oversize_limit_to_hard_ceiling() {
    let threads = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads.clone(), coordinator);
    let alice = caller();
    setup_owned_thread(&services, alice.clone(), "thread-cap").await;
    // Seed more than the hard ceiling so the clamp is observable.
    seed_thread_messages(&threads, &alice, "thread-cap", 250).await;

    let response = services
        .get_timeline(
            alice,
            RebornTimelineRequest::new("thread-cap".to_string()).set_limit(u32::MAX),
        )
        .await
        .expect("clamped timeline");
    assert!(
        response.messages.len() <= 200,
        "limit must be clamped to TIMELINE_MAX_PAGE_SIZE (200); got {}",
        response.messages.len()
    );
    assert!(
        response.next_cursor.is_some(),
        "next_cursor must surface because the underlying thread has more messages than the cap"
    );
}

// Regression: a malformed cursor must be rejected at the wire boundary
// with an InvalidValue validation error rather than silently treated as
// "no cursor". Without this guard, a caller could send garbage in
// `cursor=...` and quietly load page 1 instead of the intended page.
#[tokio::test]
async fn get_timeline_rejects_malformed_cursor() {
    let threads = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(threads, coordinator);
    let alice = caller();
    setup_owned_thread(&services, alice.clone(), "thread-bad-cursor").await;

    let err = services
        .get_timeline(
            alice,
            RebornTimelineRequest {
                thread_id: "thread-bad-cursor".to_string(),
                limit: None,
                cursor: Some("not-a-valid-cursor".to_string()),
            },
        )
        .await
        .expect_err("malformed cursor must be rejected");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.field.as_deref(), Some("cursor"));
    assert_eq!(
        err.validation_code,
        Some(ProductSurfaceValidationCode::InvalidValue)
    );
}

#[test]
fn service_source_avoids_forbidden_runtime_dependencies() {
    // The service is split into `src/reborn_services/` submodules, so the root
    // alone no longer holds it. `approval_settings.rs` is the one focused seam
    // that imports `ironclaw_approvals` so the rest of the service does not
    // (see that module's doc comment); it is exempt from that one token only
    // and is checked for every other forbidden dependency.
    const APPROVALS_SEAM: &str = "approval_settings.rs";
    let mut sources = vec![std::path::PathBuf::from("src/reborn_services.rs")];
    let mut pending = vec![std::path::PathBuf::from("src/reborn_services")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("service source dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push(path);
            }
        }
    }
    assert!(
        sources.len() > 1,
        "no reborn_services submodule was scanned"
    );
    assert!(
        sources
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == APPROVALS_SEAM)),
        "the {APPROVALS_SEAM} seam was not found; update this gate if it moved"
    );
    for path in &sources {
        let source = std::fs::read_to_string(path).expect("service source");
        let is_approvals_seam = path.file_name().is_some_and(|name| name == APPROVALS_SEAM);
        for forbidden in [
            "CapabilityHost",
            "ironclaw_capabilities",
            "ironclaw_host_runtime",
            "ironclaw_approvals",
            "ironclaw_storage",
            "RuntimeLane",
            "pub fn thread_service",
            "pub fn turn_coordinator",
        ] {
            if is_approvals_seam && forbidden == "ironclaw_approvals" {
                continue;
            }
            assert!(
                !source.contains(forbidden),
                "RebornServices service must not expose route handlers to {forbidden} ({})",
                path.display()
            );
        }
    }

    let _ = Utc::now();
}
