//! Shared scripted session-thread doubles.

use crate::*;

/// Stub thread service whose `list_thread_history` always returns
/// `ThreadScopeMismatch`. Used to lock in the contract that ownership probes
/// remap that variant to NotFound, since the current backends happen to return
/// `UnknownThread` for the same condition. All other methods panic — none of
/// the cancel_run / resolve_gate paths under test should reach them.
pub(crate) struct ScopeMismatchThreadStub;

#[async_trait]
impl SessionThreadService for ScopeMismatchThreadStub {
    async fn list_thread_history(
        &self,
        request: ThreadHistoryRequest,
    ) -> Result<ThreadHistory, SessionThreadError> {
        Err(SessionThreadError::ThreadScopeMismatch {
            thread_id: request.thread_id,
        })
    }

    async fn ensure_thread(
        &self,
        _request: EnsureThreadRequest,
    ) -> Result<SessionThreadRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::ensure_thread should not be reached")
    }

    async fn accept_inbound_message(
        &self,
        _request: AcceptInboundMessageRequest,
    ) -> Result<AcceptedInboundMessage, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::accept_inbound_message should not be reached")
    }

    async fn replay_accepted_inbound_message(
        &self,
        _request: ReplayAcceptedInboundMessageRequest,
    ) -> Result<Option<AcceptedInboundMessageReplay>, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::replay_accepted_inbound_message should not be reached")
    }

    async fn mark_message_submitted(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
        _turn_id: String,
        _turn_run_id: String,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::mark_message_submitted should not be reached")
    }

    async fn mark_message_rejected_busy(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::mark_message_rejected_busy should not be reached")
    }

    async fn append_assistant_draft(
        &self,
        _request: AppendAssistantDraftRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::append_assistant_draft should not be reached")
    }

    async fn append_tool_result_reference(
        &self,
        _request: AppendToolResultReferenceRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::append_tool_result_reference should not be reached")
    }

    async fn append_capability_display_preview(
        &self,
        _request: AppendCapabilityDisplayPreviewRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::append_capability_display_preview should not be reached")
    }

    async fn update_tool_result_reference(
        &self,
        _request: UpdateToolResultReferenceRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::update_tool_result_reference should not be reached")
    }

    async fn update_assistant_draft(
        &self,
        _request: UpdateAssistantDraftRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::update_assistant_draft should not be reached")
    }

    async fn finalize_assistant_message(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
        _content: MessageContent,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::finalize_assistant_message should not be reached")
    }

    async fn redact_message(
        &self,
        _request: RedactMessageRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::redact_message should not be reached")
    }

    async fn load_context_window(
        &self,
        _request: LoadContextWindowRequest,
    ) -> Result<ContextWindow, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::load_context_window should not be reached")
    }

    async fn load_context_messages(
        &self,
        _request: LoadContextMessagesRequest,
    ) -> Result<ContextMessages, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::load_context_messages should not be reached")
    }

    async fn create_summary_artifact(
        &self,
        _request: CreateSummaryArtifactRequest,
    ) -> Result<SummaryArtifact, SessionThreadError> {
        panic!("ScopeMismatchThreadStub::create_summary_artifact should not be reached")
    }
}

pub(crate) enum BoundedReadOutcome {
    Complete,
    BackendError,
    LimitExceeded,
}

pub(crate) enum ScriptedThreadBehavior {
    BackendHistory,
    History(Box<ThreadHistory>),
    ThreadArtifact {
        history: Box<ThreadHistory>,
        snapshot: Box<BoundedThreadMessageSnapshot>,
        bounded_read_outcome: BoundedReadOutcome,
    },
    ListPages,
    SubmittedReplay {
        turn_run_id: Option<String>,
    },
    RejectedBusyReplay,
    /// `mark_message_rejected_busy` fails; reconcile path replays the accepted
    /// message as RejectedBusy so no error surfaces to the caller.
    RejectedBusyMarkFails {
        /// Message id assigned by `accept_inbound_message`, shared so that
        /// `reconcile_terminal_duplicate` can match it against the handoff.
        message_id: ThreadMessageId,
    },
    /// `mark_message_rejected_busy` fails; reconcile path replays the accepted
    /// message as legacy DeferredBusy.  Unlike `RejectedBusyMarkFails`,
    /// `DeferredBusy` is non-terminal: `reconcile_terminal_duplicate` accepts
    /// only `RejectedBusy` as settled, so this replay does NOT satisfy
    /// reconciliation.  The original mark failure surfaces as a retryable error
    /// (Unavailable / 503) rather than a false-terminal RejectedBusy.
    DeferredBusyMarkFails {
        /// Message id assigned by `accept_inbound_message`, shared so that
        /// `reconcile_terminal_duplicate` can match it against the handoff.
        message_id: ThreadMessageId,
    },
}

pub(crate) struct ScriptedThreadService {
    pub(crate) behavior: ScriptedThreadBehavior,
    pub(crate) history_requests: Mutex<Vec<ThreadHistoryRequest>>,
    pub(crate) bounded_requests: Mutex<Vec<BoundedThreadMessagesRequest>>,
    pub(crate) list_requests: Mutex<Vec<ListThreadsForScopeRequest>>,
    pub(crate) list_responses: Mutex<Vec<ListThreadsForScopeResponse>>,
    /// Tracks `replay_accepted_inbound_message` call count; used by
    /// `RejectedBusyMarkFails` (and `DeferredBusyMarkFails`) to return `None`
    /// on the first two calls (idempotency probes) and `Some(…)` on the third
    /// call (reconcile probe) onward.
    pub(crate) replay_call_count: Mutex<usize>,
}

impl ScriptedThreadService {
    pub(crate) fn backend_history() -> Self {
        Self {
            behavior: ScriptedThreadBehavior::BackendHistory,
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn history(history: ThreadHistory) -> Self {
        Self {
            behavior: ScriptedThreadBehavior::History(Box::new(history)),
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn thread_artifact(
        history: ThreadHistory,
        snapshot: BoundedThreadMessageSnapshot,
    ) -> Self {
        Self {
            behavior: ScriptedThreadBehavior::ThreadArtifact {
                history: Box::new(history),
                snapshot: Box::new(snapshot),
                bounded_read_outcome: BoundedReadOutcome::Complete,
            },
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn thread_artifact_backend_error(
        history: ThreadHistory,
        snapshot: BoundedThreadMessageSnapshot,
    ) -> Self {
        Self {
            behavior: ScriptedThreadBehavior::ThreadArtifact {
                history: Box::new(history),
                snapshot: Box::new(snapshot),
                bounded_read_outcome: BoundedReadOutcome::BackendError,
            },
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn thread_artifact_limit_exceeded(
        history: ThreadHistory,
        snapshot: BoundedThreadMessageSnapshot,
    ) -> Self {
        Self {
            behavior: ScriptedThreadBehavior::ThreadArtifact {
                history: Box::new(history),
                snapshot: Box::new(snapshot),
                bounded_read_outcome: BoundedReadOutcome::LimitExceeded,
            },
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn list_pages(responses: Vec<ListThreadsForScopeResponse>) -> Self {
        Self {
            behavior: ScriptedThreadBehavior::ListPages,
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(responses),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn submitted_replay(turn_run_id: Option<String>) -> Self {
        Self {
            behavior: ScriptedThreadBehavior::SubmittedReplay { turn_run_id },
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn rejected_busy_replay() -> Self {
        Self {
            behavior: ScriptedThreadBehavior::RejectedBusyReplay,
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    /// Scripted service for the mark-failure reconcile path:
    /// - `accept_inbound_message` accepts the message
    /// - `mark_message_rejected_busy` returns a backend error
    /// - `replay_accepted_inbound_message` returns `None` on the first two
    ///   calls (idempotency probes) and `Some(RejectedBusy)` on the third
    ///   call (reconcile probe), so `reconcile_terminal_duplicate` settles
    ///   without error
    pub(crate) fn rejected_busy_mark_fails() -> Self {
        Self {
            behavior: ScriptedThreadBehavior::RejectedBusyMarkFails {
                message_id: ThreadMessageId::new(),
            },
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    /// Scripted service for the legacy DeferredBusy mark-failure path:
    /// - `accept_inbound_message` accepts the message
    /// - `mark_message_rejected_busy` returns a backend error
    /// - `replay_accepted_inbound_message` returns `None` on the first two
    ///   calls (idempotency probes) and `Some(DeferredBusy)` on the reconcile
    ///   probe.  `DeferredBusy` is non-terminal: `reconcile_terminal_duplicate`
    ///   no longer accepts it as settled (only `RejectedBusy` qualifies), so the
    ///   mark failure propagates as a retryable Unavailable error rather than
    ///   silently producing a false-terminal RejectedBusy.
    pub(crate) fn deferred_busy_mark_fails() -> Self {
        Self {
            behavior: ScriptedThreadBehavior::DeferredBusyMarkFails {
                message_id: ThreadMessageId::new(),
            },
            history_requests: Mutex::new(Vec::new()),
            bounded_requests: Mutex::new(Vec::new()),
            list_requests: Mutex::new(Vec::new()),
            list_responses: Mutex::new(Vec::new()),
            replay_call_count: Mutex::new(0),
        }
    }

    pub(crate) fn history_requests(&self) -> Vec<ThreadHistoryRequest> {
        self.history_requests.lock().expect("lock").clone()
    }

    pub(crate) fn list_requests(&self) -> Vec<ListThreadsForScopeRequest> {
        self.list_requests.lock().expect("lock").clone()
    }

    pub(crate) fn bounded_requests(&self) -> Vec<BoundedThreadMessagesRequest> {
        self.bounded_requests.lock().expect("lock").clone()
    }
}

#[async_trait]
impl SessionThreadService for ScriptedThreadService {
    async fn list_thread_history(
        &self,
        request: ThreadHistoryRequest,
    ) -> Result<ThreadHistory, SessionThreadError> {
        self.history_requests
            .lock()
            .expect("lock")
            .push(request.clone());
        match &self.behavior {
            ScriptedThreadBehavior::BackendHistory => Err(SessionThreadError::Backend(
                "backend detail /host/path secret-token".to_string(),
            )),
            ScriptedThreadBehavior::History(history) => Ok(history.as_ref().clone()),
            ScriptedThreadBehavior::ThreadArtifact { history, .. } => Ok(history.as_ref().clone()),
            ScriptedThreadBehavior::ListPages => scripted_stub_unreachable("list_thread_history"),
            ScriptedThreadBehavior::SubmittedReplay { .. }
            | ScriptedThreadBehavior::RejectedBusyReplay
            | ScriptedThreadBehavior::RejectedBusyMarkFails { .. }
            | ScriptedThreadBehavior::DeferredBusyMarkFails { .. } => Ok(ThreadHistory {
                thread: SessionThreadRecord {
                    scope: request.scope,
                    thread_id: request.thread_id,
                    created_by_actor_id: "user-alpha".to_string(),
                    title: None,
                    metadata_json: None,
                    goal: None,
                    created_at: None,
                    updated_at: None,
                },
                messages: Vec::new(),
                summary_artifacts: Vec::new(),
            }),
        }
    }

    async fn ensure_thread(
        &self,
        _request: EnsureThreadRequest,
    ) -> Result<SessionThreadRecord, SessionThreadError> {
        scripted_stub_unreachable("ensure_thread")
    }

    async fn accept_inbound_message(
        &self,
        request: AcceptInboundMessageRequest,
    ) -> Result<AcceptedInboundMessage, SessionThreadError> {
        match &self.behavior {
            ScriptedThreadBehavior::RejectedBusyMarkFails { message_id }
            | ScriptedThreadBehavior::DeferredBusyMarkFails { message_id } => {
                Ok(AcceptedInboundMessage {
                    thread_id: request.thread_id,
                    message_id: *message_id,
                    sequence: 1,
                    idempotent_replay: false,
                    replay_metadata: Default::default(),
                })
            }
            _ => scripted_stub_unreachable("accept_inbound_message"),
        }
    }

    async fn replay_accepted_inbound_message(
        &self,
        request: ReplayAcceptedInboundMessageRequest,
    ) -> Result<Option<AcceptedInboundMessageReplay>, SessionThreadError> {
        match &self.behavior {
            ScriptedThreadBehavior::SubmittedReplay { turn_run_id } => {
                Ok(Some(AcceptedInboundMessageReplay {
                    scope: request.scope,
                    thread_id: ThreadId::new("thread-alpha").expect("valid thread"),
                    message_id: ThreadMessageId::new(),
                    sequence: 1,
                    status: MessageStatus::Submitted,
                    actor_id: Some(request.actor_id),
                    source_binding_id: Some(request.source_binding_id),
                    reply_target_binding_id: Some("webui-reply:replayed".to_string()),
                    turn_run_id: turn_run_id.clone(),
                    replay_metadata: Default::default(),
                }))
            }
            ScriptedThreadBehavior::RejectedBusyReplay => Ok(Some(AcceptedInboundMessageReplay {
                scope: request.scope,
                thread_id: ThreadId::new("thread-alpha").expect("valid thread"),
                message_id: ThreadMessageId::new(),
                sequence: 1,
                status: MessageStatus::RejectedBusy,
                actor_id: Some(request.actor_id),
                source_binding_id: Some(request.source_binding_id),
                reply_target_binding_id: Some("webui-reply:replayed".to_string()),
                turn_run_id: None,
                replay_metadata: Default::default(),
            })),
            ScriptedThreadBehavior::RejectedBusyMarkFails { message_id } => {
                // replay_webui_send_message probes with two source-binding variants
                // (main + legacy) before accepting the message, so calls 1 and 2
                // are the initial idempotency probes — both must return None so
                // accept_inbound_message is reached.  Call 3+ comes from
                // reconcile_terminal_duplicate after mark_message_rejected_busy
                // fails; return the already-settled RejectedBusy so reconciliation
                // succeeds without propagating the mark error.
                let mut count = self.replay_call_count.lock().expect("lock");
                *count += 1;
                if *count <= 2 {
                    Ok(None)
                } else {
                    Ok(Some(AcceptedInboundMessageReplay {
                        scope: request.scope,
                        thread_id: ThreadId::new("thread-alpha").expect("valid thread"),
                        message_id: *message_id,
                        sequence: 1,
                        status: MessageStatus::RejectedBusy,
                        actor_id: Some(request.actor_id),
                        source_binding_id: Some(request.source_binding_id),
                        reply_target_binding_id: Some("webui-reply:replayed".to_string()),
                        turn_run_id: None,
                        replay_metadata: Default::default(),
                    }))
                }
            }
            ScriptedThreadBehavior::DeferredBusyMarkFails { message_id } => {
                // Same two-phase probe as RejectedBusyMarkFails: calls 1 and 2 are
                // the initial idempotency probes and must return None.  Call 3+
                // comes from reconcile_terminal_duplicate; return legacy DeferredBusy.
                // DeferredBusy is non-terminal — reconcile_terminal_duplicate accepts
                // only RejectedBusy as settled, so this replay does NOT satisfy
                // reconciliation.  The original mark failure surfaces as an error.
                let mut count = self.replay_call_count.lock().expect("lock");
                *count += 1;
                if *count <= 2 {
                    Ok(None)
                } else {
                    Ok(Some(AcceptedInboundMessageReplay {
                        scope: request.scope,
                        thread_id: ThreadId::new("thread-alpha").expect("valid thread"),
                        message_id: *message_id,
                        sequence: 1,
                        status: MessageStatus::DeferredBusy,
                        actor_id: Some(request.actor_id),
                        source_binding_id: Some(request.source_binding_id),
                        reply_target_binding_id: Some("webui-reply:replayed".to_string()),
                        turn_run_id: None,
                        replay_metadata: Default::default(),
                    }))
                }
            }
            ScriptedThreadBehavior::BackendHistory
            | ScriptedThreadBehavior::History(_)
            | ScriptedThreadBehavior::ThreadArtifact { .. }
            | ScriptedThreadBehavior::ListPages => {
                scripted_stub_unreachable("replay_accepted_inbound_message")
            }
        }
    }

    async fn mark_message_submitted(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
        _turn_id: String,
        _turn_run_id: String,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("mark_message_submitted")
    }

    async fn mark_message_rejected_busy(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        match &self.behavior {
            ScriptedThreadBehavior::RejectedBusyMarkFails { .. }
            | ScriptedThreadBehavior::DeferredBusyMarkFails { .. } => {
                Err(SessionThreadError::Backend(
                    "simulated backend failure in mark_message_rejected_busy".to_string(),
                ))
            }
            _ => scripted_stub_unreachable("mark_message_rejected_busy"),
        }
    }

    async fn read_thread_message(
        &self,
        _scope: &ThreadScope,
        thread_id: &ThreadId,
        message_id: ThreadMessageId,
    ) -> Result<Option<ThreadMessageRecord>, SessionThreadError> {
        // Reconcile point-read for the mark-failure scenarios: report the row
        // in the settled state the scripted behavior models.
        let status = match &self.behavior {
            ScriptedThreadBehavior::RejectedBusyMarkFails { .. } => MessageStatus::RejectedBusy,
            ScriptedThreadBehavior::DeferredBusyMarkFails { .. } => MessageStatus::DeferredBusy,
            _ => scripted_stub_unreachable("read_thread_message"),
        };
        Ok(Some(ThreadMessageRecord {
            message_id,
            thread_id: thread_id.clone(),
            sequence: 1,
            kind: MessageKind::User,
            status,
            created_at: None,
            updated_at: None,
            actor_id: None,
            source_binding_id: None,
            reply_target_binding_id: None,
            turn_id: None,
            turn_run_id: None,
            tool_result_ref: None,
            tool_result_provider_call: None,
            content: Some("scripted".to_string()),
            attachments: Vec::new(),
            redaction_ref: None,
        }))
    }

    async fn append_assistant_draft(
        &self,
        _request: AppendAssistantDraftRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("append_assistant_draft")
    }

    async fn append_tool_result_reference(
        &self,
        _request: AppendToolResultReferenceRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("append_tool_result_reference")
    }

    async fn append_capability_display_preview(
        &self,
        _request: AppendCapabilityDisplayPreviewRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("append_capability_display_preview")
    }

    async fn update_tool_result_reference(
        &self,
        _request: UpdateToolResultReferenceRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("update_tool_result_reference")
    }

    async fn update_assistant_draft(
        &self,
        _request: UpdateAssistantDraftRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("update_assistant_draft")
    }

    async fn finalize_assistant_message(
        &self,
        _scope: &ThreadScope,
        _thread_id: &ThreadId,
        _message_id: ThreadMessageId,
        _content: MessageContent,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("finalize_assistant_message")
    }

    async fn redact_message(
        &self,
        _request: RedactMessageRequest,
    ) -> Result<ThreadMessageRecord, SessionThreadError> {
        scripted_stub_unreachable("redact_message")
    }

    async fn load_context_window(
        &self,
        _request: LoadContextWindowRequest,
    ) -> Result<ContextWindow, SessionThreadError> {
        scripted_stub_unreachable("load_context_window")
    }

    async fn load_context_messages(
        &self,
        _request: LoadContextMessagesRequest,
    ) -> Result<ContextMessages, SessionThreadError> {
        scripted_stub_unreachable("load_context_messages")
    }

    async fn list_thread_messages_bounded(
        &self,
        request: BoundedThreadMessagesRequest,
    ) -> Result<BoundedThreadMessages, SessionThreadError> {
        self.bounded_requests.lock().expect("lock").push(request);
        match &self.behavior {
            ScriptedThreadBehavior::ThreadArtifact {
                snapshot,
                bounded_read_outcome,
                ..
            } => match bounded_read_outcome {
                BoundedReadOutcome::Complete => Ok(BoundedThreadMessages::Complete(Box::new(
                    snapshot.as_ref().clone(),
                ))),
                BoundedReadOutcome::BackendError => Err(SessionThreadError::Backend(
                    "bounded query unsupported".to_string(),
                )),
                BoundedReadOutcome::LimitExceeded => Ok(BoundedThreadMessages::LimitExceeded),
            },
            _ => scripted_stub_unreachable("list_thread_messages_bounded"),
        }
    }

    async fn create_summary_artifact(
        &self,
        _request: CreateSummaryArtifactRequest,
    ) -> Result<SummaryArtifact, SessionThreadError> {
        scripted_stub_unreachable("create_summary_artifact")
    }

    async fn list_threads_for_scope(
        &self,
        request: ListThreadsForScopeRequest,
    ) -> Result<ListThreadsForScopeResponse, SessionThreadError> {
        match &self.behavior {
            ScriptedThreadBehavior::ListPages => {
                self.list_requests.lock().expect("lock").push(request);
                let mut responses = self.list_responses.lock().expect("lock");
                if responses.is_empty() {
                    scripted_stub_unreachable("list_threads_for_scope");
                }
                Ok(responses.remove(0))
            }
            _ => scripted_stub_unreachable("list_threads_for_scope"),
        }
    }
}
