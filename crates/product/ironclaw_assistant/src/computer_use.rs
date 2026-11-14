//! Computer use as product glue over the
//! [`ComputerUseSessions`] port (implemented by `ironclaw_computer_use`,
//! which this crate does not name):
//!
//! - every server tool as a synthetic loop capability (`computer_use.<tool>`),
//!   declared mutating: the shared synthetic approval gate raises a durable
//!   approval request unless the user chose "always allow" for that tool or
//!   the thread runs on Full access. A tool setting of `Disabled` denies,
//!   `AskEachTime` always asks, and global auto-approve is ignored: it never
//!   hands the desktop to the model on its own. One approval covers the rest
//!   of its task. An allowed call reaches the thread's session through the
//!   port;
//! - the thread Stop hook: a turn that is stopped or cancelled ends the
//!   thread's session (kills the process), any other turn end sends the
//!   server its turn-ended notification;
//! - the `computer_use.session` view and `computer_use.control` command
//!   (Stop, take over, hand back) behind the WebUI routes;
//! - the operator-only `computer_use.readiness` view: this host's engine and
//!   macOS permissions, from the [`ComputerUseReadinessProbe`] port, so a
//!   client on another computer never checks its own.
//!

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::{
    ids::{InvocationId, ThreadId, UserId},
    resolution::Resolution,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityProgress, ModelVisibleToolObservation,
    ToolObservationDetail, resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticApproval, SyntheticCapability,
    SyntheticCapabilityDescriptor, SyntheticCapabilityEffect, SyntheticCapabilityHandler,
    SyntheticCapabilityInvocation,
};
use ironclaw_processes::{
    ProcessJournalCommit, ProcessJournalCommitObserver, ProcessKind, ProcessLifecycleStatus,
};
use ironclaw_product_contracts::computer_use::{
    COMPUTER_USE_CAPABILITY_PREFIX, COMPUTER_USE_CONTROL_COMMAND_ID, COMPUTER_USE_FRAME_VIEW,
    COMPUTER_USE_READINESS_VIEW, COMPUTER_USE_RESULT_FRAME, COMPUTER_USE_SESSION_VIEW,
    COMPUTER_USE_STATE_VIEW, COMPUTER_USE_TOOL_NAME_PREFIX, ComputerUseControlRequest,
    ComputerUseFrameRequest, ComputerUseReadiness, ComputerUseReadinessProbe,
    ComputerUseReadinessRequest, ComputerUseSession, ComputerUseSessionRequest,
    ComputerUseSessions, ComputerUseStateRequest, ComputerUseToolSpec,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};
use serde_json::{Value, json};

/// Fixed, interpolation-free gate text, shared by every computer-use tool so
/// one approval covers the task's later calls of any of them.
const APPROVAL_SUMMARY: &str = "computer use requires approval";

/// One approval-gated capability per server tool.
pub fn computer_use_capabilities<S: ComputerUseSessions + 'static>(
    sessions: Arc<S>,
    fallback_user_id: UserId,
) -> Result<Vec<SyntheticCapability>, AgentLoopHostError> {
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
                    SyntheticCapabilityEffect::Mutating(
                        SyntheticApproval::new(
                            APPROVAL_SUMMARY,
                            format!(
                                "Computer use for this task, starting with {}. Approving lets \
                                 the agent use this Mac's apps until it answers.",
                                tool.name
                            ),
                        )
                        .ignoring_global_auto_approve()
                        .lasting_for_task(),
                    ),
                )?,
                Arc::new(ToolHandler {
                    tool: tool.clone(),
                    sessions: Arc::clone(&sessions),
                    fallback_user_id: fallback_user_id.clone(),
                }),
            ))
        })
        .collect()
}

struct ToolHandler {
    tool: ComputerUseToolSpec,
    sessions: Arc<dyn ComputerUseSessions>,
    fallback_user_id: UserId,
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

fn host_error(kind: AgentLoopHostErrorKind, message: impl Into<String>) -> AgentLoopHostError {
    AgentLoopHostError::new(kind, message)
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
    readiness: Arc<dyn ComputerUseReadinessProbe>,
}

pub fn wrap_surface<S: ComputerUseSessions + 'static>(
    inner: Arc<dyn ProductSurface>,
    sessions: Option<Arc<S>>,
    readiness: Arc<dyn ComputerUseReadinessProbe>,
) -> Arc<ComputerUseProductSurface> {
    Arc::new(ComputerUseProductSurface {
        inner,
        sessions: sessions.map(|sessions| sessions as Arc<dyn ComputerUseSessions>),
        readiness,
    })
}

/// The readiness view item: the probe's answer plus `ready`.
fn readiness_item(readiness: &ComputerUseReadiness) -> Result<Value, ProductSurfaceError> {
    let mut item = serde_json::to_value(readiness).map_err(ProductSurfaceError::internal_from)?;
    if let Some(fields) = item.as_object_mut() {
        fields.insert("ready".to_string(), Value::Bool(readiness.ready()));
    }
    Ok(item)
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
        } else if request.view_id == COMPUTER_USE_READINESS_VIEW.id {
            // Host permissions are deployment facts: operators only.
            if !caller.operator_config {
                return Err(ProductSurfaceError::from_status(
                    ironclaw_product_contracts::surface::ProductSurfaceErrorCode::Forbidden,
                    403,
                    false,
                ));
            }
            let _: ComputerUseReadinessRequest = decode(request.input)?;
            readiness_item(&self.readiness.readiness())?
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

pub mod cli_bridge;

#[cfg(test)]
mod tests;
