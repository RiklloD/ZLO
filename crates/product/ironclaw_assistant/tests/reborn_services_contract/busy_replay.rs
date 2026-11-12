//! Busy-rejection notices, rejected/deferred replays, and WebUI queued replay.

use super::*;

// ---------------------------------------------------------------------------
// Notice-text mapping: rejected_busy_notice maps TurnStatus to the right copy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejected_busy_notice_blocked_approval_contains_approval_copy() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id: TurnRunId::new(),
            status: TurnStatus::BlockedApproval,
            event_cursor: EventCursor(5),
        }),
    ));
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-notice").await;

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-notice-approval",
                "thread_id": "thread-notice",
                "content": "hello"
            }))
            .expect("request"),
        )
        .await
        .expect("busy submit succeeds with RejectedBusy");

    match response {
        RebornSubmitTurnResponse::RejectedBusy {
            status: Some(status),
            notice,
            ..
        } => {
            assert_eq!(status, TurnStatus::BlockedApproval);
            assert_eq!(
                notice,
                "An approval gate is open on this thread — resolve it (approve or deny) before continuing, then resend your message."
            );
        }
        other => panic!("expected RejectedBusy, got {other:?}"),
    }
}

#[tokio::test]
async fn rejected_busy_notice_blocked_auth_contains_auth_copy() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id: TurnRunId::new(),
            status: TurnStatus::BlockedAuth,
            event_cursor: EventCursor(5),
        }),
    ));
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-notice").await;

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-notice-auth",
                "thread_id": "thread-notice",
                "content": "hello"
            }))
            .expect("request"),
        )
        .await
        .expect("busy submit succeeds with RejectedBusy");

    match response {
        RebornSubmitTurnResponse::RejectedBusy {
            status: Some(status),
            notice,
            ..
        } => {
            assert_eq!(status, TurnStatus::BlockedAuth);
            assert_eq!(
                notice,
                "An authentication gate is open on this thread — complete authentication before continuing, then resend your message."
            );
        }
        other => panic!("expected RejectedBusy, got {other:?}"),
    }
}

#[tokio::test]
async fn rejected_busy_notice_generic_status_contains_generic_copy() {
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id: TurnRunId::new(),
            status: TurnStatus::Running,
            event_cursor: EventCursor(5),
        }),
    ));
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-notice").await;

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-notice-generic",
                "thread_id": "thread-notice",
                "content": "hello"
            }))
            .expect("request"),
        )
        .await
        .expect("busy submit succeeds with RejectedBusy");

    match response {
        RebornSubmitTurnResponse::RejectedBusy {
            status: Some(status),
            notice,
            ..
        } => {
            assert_eq!(status, TurnStatus::Running);
            assert_eq!(
                notice,
                "Ironclaw is still working on a previous message — resend yours once the current task finishes."
            );
        }
        other => panic!("expected RejectedBusy, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Replay regression: a replayed RejectedBusy must return RejectedBusy again,
// never submit a new run (contract from PR #4838)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replayed_rejected_busy_returns_rejected_busy_without_new_submission() {
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    // ScriptedThreadService pre-seeds the message as RejectedBusy — simulates
    // the client retrying after the original rejection response was lost.
    let services = session_services(
        Arc::new(ScriptedThreadService::rejected_busy_replay()),
        coordinator.clone(),
    );

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-replay-rejected-busy",
                "thread_id": "thread-alpha",
                "content": "hello from webui"
            }))
            .expect("request"),
        )
        .await
        .expect("replayed RejectedBusy must succeed (not error)");

    assert!(
        matches!(response, RebornSubmitTurnResponse::RejectedBusy { .. }),
        "replay of RejectedBusy must return RejectedBusy, got {response:?}"
    );
    assert_eq!(
        coordinator.submission_count(),
        0,
        "a replayed RejectedBusy must not produce a new turn submission"
    );
}

// ---------------------------------------------------------------------------
// Option<> run-metadata contract: replay path yields None; fresh path yields Some
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replayed_rejected_busy_returns_none_run_metadata() {
    // Replay: the original blocking run is gone — run metadata must be None,
    // not a fabricated run-id or status that the client cannot query.
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let services = session_services(
        Arc::new(ScriptedThreadService::rejected_busy_replay()),
        coordinator.clone(),
    );

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-replay-none-metadata",
                "thread_id": "thread-alpha",
                "content": "replay with none metadata"
            }))
            .expect("request"),
        )
        .await
        .expect("replayed RejectedBusy must succeed");

    match response {
        RebornSubmitTurnResponse::RejectedBusy {
            active_run_id,
            status,
            event_cursor,
            notice,
            ..
        } => {
            assert!(
                active_run_id.is_none(),
                "replayed RejectedBusy must not fabricate active_run_id, got {active_run_id:?}"
            );
            assert!(
                status.is_none(),
                "replayed RejectedBusy must not fabricate status, got {status:?}"
            );
            assert!(
                event_cursor.is_none(),
                "replayed RejectedBusy must not fabricate event_cursor, got {event_cursor:?}"
            );
            assert!(
                !notice.is_empty(),
                "replayed RejectedBusy must carry a notice"
            );
        }
        other => panic!("expected RejectedBusy, got {other:?}"),
    }
    assert_eq!(
        coordinator.submission_count(),
        0,
        "replay must not produce a new turn submission"
    );
}

#[tokio::test]
async fn fresh_rejected_busy_returns_some_run_metadata() {
    // Fresh ThreadBusy: the blocking run is live — run metadata must be Some
    // with the real values so the client can poll the existing run.
    let active_run_id = TurnRunId::new();
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id,
            status: TurnStatus::Running,
            event_cursor: EventCursor(7),
        }),
    ));
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(threads, coordinator.clone());
    create_thread_for(&services, caller(), "thread-busy-fresh").await;

    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-fresh-busy-metadata",
                "thread_id": "thread-busy-fresh",
                "content": "hello busy"
            }))
            .expect("request"),
        )
        .await
        .expect("fresh RejectedBusy must succeed");

    match response {
        RebornSubmitTurnResponse::RejectedBusy {
            active_run_id: returned_run_id,
            status: returned_status,
            event_cursor: returned_cursor,
            notice,
            ..
        } => {
            assert_eq!(
                returned_run_id,
                Some(active_run_id),
                "fresh RejectedBusy must carry the real blocking run id"
            );
            assert_eq!(
                returned_status,
                Some(TurnStatus::Running),
                "fresh RejectedBusy must carry the real blocking run status"
            );
            assert_eq!(
                returned_cursor,
                Some(EventCursor(7)),
                "fresh RejectedBusy must carry the real event cursor"
            );
            assert!(!notice.is_empty(), "fresh RejectedBusy must carry a notice");
        }
        other => panic!("expected RejectedBusy, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Mark-failure reconcile path: mark_message_rejected_busy errors → replay
// confirms RejectedBusy → no error surfaces, RejectedBusy returned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejected_busy_mark_failure_reconciles_via_replay_and_returns_rejected_busy() {
    // Arrange: coordinator returns ThreadBusy so the busy path fires; the
    // scripted thread service makes mark_message_rejected_busy fail and then
    // supplies a RejectedBusy replay on the reconcile probe so
    // reconcile_terminal_duplicate settles the race without propagating the error.
    let active_run_id = TurnRunId::new();
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id,
            status: TurnStatus::Running,
            event_cursor: EventCursor(3),
        }),
    ));
    let services = session_services(
        Arc::new(ScriptedThreadService::rejected_busy_mark_fails()),
        coordinator,
    );

    // Act: submit a fresh turn against thread-alpha (which the scripted service
    // owns); coordinator fires ThreadBusy, mark fails, reconcile replays.
    let response = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-mark-fail-reconcile",
                "thread_id": "thread-alpha",
                "content": "hello mark-fail"
            }))
            .expect("request"),
        )
        .await
        .expect("mark-failure reconcile must succeed (not error)");

    // Assert: the mark error must NOT propagate to the caller — reconcile_terminal_duplicate
    // replays the accepted message, sees RejectedBusy, and returns Ok(()).
    // The response is built from the original ThreadBusy metadata (active_run_id,
    // status, event_cursor), proving the full path ran without dropping state.
    match response {
        RebornSubmitTurnResponse::RejectedBusy {
            active_run_id: returned_run_id,
            status: returned_status,
            event_cursor: returned_cursor,
            notice,
            ..
        } => {
            assert_eq!(
                returned_run_id,
                Some(active_run_id),
                "mark-failure reconcile must carry the real blocking run id from ThreadBusy"
            );
            assert_eq!(
                returned_status,
                Some(TurnStatus::Running),
                "mark-failure reconcile must carry the real blocking run status"
            );
            assert_eq!(
                returned_cursor,
                Some(EventCursor(3)),
                "mark-failure reconcile must carry the real event cursor"
            );
            assert!(!notice.is_empty(), "RejectedBusy must carry a notice");
        }
        other => {
            panic!("mark-failure reconcile must return RejectedBusy (not error), got {other:?}")
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy DeferredBusy mark-failure reconcile path: mark_message_rejected_busy errors
// → replay returns legacy DeferredBusy (non-terminal) → predicate does NOT match
// → original mark error surfaces as Unavailable, not a false-terminal RejectedBusy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_deferred_busy_mark_failure_surfaces_error_not_false_terminal() {
    // Arrange: coordinator returns ThreadBusy so the busy path fires; the
    // scripted thread service makes mark_message_rejected_busy fail and then
    // supplies a legacy DeferredBusy replay on the reconcile probe.
    // DeferredBusy is non-terminal — reconcile_terminal_duplicate must NOT
    // accept it as settled.  The predicate now matches only RejectedBusy, so
    // the `_ =>` arm propagates the original mark failure as an error.
    let active_run_id = TurnRunId::new();
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id,
            status: TurnStatus::Running,
            event_cursor: EventCursor(3),
        }),
    ));
    let services = session_services(
        Arc::new(ScriptedThreadService::deferred_busy_mark_fails()),
        coordinator,
    );

    // Act: submit a fresh turn against thread-alpha; coordinator fires ThreadBusy,
    // mark_message_rejected_busy fails, reconcile sees legacy DeferredBusy which
    // no longer matches → the original mark error must propagate.
    let error = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-deferred-busy-mark-fail-reconcile",
                "thread_id": "thread-alpha",
                "content": "hello deferred-busy mark-fail"
            }))
            .expect("request"),
        )
        .await
        .expect_err(
            "legacy DeferredBusy reconcile must surface the mark failure as an error, \
             not silently return a false-terminal RejectedBusy",
        );

    // Assert: SessionThreadError::Backend maps to service_unavailable(true) —
    // code=Unavailable, status_code=503, retryable=true.
    assert_eq!(
        error.code,
        ProductSurfaceErrorCode::Unavailable,
        "DeferredBusy reconcile miss must surface the backend mark failure (Unavailable), got {error:?}",
    );
    assert_eq!(
        error.status_code, 503,
        "DeferredBusy reconcile miss must return 503, got {error:?}",
    );
    assert!(
        error.retryable,
        "backend mark failure is retryable, got {error:?}",
    );
}

// ---------------------------------------------------------------------------
// Queued-replay crash-orphan recovery (WebUI path): a Queued row whose queue
// entry was lost (crash between the status write and the enqueue) must be
// idempotently re-enqueued on replay instead of treating Queued as proof.
// ---------------------------------------------------------------------------

struct VanishingWebUiEnqueue;

#[async_trait]
impl ironclaw_loop_host::HostInputEnqueuePort for VanishingWebUiEnqueue {
    async fn enqueue_queued_message(
        &self,
        request: ironclaw_loop_host::EnqueueQueuedMessageRequest,
    ) -> Result<ironclaw_loop_host::HostInputEnvelope, ironclaw_loop_host::HostInputQueueError>
    {
        Ok(ironclaw_loop_host::HostInputEnvelope {
            input: request.input,
            cursor: ironclaw_loop_contracts::LoopInputCursorToken::new(
                "input-cursor:1".to_string(),
            )
            .expect("cursor"),
            ack_token: ironclaw_loop_contracts::LoopInputAckToken::new("input-ack:1".to_string())
                .expect("ack token"),
        })
    }
}

#[tokio::test]
async fn webui_queued_replay_re_enqueues_crash_orphaned_message() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let active_run_id = TurnRunId::new();
    let coordinator = Arc::new(FakeTurnCoordinator::with_submit_error(
        TurnError::ThreadBusy(ironclaw_turns::ThreadBusy {
            active_run_id,
            status: TurnStatus::Running,
            event_cursor: EventCursor(3),
        }),
    ));
    // Phase 1 — the "crashed" process: the enqueue vanishes, leaving the row
    // Queued with no backing queue entry.
    let crashed = session_services(
        thread_service.clone() as Arc<dyn SessionThreadService>,
        coordinator.clone(),
    )
    .with_input_enqueue(Arc::new(VanishingWebUiEnqueue));
    setup_owned_thread(&crashed, caller(), "thread-orphan-webui").await;
    let request = || {
        session_submit_request(json!({
            "client_action_id": "send-orphan-webui",
            "thread_id": "thread-orphan-webui",
            "content": "orphaned webui steering"
        }))
        .expect("request")
    };
    let response = crashed
        .submit_turn(caller(), request())
        .await
        .expect("busy submit succeeds");
    assert!(
        matches!(response, RebornSubmitTurnResponse::DeferredBusy { .. }),
        "expected DeferredBusy, got {response:?}"
    );

    // Phase 2 — the restarted process with the REAL queue: the same
    // client_action_id triggers the WebUI replay, which must re-enqueue.
    let real_queue = Arc::new(ironclaw_loop_host::InMemoryHostInputQueue::new(
        thread_service.clone() as Arc<dyn SessionThreadService>,
    ));
    let restarted = session_services(
        thread_service.clone() as Arc<dyn SessionThreadService>,
        coordinator,
    )
    .with_input_enqueue(real_queue.clone() as Arc<dyn ironclaw_loop_host::HostInputEnqueuePort>);
    let replayed = restarted
        .submit_turn(caller(), request())
        .await
        .expect("replay succeeds");
    assert!(
        matches!(replayed, RebornSubmitTurnResponse::DeferredBusy { .. }),
        "replay stays DeferredBusy, got {replayed:?}"
    );
    use ironclaw_loop_host::HostInputQueue as _;
    let batch = real_queue
        .next_after(
            active_run_id,
            ironclaw_loop_contracts::LoopInputCursorToken::origin(),
            8,
        )
        .await
        .expect("poll after replay");
    assert_eq!(
        batch.inputs.len(),
        1,
        "the WebUI queued replay must re-enqueue the orphaned steering input"
    );
}
