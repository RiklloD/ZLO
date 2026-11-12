use super::*;

pub(super) async fn runtime_outcome_to_loop(
    run_context: &LoopRunContext,
    result_writer: &(dyn LoopCapabilityResultWriter + Send + Sync),
    conversion: RuntimeOutcomeConversion<'_>,
) -> Result<GatedResolution, AgentLoopHostError> {
    ensure_runtime_outcome_matches(conversion.requested_capability_id, &conversion.outcome)?;
    Ok(match conversion.outcome {
        RuntimeCapabilityOutcome::Completed(completed) => {
            let write_result = result_writer
                .write_capability_result(CapabilityResultWrite {
                    run_context,
                    input_ref: conversion.input_ref,
                    invocation_id: conversion.invocation_id,
                    capability_id: &completed.capability_id,
                    output: completed.output.clone(),
                    display_preview: completed.display_preview.clone(),
                    durable_persistence: DurablePersistence::Persist,
                })
                .await?;
            GatedResolution::bare(resolution::completed(
                write_result.result_ref,
                "capability completed".to_string(),
                ironclaw_loop_contracts::CapabilityProgress::MadeProgress,
                false,
                write_result.byte_len,
                write_result.output_digest,
                write_result.model_observation,
            ))
        }
        RuntimeCapabilityOutcome::ApprovalRequired(gate) => {
            // Raw input/estimate no longer ride the loop-facing resolution; the
            // host persists them in the replay-payload store at the fresh gate
            // raise (see `persist_replay_payload_for_fresh_gate`) and reconstitutes
            // them on resume (arch-simplification §5.3 Stage 2a-i).
            resolution::approval_required(
                loop_gate_ref("approval", gate.approval_request_id.to_string())?,
                blocked_summary(gate.reason).to_string(),
                Some(ironclaw_loop_contracts::CapabilityApprovalResume {
                    approval_request_id: gate.approval_request_id,
                    resume_token: resume_token_from_invocation_id(conversion.invocation_id)?,
                    correlation_id: conversion.correlation_id,
                    input_ref: conversion.input_ref.clone(),
                }),
            )
        }
        RuntimeCapabilityOutcome::AuthRequired(gate) => {
            let diagnostic = auth_gate_diagnostic(gate.provider_diagnostic());
            let gate_ref = auth_loop_gate_ref(gate.gate_id.as_ref(), diagnostic.as_ref())?;
            resolution::auth_required_with_diagnostic(
                gate_ref.clone(),
                gate.credential_requirements,
                blocked_summary(gate.reason).to_string(),
                Some(ironclaw_loop_contracts::CapabilityAuthResume::resolved(
                    gate_ref,
                    resume_token_from_invocation_id(conversion.invocation_id)?,
                    None,
                )),
                diagnostic,
            )
        }
        RuntimeCapabilityOutcome::ResourceBlocked(gate) => resolution::resource_blocked(
            loop_gate_ref("resource", gate.gate_id.to_string())?,
            blocked_summary(gate.reason).to_string(),
        ),
        RuntimeCapabilityOutcome::SpawnedProcess(process) => {
            GatedResolution::bare(resolution::spawned_process(
                LoopProcessRef::new(format!("process:{}", process.process_id)).map_err(|_| {
                    AgentLoopHostError::new(
                        AgentLoopHostErrorKind::Internal,
                        "process ref could not be represented",
                    )
                })?,
            ))
        }
        RuntimeCapabilityOutcome::Failed(failure) => {
            let capability_id = failure.capability_id.clone();
            let class = runtime_failure_to_loop(failure)?;
            // Surface actionable failure detail (e.g. invalid-input field issues)
            // to the per-tool UI preview by staging a display-preview record.
            // Without this the projection falls back to the bare error kind. The
            // model-visible observation is unaffected.
            if let LoopFailureClass::Failed {
                safe_summary,
                detail,
                ..
            } = &class
                && let Some(summary) = failure_display_summary(safe_summary, detail)
            {
                result_writer
                    .stage_capability_failure_preview(
                        run_context,
                        conversion.invocation_id,
                        &capability_id,
                        &summary,
                    )
                    .await;
            }
            GatedResolution::bare(class.into_resolution())
        }
    })
}

/// A runtime failure classified onto its loop channel — either a model-visible
/// recoverable failure or a terminal denial. Private to the seam: the failure
/// path needs the raw fields both to build the `Resolution` (via the producer
/// constructors) and to stage the per-tool display preview.
pub(super) enum LoopFailureClass {
    Failed {
        error_kind: FailureKind,
        safe_summary: String,
        detail: CapabilityFailureDetail,
    },
    Denied {
        reason_kind: CapabilityDeniedReasonKind,
        safe_summary: String,
    },
}

impl LoopFailureClass {
    pub(super) fn into_resolution(self) -> Resolution {
        match self {
            LoopFailureClass::Failed {
                error_kind,
                safe_summary,
                detail,
            } => resolution::failed(error_kind, safe_summary, detail),
            LoopFailureClass::Denied {
                reason_kind,
                safe_summary,
            } => resolution::denied(reason_kind, safe_summary).resolution,
        }
    }
}

pub(super) fn runtime_terminal_milestone(
    activity_id: CapabilityActivityId,
    provider: ExtensionId,
    runtime: RuntimeKind,
    outcome: &RuntimeCapabilityOutcome,
) -> Result<Option<LoopHostMilestoneKind>, AgentLoopHostError> {
    Ok(match outcome {
        RuntimeCapabilityOutcome::Completed(completed) => {
            Some(LoopHostMilestoneKind::CapabilityCompleted {
                activity_id,
                capability_id: completed.capability_id.clone(),
                provider,
                runtime,
                output_bytes: completed.usage.output_bytes,
            })
        }
        RuntimeCapabilityOutcome::Failed(failure) => {
            let safe_summary = runtime_failure_loop_safe_summary(failure);
            Some(LoopHostMilestoneKind::CapabilityFailed {
                activity_id,
                capability_id: failure.capability_id.clone(),
                provider: Some(provider),
                runtime: Some(runtime),
                reason_kind: failure.kind,
                // Sanitized, host-authored message (e.g. "invalid JSON: ...")
                // so the live per-tool UI card shows the real reason, not just
                // the bare error kind.
                safe_summary,
            })
        }
        RuntimeCapabilityOutcome::ApprovalRequired(_)
        | RuntimeCapabilityOutcome::AuthRequired(_)
        | RuntimeCapabilityOutcome::ResourceBlocked(_)
        | RuntimeCapabilityOutcome::SpawnedProcess(_) => None,
    })
}

pub(super) fn runtime_failure_to_loop(
    failure: RuntimeCapabilityFailure,
) -> Result<LoopFailureClass, AgentLoopHostError> {
    match failure.disposition() {
        CapabilityFailureDisposition::ModelVisibleToolError => {
            runtime_model_visible_failure_to_loop(failure)
        }
        CapabilityFailureDisposition::RetrySameCall => {
            let detail = runtime_failure_detail_to_loop(failure.detail.clone())
                .unwrap_or_else(|| runtime_failure_diagnostic_detail(&failure));
            Ok(LoopFailureClass::Failed {
                error_kind: failure.kind,
                safe_summary: runtime_failure_safe_summary(
                    &failure,
                    "capability invocation failed",
                ),
                detail,
            })
        }
    }
}

/// Build a model-visible, hardened diagnostic from a runtime failure's raw
/// message when the failure has no structured detail. Preserves the real cause
/// (paths, schema refs, codes) that the strict safe-summary validator drops,
/// while redacting secret VALUES through the full leak-detector registry +
/// prefix matcher and fencing any surviving injection payload
/// ([`crate::scrub_model_visible_detail`]).
pub(super) fn runtime_failure_diagnostic_detail(
    failure: &RuntimeCapabilityFailure,
) -> CapabilityFailureDetail {
    // Prefer the private in-process cause channel: the public `message` fails
    // closed (kind-only for wild raw causes), so the full descriptive cause
    // rides `model_visible_cause` and only becomes model-visible through this
    // scrub (full registry + injection fencing).
    let raw = failure
        .model_visible_cause()
        .map(str::to_owned)
        .or_else(|| failure.safe_summary());
    let text = raw
        .as_deref()
        .and_then(|raw| {
            if failure.kind == FailureKind::InputEncode
                && is_process_sandbox_capability(&failure.capability_id)
            {
                sandbox_model_visible_diagnostic_text(raw)
            } else {
                model_visible_diagnostic_text(raw)
            }
        })
        .unwrap_or_else(|| ModelDiagnostic::unavailable().into_inner());
    CapabilityFailureDetail::Diagnostic { text }
}

/// Sandbox validation diagnostics still cross the legacy host-api verdict
/// boundary as a `SafeSummary`. Apply the full secret scrub and injection fence
/// first, then normalize only the delimiters that boundary rejects. This keeps
/// corrective detail model-visible without allowing credentials or bare
/// instructions through, and preserves the previous 400-byte budget.
pub(super) fn sandbox_model_visible_diagnostic_text(raw: &str) -> Option<String> {
    pub(super) const MAX_BYTES: usize = 400;

    let scrubbed = crate::model_visible_scrub::scrub_model_visible_detail_compact(raw);
    let normalized: String = scrubbed
        .chars()
        .map(|character| match character {
            '`' => '\'',
            '{' | '}' | '[' | ']' | '<' | '>' | '/' | '\\' => ' ',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect();
    let mut text = normalized.trim().to_string();
    if text.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    if text.is_empty() { None } else { Some(text) }
}

/// Prepare free text for the model-visible diagnostic channel: scrub secret
/// values through the full registry and prefix matcher, fence surviving prompt
/// injection text, and replace control characters the model-observation
/// validator rejects (everything but `\n`, `\r`, `\t`) with spaces. This keeps
/// one stray escape byte from invalidating — and thereby dropping — the whole
/// observation.
pub(super) fn model_visible_diagnostic_text(raw: &str) -> Option<String> {
    let scrubbed = crate::scrub_model_visible_detail(raw);
    let normalized: String = scrubbed
        .chars()
        .map(|character| {
            if character == '\0'
                || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            {
                ' '
            } else {
                character
            }
        })
        .collect();
    if normalized.trim().is_empty() {
        return None;
    }
    Some(normalized)
}

/// Finalize the exact bounded bytes persisted in an auth gate record. Provider
/// text is untrusted: it crosses the same secret scrub and injection fence as
/// every other model-visible runtime diagnostic.
pub(super) fn auth_gate_diagnostic(
    diagnostic: Option<&ironclaw_host_api::dispatch::ProviderDiagnostic>,
) -> Option<ModelDiagnostic> {
    let text = model_visible_diagnostic_text(&provider_diagnostic_model_cause(diagnostic?)?)?;
    ModelDiagnostic::truncating(text).ok()
}

/// Derive the auth loop ref from the runtime's requirement fingerprint plus
/// the exact canonical diagnostic bytes stored in `GateRecord::Auth`.
/// `GateRecordStorePort` is write-once, so diagnostic presence/content must be
/// part of the key: an earlier generic auth gate must never alias a later
/// provider rejection and leave stale record content behind.
pub(super) fn auth_loop_gate_ref(
    runtime_gate_id: &str,
    diagnostic: Option<&ModelDiagnostic>,
) -> Result<LoopGateRef, AgentLoopHostError> {
    let diagnostic_token = match diagnostic {
        Some(diagnostic) => {
            let digest = sha256_digest_token(diagnostic.as_str().as_bytes());
            digest
                .strip_prefix("sha256:")
                .unwrap_or(&digest)
                .to_string()
        }
        None => "none".to_string(),
    };
    loop_gate_ref(
        "auth",
        format!("{runtime_gate_id}-diagnostic-{diagnostic_token}"),
    )
}

pub(super) fn diagnostic_detail_from_raw(raw: &str) -> CapabilityFailureDetail {
    let text = model_visible_diagnostic_text(raw)
        .unwrap_or_else(|| ModelDiagnostic::unavailable().into_inner());
    CapabilityFailureDetail::Diagnostic { text }
}

pub(super) fn runtime_model_visible_failure_to_loop(
    failure: RuntimeCapabilityFailure,
) -> Result<LoopFailureClass, AgentLoopHostError> {
    if matches!(
        failure.kind,
        FailureKind::Authorization | FailureKind::PolicyDenied
    ) {
        return Ok(LoopFailureClass::Denied {
            reason_kind: denied_reason_kind_for(failure.kind)?,
            safe_summary: runtime_failure_safe_summary(&failure, "capability authorization denied"),
        });
    }

    let error_kind = failure.kind;
    let safe_summary = runtime_failure_safe_summary(&failure, "capability invocation failed");
    let detail = runtime_failure_detail_to_loop(failure.detail.clone())
        .unwrap_or_else(|| runtime_failure_diagnostic_detail(&failure));
    Ok(LoopFailureClass::Failed {
        error_kind,
        safe_summary,
        detail,
    })
}

pub(super) fn runtime_failure_detail_to_loop(
    detail: Option<DispatchFailureDetail>,
) -> Option<CapabilityFailureDetail> {
    detail.and_then(dispatch_failure_detail_to_loop)
}

pub(super) fn dispatch_failure_detail_to_loop(
    detail: DispatchFailureDetail,
) -> Option<CapabilityFailureDetail> {
    match detail {
        DispatchFailureDetail::InvalidInput { issues } => {
            Some(CapabilityFailureDetail::InvalidInput {
                issues: issues
                    .into_iter()
                    .map(dispatch_input_issue_to_loop)
                    .collect(),
            })
        }
        // Raw failure cause the host runtime preserved because the strict
        // safe-summary validator rejected it (paths, newlines). Scrub secret
        // values and normalize control characters before the model sees it.
        DispatchFailureDetail::Diagnostic { text } => model_visible_diagnostic_text(&text)
            .map(|text| CapabilityFailureDetail::Diagnostic { text }),
        // Host-authored remediation: already validated at construction (bounded,
        // newline-only control characters, credential-VALUE shapes rejected), so
        // it passes through verbatim. Running it through
        // `model_visible_diagnostic_text` would be a no-op at best and a
        // vocabulary scrub at worst — the text NAMES config keys on purpose.
        DispatchFailureDetail::HostRemediation { text } => {
            Some(CapabilityFailureDetail::HostRemediation { text })
        }
        // Host-authored public label — already surfaced as the failure's
        // `safe_summary`; no separate model-visible representation, so
        // dropping it here avoids duplicating the same text onto the
        // diagnostic detail seam.
        DispatchFailureDetail::HostSummary { .. } => None,
    }
}

pub(super) fn dispatch_input_issue_to_loop(issue: DispatchInputIssue) -> CapabilityInputIssue {
    CapabilityInputIssue {
        path: issue.path,
        code: issue.code,
        expected: issue.expected,
        received: issue.received,
        schema_path: issue.schema_path,
    }
}

pub(super) fn runtime_failed_outcome_for_host_runtime_unavailable(
    capability_id: CapabilityId,
    reason: String,
) -> RuntimeCapabilityOutcome {
    let host_error = host_runtime_error(HostRuntimeError::Unavailable { reason });
    RuntimeCapabilityOutcome::Failed(RuntimeCapabilityFailure::new(
        capability_id,
        FailureKind::Unavailable,
        Some(host_error.safe_summary),
    ))
}

pub(super) fn ensure_runtime_outcome_matches(
    expected: &CapabilityId,
    outcome: &RuntimeCapabilityOutcome,
) -> Result<(), AgentLoopHostError> {
    let actual = match outcome {
        RuntimeCapabilityOutcome::Completed(completed) => &completed.capability_id,
        RuntimeCapabilityOutcome::ApprovalRequired(gate) => &gate.capability_id,
        RuntimeCapabilityOutcome::AuthRequired(gate) => &gate.capability_id,
        RuntimeCapabilityOutcome::ResourceBlocked(gate) => &gate.capability_id,
        RuntimeCapabilityOutcome::SpawnedProcess(process) => &process.capability_id,
        RuntimeCapabilityOutcome::Failed(failure) => &failure.capability_id,
    };
    if actual != expected {
        return Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            "host runtime returned outcome for a different capability",
        ));
    }
    Ok(())
}

/// Maps an authorization/policy runtime failure to a leak-safe denied reason
/// identifier.
///
/// `FailureKind::Authorization.as_str()` is the literal string
/// `"authorization"`, which the loop-safe identifier validator rejects as a
/// sensitive marker (it guards against leaking `Authorization:` header
/// material into identifiers). Passing it straight into
/// `capability_denied_reason_kind` therefore turned every authorization denial
/// into an internal "could not be represented" error, which the executor
/// mapped to `HostUnavailable` and the planned driver recorded as a terminal
/// "driver unavailable" failure — borking the whole run (observed when a Gmail
/// extension activation failed authorization on auth-resume). Use stable,
/// non-leaky tags so the denial surfaces to the model as a clean `Denied`
/// outcome instead.
pub(super) fn denied_reason_kind_for(
    kind: FailureKind,
) -> Result<CapabilityDeniedReasonKind, AgentLoopHostError> {
    let reason = match kind {
        FailureKind::Authorization => "auth_denied",
        FailureKind::PolicyDenied => "policy_denied",
        other => other.as_str(),
    };
    capability_denied_reason_kind(reason)
}

pub(super) fn capability_denied_reason_kind(
    value: impl Into<String>,
) -> Result<CapabilityDeniedReasonKind, AgentLoopHostError> {
    CapabilityDeniedReasonKind::unknown(value).map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            "capability denied reason kind could not be represented",
        )
    })
}

pub(super) fn runtime_safe_summary(message: Option<String>, fallback: &'static str) -> String {
    message
        .and_then(|summary| LoopSafeSummary::new(summary).ok())
        .map(|summary| summary.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) fn runtime_failure_safe_summary(
    failure: &RuntimeCapabilityFailure,
    fallback: &'static str,
) -> String {
    let fallback = runtime_failure_fallback_summary(failure.kind, fallback);
    failure
        .safe_summary()
        .and_then(|summary| LoopSafeSummary::new(summary).ok())
        .map(|summary| summary.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) fn runtime_failure_loop_safe_summary(
    failure: &RuntimeCapabilityFailure,
) -> Option<LoopSafeSummary> {
    match failure.safe_summary() {
        Some(summary) => {
            if let Ok(summary) = LoopSafeSummary::new(summary.clone()) {
                return Some(summary);
            }
            if matches!(failure.kind, FailureKind::InputEncode) {
                return Some(runtime_input_encode_summary());
            }
            Some(LoopSafeSummary::capability_failure_summary(summary))
        }
        None if matches!(failure.kind, FailureKind::InputEncode) => {
            Some(runtime_input_encode_summary())
        }
        None => None,
    }
}

pub(super) fn runtime_failure_fallback_summary(
    kind: FailureKind,
    fallback: &'static str,
) -> &'static str {
    if matches!(kind, FailureKind::InputEncode) {
        RuntimeDispatchErrorKind::InputEncode.human_summary()
    } else {
        fallback
    }
}

pub(super) fn runtime_input_encode_summary() -> LoopSafeSummary {
    LoopSafeSummary::tool_input_could_not_be_encoded()
}

pub(super) fn loop_gate_ref(kind: &str, id: String) -> Result<LoopGateRef, AgentLoopHostError> {
    LoopGateRef::new(format!("gate:{kind}-{id}")).map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            "capability gate ref could not be represented",
        )
    })
}

pub(super) fn blocked_summary(reason: RuntimeBlockedReason) -> &'static str {
    match reason {
        RuntimeBlockedReason::ApprovalRequired => "capability requires approval",
        RuntimeBlockedReason::AuthRequired => "capability requires authentication",
        RuntimeBlockedReason::ResourceLimit => "capability is blocked by resource limits",
        RuntimeBlockedReason::ResourceUnavailable => "capability resources are unavailable",
    }
}

pub(super) fn resume_token_from_invocation_id(
    invocation_id: InvocationId,
) -> Result<CapabilityResumeToken, AgentLoopHostError> {
    CapabilityResumeToken::new(invocation_id.to_string()).map_err(|reason| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            format!("capability resume token is invalid: {reason}"),
        )
    })
}

pub(super) fn invocation_id_from_resume_token(
    resume_token: &CapabilityResumeToken,
) -> Result<InvocationId, AgentLoopHostError> {
    InvocationId::parse(resume_token.as_str()).map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "capability approval resume token is invalid",
        )
    })
}

pub(super) fn ensure_resume_invocation_matches_activity(
    resume_invocation_id: InvocationId,
    requested_invocation_id: InvocationId,
    resume_kind: &'static str,
) -> Result<(), AgentLoopHostError> {
    if resume_invocation_id == requested_invocation_id {
        return Ok(());
    }
    Err(AgentLoopHostError::new(
        AgentLoopHostErrorKind::InvalidInvocation,
        format!("capability {resume_kind} resume activity identity does not match resume token"),
    ))
}

pub(super) fn host_runtime_error(error: HostRuntimeError) -> AgentLoopHostError {
    match error {
        HostRuntimeError::InvalidRequest { reason } => crate::raw_agent_loop_host_error(
            "host_runtime_capability",
            "invoke",
            AgentLoopHostErrorKind::InvalidInvocation,
            runtime_safe_summary(
                Some(reason.clone()),
                "host runtime rejected capability request",
            ),
            reason,
        ),
        HostRuntimeError::Unavailable { reason } => crate::raw_agent_loop_host_error(
            "host_runtime_capability",
            "invoke",
            AgentLoopHostErrorKind::Unavailable,
            runtime_safe_summary(
                Some(reason.clone()),
                "host runtime capability service is unavailable",
            ),
            reason,
        ),
    }
}
