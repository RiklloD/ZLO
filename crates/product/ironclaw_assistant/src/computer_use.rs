//! Computer use as product glue over the
//! [`ComputerUseSessions`] port (implemented by `ironclaw_computer_use`,
//! which this crate does not name):
//!
//! - every server tool as a synthetic loop capability (`computer_use.<tool>`)
//!   that raises a durable approval request unless the user chose "always
//!   allow" for that tool. A tool setting of `Disabled` denies, `AskEachTime`
//!   always asks, and global auto-approve is ignored: it never hands the
//!   desktop to the model on its own. A resumed call runs only with a
//!   matching, claimed lease, then reaches the thread's session through the
//!   port;
//! - the thread Stop hook: a turn that is stopped or cancelled ends the
//!   thread's session (kills the process), any other turn end sends the
//!   server its turn-ended notification;
//! - the `computer_use.session` view and `computer_use.control` command
//!   (Stop, take over, hand back) behind the WebUI routes.
//!
//! The approval steps follow `crate::supermemory`'s write gate.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_approvals::{
    ApprovalRequestStorePort, ApprovalSettingsProvider, ApprovalStatus, ApprovalStoreError,
    GateRecordStorePort, ToolPermissionOverride,
};
use ironclaw_authorization::{
    CapabilityLeaseError, CapabilityLeaseStatus, CapabilityLeaseStorePort,
};
use ironclaw_capabilities::{ReplayPayload, ReplayPayloadStoreError, ReplayPayloadStorePort};
use ironclaw_host_api::{
    action::Action,
    approval::{ApprovalRequest, InvocationFingerprint},
    gate_record::GateRecord,
    ids::{
        ApprovalRequestId, CapabilityGrantId, CapabilityId, CorrelationId, GateRef, InvocationId,
        ThreadId, UserId,
    },
    resolution::Resolution,
    resource::{ResourceEstimate, ResourceScope},
    safe_summary::SafeSummary,
    scope::Principal,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityApprovalResume,
    CapabilityDeniedReasonKind, CapabilityProgress, CapabilityResumeToken, LoopRunContext,
    ModelVisibleToolObservation, ToolObservationDetail, resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticCapability, SyntheticCapabilityDescriptor,
    SyntheticCapabilityHandler, SyntheticCapabilityInvocation,
};
use ironclaw_processes::{
    ProcessJournalCommit, ProcessJournalCommitObserver, ProcessKind, ProcessLifecycleStatus,
};
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CAPABILITY_PREFIX, COMPUTER_USE_CONTROL_COMMAND_ID, COMPUTER_USE_FRAME_VIEW,
    COMPUTER_USE_RESULT_FRAME, COMPUTER_USE_SESSION_VIEW, COMPUTER_USE_STATE_VIEW,
    COMPUTER_USE_TOOL_NAME_PREFIX, ComputerUseControlRequest, ComputerUseFrameRequest,
    ComputerUseSession, ComputerUseSessionRequest, ComputerUseSessions, ComputerUseStateRequest,
    ComputerUseToolSpec,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};
use ironclaw_turns::LoopGateRef;
use serde_json::{Value, json};

/// Fixed, interpolation-free gate text.
const APPROVAL_SUMMARY: &str = "computer use requires approval";

/// The approval stores the gate persists to, all owned by composition.
pub struct ComputerUseApprovalStores {
    pub approval_requests: Arc<dyn ApprovalRequestStorePort>,
    pub capability_leases: Arc<dyn CapabilityLeaseStorePort>,
    pub approval_settings: Arc<dyn ApprovalSettingsProvider>,
    pub replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
    pub gate_record_store: Arc<dyn GateRecordStorePort>,
}

/// One approval-gated capability per server tool.
pub fn computer_use_capabilities<S: ComputerUseSessions + 'static>(
    sessions: Arc<S>,
    stores: ComputerUseApprovalStores,
    fallback_user_id: UserId,
) -> Result<Vec<SyntheticCapability>, AgentLoopHostError> {
    let stores = Arc::new(stores);
    let approved_turns = Arc::new(ApprovedTurns::default());
    let sessions: Arc<dyn ComputerUseSessions> = sessions;
    sessions
        .tools()
        .iter()
        .map(|tool| {
            Ok(SyntheticCapability::new(
                SyntheticCapabilityDescriptor::new(
                    &format!("{COMPUTER_USE_CAPABILITY_PREFIX}{}", tool.name),
                    // The model-protocol name, needed only by this descriptor.
                    &format!("{COMPUTER_USE_TOOL_NAME_PREFIX}{}", tool.name),
                    &tool.description,
                    tool.input_schema.clone(),
                )?,
                Arc::new(ToolHandler {
                    tool: tool.clone(),
                    sessions: Arc::clone(&sessions),
                    stores: Arc::clone(&stores),
                    approved_turns: Arc::clone(&approved_turns),
                    fallback_user_id: fallback_user_id.clone(),
                }),
            ))
        })
        .collect()
}

struct ToolHandler {
    tool: ComputerUseToolSpec,
    sessions: Arc<dyn ComputerUseSessions>,
    stores: Arc<ComputerUseApprovalStores>,
    approved_turns: Arc<ApprovedTurns>,
    fallback_user_id: UserId,
}

/// Turns whose first computer-use call the user approved: the rest of that
/// task runs without asking again (one approval per task, not per click).
/// In memory and bounded; a restart asks again.
#[derive(Default)]
struct ApprovedTurns(std::sync::Mutex<std::collections::VecDeque<ironclaw_turns::TurnId>>);

const APPROVED_TURNS_KEPT: usize = 64;

impl ApprovedTurns {
    fn approve(&self, turn: &ironclaw_turns::TurnId) {
        let mut turns = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !turns.contains(turn) {
            if turns.len() == APPROVED_TURNS_KEPT {
                turns.pop_front();
            }
            turns.push_back(*turn);
        }
    }

    fn approved(&self, turn: &ironclaw_turns::TurnId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(turn)
    }
}

struct ApprovedLease {
    scope: ResourceScope,
    lease_id: CapabilityGrantId,
}

#[async_trait]
impl SyntheticCapabilityHandler for ToolHandler {
    fn validate_provider_arguments(&self, arguments: &Value) -> Result<(), AgentLoopHostError> {
        let Some(object) = arguments.as_object() else {
            return Err(host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                "computer-use arguments must be an object",
            ));
        };
        let required = self
            .tool
            .input_schema
            .get("required")
            .and_then(Value::as_array);
        match required
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|field| !object.contains_key(*field))
        {
            Some(field) => Err(host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                format!("{field} is required"),
            )),
            None => Ok(()),
        }
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        if invocation.request.auth_resume.is_some() {
            return Err(host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                "computer use does not support auth resume",
            ));
        }
        let capability_id = self.capability_id()?;
        let lease = match invocation.request.approval_resume.clone() {
            Some(resume) => match self
                .verify_resume(&invocation, &resume, &capability_id)
                .await?
            {
                Ok(lease) => {
                    self.approved_turns.approve(&invocation.run_context.turn_id);
                    Some(lease)
                }
                Err(denied) => return Ok(denied),
            },
            None => {
                let scope = self.settings_scope(&invocation.run_context);
                let settings = &self.stores.approval_settings;
                // "Full access" is the thread's profile, so ask with the thread scope.
                let thread_scope = invocation
                    .run_context
                    .acting_resource_scope(&self.fallback_user_id);
                let tool_override = settings.tool_override(&scope, &capability_id).await;
                if matches!(tool_override, Some(ToolPermissionOverride::Disabled)) {
                    return denied("computer use is disabled by tool approval settings");
                }
                let full_access = settings.full_access(&thread_scope).await;
                if !full_access
                    && (matches!(tool_override, Some(ToolPermissionOverride::AskEachTime))
                        || (!self
                            .approved_turns
                            .approved(&invocation.run_context.turn_id)
                            && !settings
                                .tool_always_allow(&scope, &capability_id, &grantee()?)
                                .await))
                {
                    return self.request_approval(&invocation, &capability_id).await;
                }
                None
            }
        };
        // A claimed lease is consumed before acting so it cannot be replayed
        // if the call is cut short.
        if let Some(lease) = lease
            && let Err(error) = self
                .stores
                .capability_leases
                .consume(&lease.scope, lease.lease_id)
                .await
        {
            return lease_outcome("consume_approval_lease", error);
        }
        let scope = invocation
            .run_context
            .acting_resource_scope(&self.fallback_user_id);
        let session = ComputerUseSession::from_scope(&scope);
        let mut output = self
            .sessions
            .call_tool(scope, &self.tool.name, invocation.input.clone())
            .await;
        // The screenshot number is host bookkeeping, not model input.
        let frame = output
            .as_object_mut()
            .and_then(|fields| fields.remove(COMPUTER_USE_RESULT_FRAME))
            .and_then(|seq| seq.as_u64());
        complete(&invocation, output, &self.tool.name, |result_ref| {
            if let (Some(seq), Some(session)) = (frame, &session) {
                self.sessions.bind_result_frame(session, seq, result_ref);
            }
        })
        .await
    }
}

impl ToolHandler {
    fn capability_id(&self) -> Result<CapabilityId, AgentLoopHostError> {
        CapabilityId::new(format!(
            "{COMPUTER_USE_CAPABILITY_PREFIX}{}",
            self.tool.name
        ))
        .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))
    }

    fn settings_scope(&self, run: &LoopRunContext) -> ResourceScope {
        ResourceScope {
            tenant_id: run.scope.tenant_id.clone(),
            user_id: run.acting_user_id(&self.fallback_user_id),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        }
    }

    fn invocation_scope(&self, run: &LoopRunContext, invocation_id: InvocationId) -> ResourceScope {
        let mut scope = run.acting_resource_scope(&self.fallback_user_id);
        scope.invocation_id = invocation_id;
        scope
    }

    async fn request_approval(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        capability_id: &CapabilityId,
    ) -> Result<Resolution, AgentLoopHostError> {
        let run = &invocation.run_context;
        let approval_request_id = ApprovalRequestId::new();
        let correlation_id = CorrelationId::new();
        let invocation_id = InvocationId::new();
        let estimate = ResourceEstimate::default();
        let scope = self.invocation_scope(run, invocation_id);
        let fingerprint = fingerprint(&scope, capability_id, &estimate, &invocation.input)?;
        let replay_scope = run.acting_resource_scope(&self.fallback_user_id);
        self.stores
            .replay_payload_store
            .save(
                replay_scope.clone(),
                invocation_id,
                ReplayPayload {
                    input: invocation.input.clone(),
                    estimate: estimate.clone(),
                    prior_approval: None,
                    input_ref: invocation.request.input_ref.clone(),
                    correlation_id,
                },
            )
            .await
            .map_err(replay_error)?;
        self.stores
            .approval_requests
            .save_pending(
                scope,
                ApprovalRequest {
                    id: approval_request_id,
                    correlation_id,
                    requested_by: grantee()?,
                    action: Box::new(Action::Dispatch {
                        capability: capability_id.clone(),
                        estimated_resources: estimate,
                    }),
                    invocation_fingerprint: Some(fingerprint),
                    reason: format!(
                        "Computer use for this task, starting with {}. Approving lets the \
                         agent use this Mac's apps until it answers.",
                        self.tool.name
                    ),
                    reusable_scope: None,
                },
            )
            .await
            .map_err(|error| store_error("save_pending_approval", error))?;
        let summary = SafeSummary::new(APPROVAL_SUMMARY).map_err(|error| {
            ironclaw_loop_host::raw_agent_loop_host_error(
                "computer_use",
                "gate_record_summary",
                AgentLoopHostErrorKind::Internal,
                "computer-use gate summary is not renderable",
                error,
            )
        })?;
        self.stores
            .gate_record_store
            .save(
                replay_scope,
                GateRef::for_approval_request(approval_request_id),
                GateRecord::Approval { summary },
            )
            .await
            .map_err(|error| store_error("save_gate_record", error))?;
        let gate_ref = LoopGateRef::new(format!("gate:approval-{approval_request_id}"))
            .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))?;
        let resume_token = CapabilityResumeToken::new(invocation_id.to_string())
            .map_err(|reason| host_error(AgentLoopHostErrorKind::Internal, reason.to_string()))?;
        Ok(resolution::approval_required(
            gate_ref,
            APPROVAL_SUMMARY.to_string(),
            Some(CapabilityApprovalResume {
                approval_request_id,
                resume_token,
                correlation_id,
                input_ref: invocation.request.input_ref.clone(),
            }),
        )
        .resolution)
    }

    /// `Ok(Ok(lease))` to act, `Ok(Err(denial))` for a lost or ungranted
    /// approval the user can re-request, `Err` for a mismatch.
    async fn verify_resume(
        &self,
        invocation: &SyntheticCapabilityInvocation,
        resume: &CapabilityApprovalResume,
        capability_id: &CapabilityId,
    ) -> Result<Result<ApprovedLease, Resolution>, AgentLoopHostError> {
        let run = &invocation.run_context;
        let invocation_id = InvocationId::parse(resume.resume_token.as_str()).map_err(|error| {
            host_error(AgentLoopHostErrorKind::InvalidInvocation, error.to_string())
        })?;
        let replay = self
            .stores
            .replay_payload_store
            .load(
                &run.acting_resource_scope(&self.fallback_user_id),
                invocation_id,
            )
            .await
            .map_err(replay_error)?
            .ok_or_else(|| {
                host_error(
                    AgentLoopHostErrorKind::Unavailable,
                    "computer-use approval replay payload is unavailable",
                )
            })?;
        let mismatch = |what: &str| {
            host_error(
                AgentLoopHostErrorKind::InvalidInvocation,
                format!("computer-use approval {what} does not match"),
            )
        };
        if replay.input != invocation.input {
            return Err(mismatch("input"));
        }
        let scope = self.invocation_scope(run, invocation_id);
        let fingerprint = fingerprint(&scope, capability_id, &replay.estimate, &invocation.input)?;
        let Some(record) = self
            .stores
            .approval_requests
            .get(&scope, resume.approval_request_id)
            .await
            .map_err(|error| store_error("load_approval", error))?
        else {
            return Ok(Err(denied(
                "computer-use approval is unavailable; re-request approval",
            )?));
        };
        if record.status != ApprovalStatus::Approved {
            return Ok(Err(denied(
                "computer-use approval has not been granted; re-request approval",
            )?));
        }
        if record.request.correlation_id != replay.correlation_id {
            return Err(mismatch("correlation"));
        }
        if record.request.invocation_fingerprint.as_ref() != Some(&fingerprint) {
            return Err(mismatch("fingerprint"));
        }
        if !matches!(record.request.action.as_ref(), Action::Dispatch { capability, .. } if capability == capability_id)
        {
            return Err(mismatch("action"));
        }
        let leases = &self.stores.capability_leases;
        let Some(lease) = leases
            .leases_for_scope(&scope)
            .await
            .into_iter()
            .find(|lease| {
                lease.status == CapabilityLeaseStatus::Active
                    && lease.grant.capability == *capability_id
                    && lease.grant.grantee == record.request.requested_by
                    && lease.invocation_fingerprint.as_ref() == Some(&fingerprint)
            })
        else {
            return Ok(Err(denied(
                "computer-use approval lease is unavailable; re-request approval",
            )?));
        };
        if let Err(error) = leases.claim(&scope, lease.grant.id, &fingerprint).await {
            return lease_outcome("claim_approval_lease", error).map(Err);
        }
        Ok(Ok(ApprovedLease {
            scope,
            lease_id: lease.grant.id,
        }))
    }
}

async fn complete(
    invocation: &SyntheticCapabilityInvocation,
    output: Value,
    name: &str,
    bind_frame: impl FnOnce(&str),
) -> Result<Resolution, AgentLoopHostError> {
    let summary = match output.get("ok") {
        Some(Value::Bool(false)) => format!(
            "{name} failed: {}",
            output
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("error")
        ),
        _ => format!("{name} done"),
    };
    let input_ref = invocation
        .request
        .approval_resume
        .as_ref()
        .map(|resume| &resume.input_ref)
        .unwrap_or(&invocation.request.input_ref);
    let serialized = serde_json::to_vec(&output).unwrap_or_default();
    let write = invocation
        .result_writer
        .write_capability_result(CapabilityResultWrite {
            run_context: &invocation.run_context,
            input_ref,
            invocation_id: InvocationId::new(),
            capability_id: &invocation.request.capability_id,
            output,
            display_preview: None,
            durable_persistence: DurablePersistence::Persist,
        })
        .await?;
    bind_frame(write.result_ref.as_str());
    let observation = write
        .model_observation
        .map(|observation| inline_observation(observation, &write.result_ref, &serialized));
    Ok(resolution::completed(
        write.result_ref,
        summary,
        CapabilityProgress::MadeProgress,
        false,
        write.byte_len,
        write.output_digest,
        observation,
    ))
}

/// The model acts on the window text it reads, so a state read or action
/// result is shown in full (up to [`INLINE_RESULT_MAX_BYTES`]) instead of the
/// generic 3 KB first look that hides the tree behind a `result_read` call.
/// A success is the engine's text as is, the way Hermes shows it; a failure
/// keeps the redacted JSON page. The stored result stays complete either way.
/// The whole observation must stay under the threads' 48 KB replay cap, and
/// a tree's tabs and newlines grow when that observation is serialized.
const INLINE_RESULT_MAX_BYTES: usize = 32 * 1024;

fn inline_observation(
    mut observation: ModelVisibleToolObservation,
    result_ref: &ironclaw_turns::LoopResultRef,
    serialized: &[u8],
) -> ModelVisibleToolObservation {
    let ToolObservationDetail::ResultReference {
        preview,
        structured_json_view,
        next_offset,
        item_count,
        ..
    } = &mut observation.detail
    else {
        return observation;
    };
    // Plain text keeps the tree's lines and indexes readable; inside a JSON
    // page it reached the model escaped twice and models re-read it.
    // A tree past the budget (Spotify's runs 60-90 KB) keeps its first lines
    // instead of collapsing to `{"ok":true}`, which left the model blind.
    if let Some(text) = success_text(serialized) {
        let full = text.len() <= INLINE_RESULT_MAX_BYTES;
        *preview = Some(if full { text } else { truncated_tree(&text) });
        *structured_json_view = false;
        *next_offset = None;
        *item_count = None;
        observation.summary = if full {
            "Tool completed; preview contains the full result."
        } else {
            "Tool completed; preview holds the first part of a long result."
        }
        .to_string();
        return observation;
    }
    let page = match ironclaw_threads::render_json_tool_result_page(
        result_ref.as_str(),
        serialized,
        "",
        0,
        INLINE_RESULT_MAX_BYTES,
        None,
    ) {
        Ok(page) => page,
        Err(error) => {
            tracing::debug!(%error, "computer-use result page unavailable; keeping first look");
            return observation;
        }
    };
    let complete = page.omitted.is_empty() && page.next.is_none();
    let Ok(text) = ironclaw_threads::model_result_preview_from_json_page(&page) else {
        return observation;
    };
    let text = text.into_inner();
    if text.len() > ironclaw_host_api::model_result_preview::MODEL_RESULT_PREVIEW_MAX_BYTES {
        return observation;
    }
    *preview = Some(text);
    *structured_json_view = true;
    *next_offset = None;
    *item_count = None;
    observation.summary = if complete {
        "Tool completed; preview contains the full result.".to_string()
    } else {
        "Tool completed; preview is a bounded JSON view. Select omitted nodes or use the preview's next request to continue.".to_string()
    };
    observation
}

/// The first whole lines of a long tree that fit the inline budget, and a
/// note pointing the model at `query` for the rest.
fn truncated_tree(text: &str) -> String {
    let note = format!(
        "\n(tree cut at {INLINE_RESULT_MAX_BYTES} of {} bytes: find other elements with query by text or role, or read again with a smaller max_tree_depth.)",
        text.len()
    );
    let mut end = INLINE_RESULT_MAX_BYTES.saturating_sub(note.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end].rfind('\n').unwrap_or(end);
    format!("{}{note}", &text[..end])
}

/// The engine's text of a successful call (`{"ok": true, "result": "..."}`).
fn success_text(serialized: &[u8]) -> Option<String> {
    let output: Value = serde_json::from_slice(serialized).ok()?;
    if output.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    match output.get("result")? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// The same synthetic grantee the other approval-gated builtin tools use.
fn grantee() -> Result<Principal, AgentLoopHostError> {
    crate::outbound_delivery_synthetic_provider()
        .map(Principal::Extension)
        .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))
}

fn fingerprint(
    scope: &ResourceScope,
    capability_id: &CapabilityId,
    estimate: &ResourceEstimate,
    input: &Value,
) -> Result<InvocationFingerprint, AgentLoopHostError> {
    InvocationFingerprint::for_dispatch(scope, capability_id, estimate, input)
        .map_err(|error| host_error(AgentLoopHostErrorKind::Internal, error.to_string()))
}

fn host_error(kind: AgentLoopHostErrorKind, message: impl Into<String>) -> AgentLoopHostError {
    AgentLoopHostError::new(kind, message)
}

fn denied(summary: &str) -> Result<Resolution, AgentLoopHostError> {
    let reason = CapabilityDeniedReasonKind::unknown("computer_use_approval_required")
        .map_err(|reason| host_error(AgentLoopHostErrorKind::Internal, reason.to_string()))?;
    Ok(resolution::denied(reason, summary.to_string()).resolution)
}

fn replay_error(error: ReplayPayloadStoreError) -> AgentLoopHostError {
    tracing::warn!(error = %error, "failed to access computer-use replay payload");
    host_error(
        AgentLoopHostErrorKind::Unavailable,
        "failed to access capability replay payload",
    )
}

fn store_error(operation: &'static str, error: ApprovalStoreError) -> AgentLoopHostError {
    ironclaw_loop_host::raw_agent_loop_host_error(
        "computer_use",
        operation,
        AgentLoopHostErrorKind::Unavailable,
        "computer-use approval state operation failed",
        error,
    )
}

/// A stale or lost lease is a recoverable denial; lease storage faults end the run.
fn lease_outcome(
    operation: &'static str,
    error: CapabilityLeaseError,
) -> Result<Resolution, AgentLoopHostError> {
    match error {
        CapabilityLeaseError::UnknownLease { .. }
        | CapabilityLeaseError::ExpiredLease { .. }
        | CapabilityLeaseError::ExhaustedLease { .. }
        | CapabilityLeaseError::UnclaimedFingerprintLease { .. }
        | CapabilityLeaseError::FingerprintMismatch { .. }
        | CapabilityLeaseError::InactiveLease { .. } => {
            denied("computer-use approval lease is no longer valid; re-request approval")
        }
        CapabilityLeaseError::Persistence { .. }
        | CapabilityLeaseError::VersionMismatch
        | CapabilityLeaseError::CasExhausted => Err(ironclaw_loop_host::raw_agent_loop_host_error(
            "computer_use",
            operation,
            AgentLoopHostErrorKind::Unavailable,
            "computer-use approval lease operation failed",
            error,
        )),
    }
}

/// Ends a thread's session when its turn is stopped or cancelled; tells the
/// server any other turn ended.
pub struct ComputerUseStopObserver {
    sessions: Arc<dyn ComputerUseSessions>,
}

pub fn stop_observer<S: ComputerUseSessions + 'static>(
    sessions: &Arc<S>,
) -> Arc<ComputerUseStopObserver> {
    let sessions: Arc<dyn ComputerUseSessions> = Arc::clone(sessions) as _;
    Arc::new(ComputerUseStopObserver { sessions })
}

#[async_trait]
impl ProcessJournalCommitObserver for ComputerUseStopObserver {
    fn process_observer_id(&self) -> &'static str {
        "computer-use-stop-observer-v1"
    }

    async fn observe_process_commit(&self, commit: ProcessJournalCommit) -> Result<(), String> {
        let state = &commit.state;
        if state.process_kind != ProcessKind::AgentTurn {
            return Ok(());
        }
        let Some(thread_id) = state.scope.thread_id.as_ref() else {
            return Ok(());
        };
        let tenant_id = &state.scope.tenant_id;
        match state.status {
            ProcessLifecycleStatus::StopRequested
            | ProcessLifecycleStatus::CancelRequested
            | ProcessLifecycleStatus::Stopped
            | ProcessLifecycleStatus::Cancelled
            | ProcessLifecycleStatus::Killed => {
                self.sessions.thread_stopped(tenant_id, thread_id).await;
            }
            status if status.is_terminal() => {
                self.sessions.turn_ended(tenant_id, thread_id).await;
            }
            _ => {}
        }
        Ok(())
    }
}

/// `ProductSurface` decorator answering the computer-use session view and
/// control command; everything else passes through. Wraps even when computer
/// use is off, so the route reports `enabled: false` instead of failing.
pub struct ComputerUseProductSurface {
    inner: Arc<dyn ProductSurface>,
    sessions: Option<Arc<dyn ComputerUseSessions>>,
}

pub fn wrap_surface<S: ComputerUseSessions + 'static>(
    inner: Arc<dyn ProductSurface>,
    sessions: Option<Arc<S>>,
) -> Arc<ComputerUseProductSurface> {
    Arc::new(ComputerUseProductSurface {
        inner,
        sessions: sessions.map(|sessions| sessions as Arc<dyn ComputerUseSessions>),
    })
}

fn session_key(
    caller: &ProductSurfaceCaller,
    thread_id: &str,
) -> Result<ComputerUseSession, ProductSurfaceError> {
    let thread_id = ThreadId::new(thread_id).map_err(|error| {
        tracing::debug!(%error, "computer-use thread id rejected");
        ProductSurfaceError::validation("thread_id", ProductSurfaceValidationCode::InvalidValue)
    })?;
    Ok(ComputerUseSession {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
        thread_id,
    })
}

/// A viewer id is the viewer's own random label: short and printable.
fn bounded_viewer_id(viewer: &str) -> Result<&str, ProductSurfaceError> {
    let ok = !viewer.is_empty()
        && viewer.len() <= 64
        && viewer
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(viewer)
    } else {
        Err(ProductSurfaceError::validation(
            "viewer",
            ProductSurfaceValidationCode::InvalidValue,
        ))
    }
}

fn decode<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ProductSurfaceError> {
    serde_json::from_value(input).map_err(|error| {
        tracing::debug!(%error, "computer-use request rejected");
        ProductSurfaceError::validation("body", ProductSurfaceValidationCode::InvalidValue)
    })
}

#[async_trait]
impl ProductSurface for ComputerUseProductSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        if request.operation_id.as_str() != COMPUTER_USE_CONTROL_COMMAND_ID {
            return self.inner.invoke(caller, request).await;
        }
        let control: ComputerUseControlRequest = decode(request.input)?;
        let output = match &self.sessions {
            None => json!({"ok": false, "enabled": false}),
            Some(sessions) => {
                let session = session_key(&caller, &control.thread_id)?;
                let viewer = control
                    .viewer
                    .as_deref()
                    .map(bounded_viewer_id)
                    .transpose()?;
                sessions.control(&session, control.action, viewer).await
            }
        };
        Ok(ProductSurfaceInvokeResponse { output })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        let item = if request.view_id == COMPUTER_USE_SESSION_VIEW.id {
            let selector: ComputerUseSessionRequest = decode(request.input)?;
            match &self.sessions {
                None => json!({"enabled": false}),
                Some(sessions) => sessions.session_view(
                    &session_key(&caller, &selector.thread_id)?,
                    selector.after_frame,
                ),
            }
        } else if request.view_id == COMPUTER_USE_STATE_VIEW.id {
            let selector: ComputerUseStateRequest = decode(request.input)?;
            let viewer = selector
                .viewer
                .as_deref()
                .map(bounded_viewer_id)
                .transpose()?;
            match &self.sessions {
                None => json!({"enabled": false, "sessions": []}),
                Some(sessions) => sessions.viewer_state(&caller.tenant_id, &caller.user_id, viewer),
            }
        } else if request.view_id == COMPUTER_USE_FRAME_VIEW.id {
            let selector: ComputerUseFrameRequest = decode(request.input)?;
            match &self.sessions {
                None => json!({"ok": false, "enabled": false}),
                Some(sessions) => {
                    let session = session_key(&caller, &selector.thread_id)?;
                    sessions.window_frame(&session, &selector).await
                }
            }
        } else {
            return self.inner.query(caller, request).await;
        };
        Ok(ProductSurfaceQueryPage {
            items: vec![item],
            next_cursor: None,
        })
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}

#[cfg(test)]
mod tests;
