//! Thread lifecycle and access resolution, thread-list and timeline reads, their
//! page-size clamps and cursor codec, and per-thread operation locking
//! (`threads` charter row).

use super::*;

pub const THREAD_DELETE_CAPABILITY_ID: &str = "builtin.thread_delete";

pub const THREAD_DELETE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(THREAD_DELETE_CAPABILITY_ID);

pub const CREATE_THREAD_COMMAND_ID: &str = "thread.create";

pub const CREATE_THREAD_COMMAND: ProductSurfaceCommandDescriptor<
    ProductCreateThreadRequest,
    RebornCreateThreadResponse,
> = ProductSurfaceCommandDescriptor::new(CREATE_THREAD_COMMAND_ID);

pub const THREADS_VIEW: ProductView<ProductListThreadsRequest, RebornListThreadsResponse> =
    ProductView::paginated("threads");

pub const TIMELINE_VIEW: ProductView<RebornTimelineRequest, RebornTimelineResponse> =
    ProductView::paginated("timeline");

pub(super) type ThreadOperationLocks = StdMutex<HashMap<String, Weak<AsyncMutex<()>>>>;

pub(super) struct ResolvedThreadAccess {
    pub(super) scope: TurnScope,
    pub(super) run_actor: TurnActor,
}

/// Ownership probes must collapse "thread does not exist" and "thread exists
/// but is owned by another caller" into NotFound so that a caller sharing the
/// (tenant, agent, project) scope cannot tell whether the supplied `thread_id`
/// matches a real thread under a different owner. The current backends return
/// `UnknownThread` for both cases on `list_thread_history`, but the contract
/// also permits `ThreadScopeMismatch`; remap it explicitly so a future backend
/// change cannot silently reintroduce an existence-leak.
pub(super) fn map_ownership_probe_error(error: SessionThreadError) -> ProductSurfaceError {
    match &error {
        SessionThreadError::ThreadScopeMismatch { .. } => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        }
        _ => map_thread_error(error),
    }
}

pub(super) fn thread_scope_from_turn_scope(
    scope: &TurnScope,
    owner_user_id: Option<ironclaw_host_api::ids::UserId>,
) -> Result<ThreadScope, ProductSurfaceError> {
    let Some(agent_id) = scope.agent_id.clone() else {
        return Err(ProductSurfaceError::from_status(
            ProductSurfaceErrorCode::InvalidRequest,
            400,
            false,
        ));
    };
    Ok(ThreadScope {
        tenant_id: scope.tenant_id.clone(),
        agent_id,
        project_id: scope.project_id.clone(),
        owner_user_id,
        mission_id: None,
    })
}

pub(super) fn parse_thread_id_field(
    field: &'static str,
    value: String,
) -> Result<ThreadId, ProductSurfaceError> {
    ThreadId::new(value).map_err(|_| {
        ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidId)
    })
}

pub(super) fn thread_operation_key(scope: &TurnScope) -> String {
    format!(
        "{}{}{}{}{}",
        segment("tenant", scope.tenant_id.as_str()),
        segment(
            "agent",
            scope.agent_id.as_ref().map(AgentId::as_str).unwrap_or("")
        ),
        segment(
            "project",
            scope
                .project_id
                .as_ref()
                .map(ProjectId::as_str)
                .unwrap_or("")
        ),
        segment("thread", scope.thread_id.as_str()),
        segment(
            "owner",
            scope
                .explicit_owner_user_id()
                .map(UserId::as_str)
                .unwrap_or("")
        )
    )
}

/// Default page size for [`TIMELINE_VIEW`] when the
/// caller does not supply one. Sized to cover a typical chat history
/// without forcing a multi-megabyte JSON response on first load.
pub(crate) const TIMELINE_DEFAULT_PAGE_SIZE: u32 = 100;

/// Hard ceiling on the number of messages a single timeline response can
/// carry. Callers asking for more get the cap. Without this, a large
/// thread would let the per-route rate limit be the only thing bounding
/// per-request response size, which was the original Medium review
/// issue.
pub(crate) const TIMELINE_MAX_PAGE_SIZE: u32 = 200;

/// Hard ceiling on summary artifacts returned per response. Summary
/// artifacts are typically much smaller than the message transcript so
/// this cap is generous; it exists to bound the worst case where a
/// thread accumulates an unusual number of summaries.
pub(super) const TIMELINE_MAX_SUMMARY_ARTIFACTS: usize = 200;

pub(super) const THREAD_LIST_DEFAULT_PAGE_SIZE: u32 = 50;

pub(super) const THREAD_LIST_MAX_PAGE_SIZE: u32 = 200;

pub(super) const THREAD_LIST_FILTER_MIN_FETCH_SIZE: usize = 50;

pub(super) const THREAD_LIST_FILTER_MAX_PAGES: usize = 20;

pub(super) fn clamp_timeline_limit(requested: Option<u32>) -> usize {
    let raw = requested.unwrap_or(TIMELINE_DEFAULT_PAGE_SIZE);
    let clamped = raw.clamp(1, TIMELINE_MAX_PAGE_SIZE);
    clamped as usize
}

pub(super) fn clamp_thread_list_limit(requested: Option<u32>) -> usize {
    let raw = requested.unwrap_or(THREAD_LIST_DEFAULT_PAGE_SIZE);
    let clamped = raw.clamp(1, THREAD_LIST_MAX_PAGE_SIZE);
    clamped as usize
}

/// Wire shape of the opaque timeline cursor. The browser does not need
/// to interpret this; it just echoes the previous response's
/// `next_cursor` back as the next request's `cursor`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct TimelineCursor {
    /// Only return messages whose `sequence` is strictly less than this
    /// value. Naming is deliberate: `before_*` makes the directional
    /// semantics (page backward through history) obvious at call sites.
    before_message_sequence: u64,
}

pub(super) fn parse_timeline_cursor(
    raw: Option<&str>,
) -> Result<Option<TimelineCursor>, ProductSurfaceError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let cursor: TimelineCursor = serde_json::from_str(raw).map_err(|_| {
        ProductSurfaceError::validation("cursor", ProductSurfaceValidationCode::InvalidValue)
    })?;
    Ok(Some(cursor))
}

pub(super) fn serialize_timeline_cursor(cursor: &TimelineCursor) -> Option<String> {
    // Serialization of a tiny tagged struct is total in practice, but
    // returning Option keeps the call site honest without an unwrap.
    serde_json::to_string(cursor).ok()
}

/// Slice the message transcript to the most recent `limit` messages
/// strictly older than `cursor.before_message_sequence` (or the most
/// recent `limit` overall when no cursor is supplied), returning the
/// page plus the cursor the caller should pass back to load the page
/// preceding this one. `None` for `next_cursor` means there is nothing
/// older — the caller has reached the start of the thread.
///
/// Messages are sorted by `sequence` ascending before slicing so the
/// returned page is in monotonic order regardless of the input order
/// the underlying store happens to produce.
pub(super) fn paginate_timeline_messages(
    mut messages: Vec<ironclaw_threads::ThreadMessageRecord>,
    limit: usize,
    cursor: Option<TimelineCursor>,
) -> (Vec<ironclaw_threads::ThreadMessageRecord>, Option<String>) {
    messages.sort_by_key(|message| message.sequence);
    if let Some(cursor) = cursor.as_ref() {
        messages.retain(|message| message.sequence < cursor.before_message_sequence);
    }
    let total = messages.len();
    let start = total.saturating_sub(limit);
    let next_cursor = if start > 0 {
        // The next page is older than the oldest message in *this* page.
        // We take the sequence of the page's first (oldest) message and
        // use it as `before_message_sequence` for the follow-up: that
        // request returns messages with sequence < this one, i.e. the
        // page strictly preceding the current one.
        messages.get(start).and_then(|message| {
            serialize_timeline_cursor(&TimelineCursor {
                before_message_sequence: message.sequence,
            })
        })
    } else {
        None
    };
    let page: Vec<_> = messages.into_iter().skip(start).collect();
    (page, next_cursor)
}

pub(super) fn cap_summary_artifacts(
    mut artifacts: Vec<ironclaw_threads::SummaryArtifact>,
) -> Vec<ironclaw_threads::SummaryArtifact> {
    if artifacts.len() > TIMELINE_MAX_SUMMARY_ARTIFACTS {
        artifacts.truncate(TIMELINE_MAX_SUMMARY_ARTIFACTS);
    }
    artifacts
}

pub(super) fn map_timeline_probe_error(error: SessionThreadError) -> ProductSurfaceError {
    match error {
        SessionThreadError::Serialization(_)
        | SessionThreadError::Deserialization(_)
        | SessionThreadError::InvalidMessageTimestamp { .. }
        | SessionThreadError::Backend(_) => {
            // The boundary error is sanitized to a retryable 503; the failure
            // still has to be visible server-side or it is undiagnosable. Log
            // the detail-free kind rather than the Display, whose Backend
            // variant carries virtual tenant/user paths and raw backend text.
            tracing::warn!(
                error_kind = error.kind_name(),
                "timeline probe failed; returning retryable TimelineUnavailable"
            );
            ProductSurfaceError::from_status_kind(
                ProductSurfaceErrorCode::Unavailable,
                ProductSurfaceErrorKind::TimelineUnavailable,
                503,
                true,
            )
        }
        _ => map_ownership_probe_error(error),
    }
}

pub(super) fn map_thread_error(error: SessionThreadError) -> ProductSurfaceError {
    match error {
        SessionThreadError::UnknownThread { .. } | SessionThreadError::UnknownMessage { .. } => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        }
        SessionThreadError::IdempotentReplayThreadMismatch { .. } => {
            ProductSurfaceError::from_status_kind(
                ProductSurfaceErrorCode::Conflict,
                ProductSurfaceErrorKind::Duplicate,
                409,
                false,
            )
        }
        SessionThreadError::ThreadScopeMismatch { .. }
        | SessionThreadError::IdempotentReplayActorMismatch { .. }
        | SessionThreadError::StructuredFinalizationConflict { .. }
        | SessionThreadError::StructuredFinalizationPublishMismatch { .. }
        | SessionThreadError::InvalidMessageTransition { .. }
        | SessionThreadError::MessageNotDraft { .. }
        | SessionThreadError::InvalidSummaryRange { .. }
        | SessionThreadError::OverlappingSummaryRange { .. } => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Conflict, 409, false)
        }
        SessionThreadError::InvalidAttachment(_)
        | SessionThreadError::InvalidPreparedContext { .. }
        | SessionThreadError::InvalidSubagentResult { .. }
        | SessionThreadError::PreparedContextKeyMismatch { .. } => {
            ProductSurfaceError::from_status_kind(
                ProductSurfaceErrorCode::InvalidRequest,
                ProductSurfaceErrorKind::Validation,
                400,
                false,
            )
        }
        SessionThreadError::GeneratedThreadId(_)
        | SessionThreadError::Serialization(_)
        | SessionThreadError::Deserialization(_)
        | SessionThreadError::ToolResultRecordRead(_)
        | SessionThreadError::InvalidStructuredFinalization { .. }
        | SessionThreadError::InvalidMessageTimestamp { .. }
        | SessionThreadError::Backend(_) => ProductSurfaceError::service_unavailable(true),
    }
}

pub(super) fn delete_thread_busy() -> ProductSurfaceError {
    ProductSurfaceError::from_status_kind(
        ProductSurfaceErrorCode::Conflict,
        ProductSurfaceErrorKind::Busy,
        409,
        false,
    )
}

pub(super) fn create_thread_metadata_json(
    client_action_id: &ironclaw_host_api::turn::IdempotencyKey,
) -> Result<String, ProductSurfaceError> {
    serde_json::to_string(&serde_json::json!({
        "client_action_id": client_action_id.as_str(),
    }))
    .map_err(ProductSurfaceError::internal_from)
}

pub(super) fn generated_thread_id(
    caller: &ProductSurfaceCaller,
    client_action_id: &ironclaw_host_api::turn::IdempotencyKey,
) -> ThreadId {
    let seed = format!(
        "{}{}{}{}{}{}",
        segment("surface", "webui-create-thread"),
        segment("tenant", caller.tenant_id.as_str()),
        segment("user", caller.user_id.as_str()),
        segment(
            "agent",
            caller.agent_id.as_ref().map(AgentId::as_str).unwrap_or("")
        ),
        segment(
            "project",
            caller
                .project_id
                .as_ref()
                .map(ironclaw_host_api::ids::ProjectId::as_str)
                .unwrap_or("")
        ),
        segment("action", client_action_id.as_str())
    );
    let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes());
    // UUID text contains no path separators/control characters and is accepted by ThreadId.
    match ThreadId::new(id.to_string()) {
        Ok(thread_id) => thread_id,
        Err(error) => {
            debug_assert!(false, "generated UUID thread id should be valid: {error}");
            // Fallback remains valid under ThreadId validation rules.
            ThreadId::new("generated-thread-fallback").unwrap_or_else(|_| unreachable!())
        }
    }
}

// Owner-bound thread resolution shared by the WebUI-facing methods that
// only need to prove a browser thread id belongs to the authenticated actor.
// The actor is pinned as `owner_user_id` so a caller sharing (tenant, agent,
// project) cannot act on a thread it does not own; `map_ownership_probe_error`
// collapses both UnknownThread and ThreadScopeMismatch into NotFound so the
// response cannot be used as an existence oracle.
//
// Automation-trigger threads are an exception: they are stored by
// `record_trigger_prompt` (trigger_poller_trusted_submit.rs) with
// `owner_user_id = Some(creator_user_id)` — the actor that fired the trigger
// — not the WebUI caller's user_id. The user-scoped probe therefore misses
// them. `resolve_thread_access_for_caller` handles that case via the shared
// automation fallback path; all interaction endpoints (stream, cancel, gate
// resolve, run-state) route through it so the reconstructed `TurnScope` (with
// `owner_user_id = Some(creator_user_id)`) is returned to callers that need
// to act on a trigger run.
//
// Authorization is revalidated on every call — no caching of the authz result
// — so a caller that loses automation visibility between polls cannot keep
// accessing the trigger-owned thread.
//
// Scope reconstruction field-by-field match against `record_trigger_prompt`
// (trigger_poller_trusted_submit.rs:285-291):
//   tenant_id    : resolution.turn_scope.tenant_id == caller's tenant_id (same installation)
//   agent_id     : resolution.turn_scope.agent_id OR default_agent_id
//                → trigger_scope.agent_id OR bound_caller.agent_id  (same fallback shape)
//   project_id   : resolution.turn_scope.project_id == trigger_scope.project_id
//   owner_user_id: Some(resolution.actor.user_id)
//                == Some(trigger_scope.creator_user_id)
//                == Some(fire.creator_user_id) [post-#4754: new first-fire bindings
//                   persist creator; legacy (pre-#4754) bindings remain owner-None
//                   and will not match — accepted breakage; recreate trigger to fix].
impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn build_threads_view(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductListThreadsRequest,
    ) -> Result<RebornListThreadsResponse, ProductSurfaceError> {
        // Reuse the same scope-construction shape the other v2 service
        // methods use: fail-closed when the caller has no agent
        // binding, owner-scope to the caller's user_id so the listing
        // is per-caller.
        let Some(agent_id) = caller.agent_id.clone() else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        let scope = ThreadScope {
            tenant_id: caller.tenant_id.clone(),
            agent_id,
            project_id: caller.project_id.clone(),
            owner_user_id: Some(caller.user_id.clone()),
            mission_id: None,
        };
        self.list_visible_threads_for_scope(scope, request, caller)
            .await
    }

    /// `requested_thread_id` makes the caller's choice authoritative.
    /// Without it, `client_action_id` deterministically derives the thread id
    /// so a retry of the same create maps back to the same thread.
    ///
    /// When the caller supplies an explicit `requested_thread_id`, an
    /// `ensure_thread` collision with a thread owned by another user is
    /// remapped to `NotFound` rather than the underlying `409 Conflict`.
    /// Otherwise the 400/409 distinction would be an existence oracle:
    /// callers sharing the same (tenant, agent, project) scope could probe
    /// for thread ids they did not create.
    pub async fn create_thread(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductCreateThreadRequest,
    ) -> Result<RebornCreateThreadResponse, ProductSurfaceError> {
        // A browser may propose a project for the new thread; authorize the
        // caller's access to it (never trust the body alone) and adopt it as the
        // thread's scope for this request only. Without a proposed project the
        // caller's default scope is used unchanged.
        let caller = self
            .authorize_create_thread_project(caller, request.project_id.clone())
            .await?;
        let command = request.into_command(caller.clone())?;
        let ProductInboundCommand::CreateThread {
            caller,
            client_action_id,
            requested_thread_id,
        } = command
        else {
            return Err(ProductSurfaceError::internal_invariant());
        };
        let caller_supplied_id = requested_thread_id.is_some();
        let thread_id =
            requested_thread_id.unwrap_or_else(|| generated_thread_id(&caller, &client_action_id));
        let scope = caller.turn_scope(thread_id.clone());
        let thread_scope = thread_scope_from_turn_scope(&scope, Some(caller.user_id.clone()))?;
        let thread = self
            .thread_service
            .ensure_thread(EnsureThreadRequest {
                scope: thread_scope,
                thread_id: Some(thread_id),
                created_by_actor_id: caller.user_id.as_str().to_string(),
                title: None,
                metadata_json: Some(create_thread_metadata_json(&client_action_id)?),
            })
            .await
            .map_err(|error| {
                if caller_supplied_id {
                    map_ownership_probe_error(error)
                } else {
                    // Deterministic generated ids derive from caller scope so
                    // a cross-user collision implies a UUIDv5 hash collision,
                    // which is not an oracle the caller can usefully probe.
                    // Preserve the underlying mapping for diagnosability.
                    map_thread_error(error)
                }
            })?;
        Ok(RebornCreateThreadResponse { thread })
    }

    pub async fn delete_thread(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornDeleteThreadRequest,
    ) -> Result<RebornDeleteThreadResponse, ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let scope = caller.turn_scope(thread_id.clone());
        let thread_scope = thread_scope_from_turn_scope(&scope, Some(caller.user_id.clone()))?;
        let _thread_operation_guard = self.lock_thread_operation(&scope).await;
        self.reject_delete_with_active_run(&scope, &thread_scope, &thread_id)
            .await?;
        self.thread_service
            .delete_thread(&thread_scope, &thread_id)
            .await
            .map_err(map_ownership_probe_error)?;
        Ok(RebornDeleteThreadResponse {
            thread_id,
            deleted: true,
        })
    }

    pub async fn get_timeline(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornTimelineRequest,
    ) -> Result<RebornTimelineResponse, ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let limit = clamp_timeline_limit(request.limit);
        let cursor = parse_timeline_cursor(request.cursor.as_deref())?;
        let scope = caller.turn_scope(thread_id);
        let (_thread_scope, history) = self
            .resolve_thread_history_for_caller(caller, &scope)
            .await?;

        let (messages, next_cursor) = paginate_timeline_messages(history.messages, limit, cursor);
        let summary_artifacts = cap_summary_artifacts(history.summary_artifacts);

        Ok(RebornTimelineResponse {
            thread: history.thread,
            messages,
            summary_artifacts,
            next_cursor,
        })
    }

    /// Shared authorization check for automation-trigger threads.
    ///
    /// Checks whether `scope.thread_id` belongs to one of the authenticated
    /// caller's automation triggers and, if so, returns a `TurnScope` with the
    /// TRUE stored scope (agent_id, project_id, and owner_user_id = creator_user_id).
    ///
    /// Requires #4754 ("Part A"): `record_trigger_prompt` stores threads with
    /// `owner_user_id = Some(fire.creator_user_id)` only for new first-fire
    /// bindings created after #4754 landed. Pre-#4754 (legacy) runs were stored
    /// with `owner_user_id = None`; their gate/cancel/run-state will NOT match
    /// the reconstructed scope — this is accepted breakage; recreating the
    /// trigger creates a fresh owner-bearing binding.
    ///
    /// Delegates to `AutomationProductService::resolve_run_thread_scope` which
    /// is caller-scoped: authorization is embedded in the repository lookup.
    /// If the trigger exists for this caller and contains the run, the returned
    /// scope lets all downstream storage lookups (timeline, gate, cancel, SSE)
    /// find the thread as stored rather than under the caller's session scope.
    ///
    /// Authorization is revalidated on every call (no caching) so a caller
    /// that loses automation visibility cannot keep acting on the thread.
    ///
    /// Returns `original_not_found_error` when:
    ///  - The caller has no bound agent.
    ///  - `resolve_run_thread_scope` returns `None` (thread not in caller's triggers).
    ///
    /// This is the authorization half of the trigger-thread fallback. Callers
    /// that need the full transcript call `try_automation_trigger_timeline_fallback`.
    pub(super) async fn check_automation_trigger_access(
        &self,
        caller: ProductSurfaceCaller,
        scope: &TurnScope,
        original_not_found_error: ProductSurfaceError,
    ) -> Result<ResolvedThreadAccess, ProductSurfaceError> {
        let Some(bound_caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(original_not_found_error);
        };
        let thread_id = &scope.thread_id;
        let Some(trigger_scope) = self
            .automation_service
            .resolve_run_thread_scope(bound_caller.clone(), thread_id)
            .await?
        else {
            return Err(original_not_found_error);
        };
        // Use the trigger's stored agent_id; fall back to the caller's agent_id
        // when the trigger record had no explicit agent.
        let true_agent_id = trigger_scope
            .agent_id
            .or_else(|| Some(bound_caller.agent_id.clone()));
        let run_actor = TurnActor::new(trigger_scope.creator_user_id.clone());
        Ok(ResolvedThreadAccess {
            scope: TurnScope::new_with_owner(
                scope.tenant_id.clone(),
                true_agent_id,
                trigger_scope.project_id,
                thread_id.clone(),
                Some(trigger_scope.creator_user_id),
            ),
            run_actor,
        })
    }

    /// Fallback timeline fetch for automation-trigger threads.
    ///
    /// Automation-trigger threads are created under the trigger creator's
    /// scope, not the caller's session scope. The normal user-scoped
    /// `list_thread_history` therefore always misses them. This fallback is
    /// only reached when the user-scoped lookup returned `UnknownThread` or
    /// `ThreadScopeMismatch`.
    ///
    /// Authorization: the thread_id must appear in at least one `recent_run`
    /// for an automation returned by `list_automations` for this caller. That
    /// is the same authorization check the Automations list endpoint applies,
    /// so no new trust boundary is introduced. Authorization is revalidated on
    /// every call — no caching.
    ///
    /// On authorization success, the history is loaded with the trigger-owned
    /// scope. On authorization failure (thread not in any of the caller's
    /// automation runs), the `original_not_found_error` is returned so the
    /// response is indistinguishable from a genuinely absent thread.
    /// Resolve a caller-visible thread's history together with the thread scope
    /// it actually lives under.
    ///
    /// The primary path is the caller's own session scope. On a 404-class miss
    /// it applies the automation-trigger fallback: trigger-fired threads are
    /// stored under the creator's scope, not the WebUI caller's session scope,
    /// so the user-scoped lookup always misses them. If the thread belongs to
    /// one of the caller's automations (`list_automations` applies the same
    /// authorization), the history is re-fetched under the trigger-owned scope.
    /// Both `UnknownThread` and `ThreadScopeMismatch` are eligible for the
    /// fallback; backend/serialization errors propagate as-is.
    ///
    /// Returning the resolved scope — not just the history — lets callers that
    /// must do further scope-bound work (e.g. reading attachment bytes through
    /// the project mount) address the correct scope instead of re-deriving the
    /// caller's session scope, which would be wrong for a trigger thread.
    pub(super) async fn resolve_thread_history_for_caller(
        &self,
        caller: ProductSurfaceCaller,
        scope: &TurnScope,
    ) -> Result<(ThreadScope, ThreadHistory), ProductSurfaceError> {
        let thread_scope =
            thread_scope_from_turn_scope(scope, Some(caller.actor().user_id.clone()))?;
        match self
            .thread_service
            .list_thread_history(ThreadHistoryRequest {
                scope: thread_scope.clone(),
                thread_id: scope.thread_id.clone(),
            })
            .await
        {
            Ok(history) => Ok((thread_scope, history)),
            Err(
                SessionThreadError::UnknownThread { .. }
                | SessionThreadError::ThreadScopeMismatch { .. },
            ) => {
                let original_error =
                    ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false);
                self.try_automation_trigger_timeline_fallback(caller, scope, original_error)
                    .await
            }
            Err(err) => Err(map_timeline_probe_error(err)),
        }
    }

    pub(super) async fn try_automation_trigger_timeline_fallback(
        &self,
        caller: ProductSurfaceCaller,
        scope: &TurnScope,
        original_not_found_error: ProductSurfaceError,
    ) -> Result<(ThreadScope, ThreadHistory), ProductSurfaceError> {
        let access = self
            .check_automation_trigger_access(caller, scope, original_not_found_error)
            .await?;
        // Authorized: re-fetch the history using the TRUE stored scope
        // (owner_user_id = creator_user_id, not the caller's session user) and
        // return that scope so byte reads address the trigger creator's mount.
        let true_thread_scope = thread_scope_from_turn_scope(
            &access.scope,
            access.scope.explicit_owner_user_id().cloned(),
        )?;
        let history = self
            .thread_service
            .list_thread_history(ThreadHistoryRequest {
                scope: true_thread_scope.clone(),
                thread_id: access.scope.thread_id.clone(),
            })
            .await
            .map_err(map_timeline_probe_error)?;
        Ok((true_thread_scope, history))
    }

    /// Ownership probe for interaction endpoints (stream, cancel, gate resolve,
    /// run-state).
    ///
    /// Tries the primary user-scoped `read_thread` probe. On a 404-class miss
    /// (UnknownThread / ThreadScopeMismatch), falls back to the automation
    /// trigger authorization check. If the thread belongs to one of the
    /// caller's automations, returns the trigger-owned `TurnScope` and run
    /// actor so downstream turn operations address the submitted run. Non-owner
    /// callers and genuinely absent threads both receive the same canonical
    /// NotFound response.
    ///
    /// Authorization is revalidated on every call — no caching of the authz
    /// result — so a caller that loses automation visibility cannot keep
    /// acting on the thread after their access is revoked.
    pub(super) async fn resolve_thread_access_for_caller(
        &self,
        caller: ProductSurfaceCaller,
        scope: TurnScope,
        actor: &TurnActor,
    ) -> Result<ResolvedThreadAccess, ProductSurfaceError> {
        let thread_scope = thread_scope_from_turn_scope(&scope, Some(actor.user_id.clone()))?;
        match self
            .thread_service
            .read_thread(ThreadHistoryRequest {
                scope: thread_scope.clone(),
                thread_id: scope.thread_id.clone(),
            })
            .await
        {
            Ok(_) => Ok(ResolvedThreadAccess {
                scope,
                run_actor: actor.clone(),
            }),
            Err(
                SessionThreadError::UnknownThread { .. }
                | SessionThreadError::ThreadScopeMismatch { .. },
            ) => {
                let original_error =
                    ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false);
                let access = self
                    .check_automation_trigger_access(caller, &scope, original_error)
                    .await?;
                Ok(ResolvedThreadAccess {
                    scope: access.scope,
                    run_actor: access.run_actor,
                })
            }
            Err(err) => Err(map_ownership_probe_error(err)),
        }
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker,
    V: RebornViewProvider,
{
    pub(super) async fn list_visible_threads_for_scope(
        &self,
        scope: ThreadScope,
        request: ProductListThreadsRequest,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornListThreadsResponse, ProductSurfaceError> {
        let visible_limit = clamp_thread_list_limit(request.limit);
        let needs_approval = request.needs_approval;
        if needs_approval {
            return tokio::time::timeout(
                NOTIFICATION_APPROVAL_QUERY_TIMEOUT,
                self.list_automation_threads_needing_approval(
                    caller,
                    visible_limit,
                    request.candidate_thread_id,
                ),
            )
            .await
            .map_err(|_| notification_approval_timeout_error())?;
        }
        let fetch_limit = visible_limit
            .max(THREAD_LIST_FILTER_MIN_FETCH_SIZE)
            .min(THREAD_LIST_MAX_PAGE_SIZE as usize);
        let mut cursor = request.cursor;
        let mut visible_threads = Vec::with_capacity(visible_limit);
        let mut next_cursor = None;
        let mut pages_fetched = 0usize;

        while visible_threads.len() < visible_limit {
            if pages_fetched >= THREAD_LIST_FILTER_MAX_PAGES {
                tracing::warn!(
                    cursor = ?cursor,
                    pages_fetched,
                    max_pages = THREAD_LIST_FILTER_MAX_PAGES,
                    visible_threads = visible_threads.len(),
                    visible_limit,
                    "thread listing filter page budget exhausted while skipping automation threads"
                );
                next_cursor = None;
                break;
            }
            pages_fetched += 1;
            let response = self
                .thread_service
                .list_threads_for_scope(ironclaw_threads::ListThreadsForScopeRequest {
                    scope: scope.clone(),
                    limit: Some(fetch_limit as u32),
                    cursor: cursor.clone(),
                })
                .await
                .map_err(map_thread_error)?;
            for thread in response.threads {
                if is_automation_trigger_thread(&thread) {
                    continue;
                }
                visible_threads.push(thread);
            }
            next_cursor = response.next_cursor;
            let Some(next) = next_cursor.clone() else {
                break;
            };
            if cursor.as_deref() == Some(next.as_str()) {
                tracing::warn!(
                    cursor = %next,
                    "thread listing cursor did not advance while filtering automation threads"
                );
                next_cursor = None;
                break;
            }
            cursor = Some(next);
        }

        if visible_threads.len() > visible_limit {
            next_cursor = visible_threads
                .get(visible_limit.saturating_sub(1))
                .map(|thread| thread.thread_id.as_str().to_string());
            visible_threads.truncate(visible_limit);
        }

        Ok(RebornListThreadsResponse {
            threads: visible_threads,
            next_cursor,
        })
    }

    pub(super) fn thread_operation_lock(&self, scope: &TurnScope) -> Arc<AsyncMutex<()>> {
        let key = thread_operation_key(scope);
        let mut locks = match self.thread_operation_locks.lock() {
            Ok(locks) => locks,
            Err(poisoned) => poisoned.into_inner(),
        };
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    pub(super) async fn lock_thread_operation(&self, scope: &TurnScope) -> OwnedMutexGuard<()> {
        self.thread_operation_lock(scope).lock_owned().await
    }

    pub(super) async fn reject_delete_with_active_run(
        &self,
        scope: &TurnScope,
        thread_scope: &ThreadScope,
        thread_id: &ThreadId,
    ) -> Result<(), ProductSurfaceError> {
        let history = self
            .thread_service
            .list_thread_history(ThreadHistoryRequest {
                scope: thread_scope.clone(),
                thread_id: thread_id.clone(),
            })
            .await
            .map_err(map_timeline_probe_error)?;
        let mut seen = HashSet::new();
        for run_id in history
            .messages
            .iter()
            .filter_map(|message| message.turn_run_id.as_deref())
            .map(parse_persisted_turn_run_id)
        {
            let run_id = run_id?;
            if !seen.insert(run_id) {
                continue;
            }
            match self
                .turn_coordinator
                .get_run_state(GetRunStateRequest {
                    scope: scope.clone(),
                    run_id,
                })
                .await
            {
                Ok(state) if state.status.keeps_active_lock() => {
                    return Err(delete_thread_busy());
                }
                Ok(_) | Err(TurnError::ScopeNotFound) => {}
                Err(error) => return Err(map_turn_error(error)),
            }
        }
        Ok(())
    }
}
