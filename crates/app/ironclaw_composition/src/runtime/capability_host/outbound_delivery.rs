//! Loop-facing outbound-delivery capabilities.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_assistant::OutboundPreferencesProductService;
use ironclaw_host_api::{
    ids::{InvocationId, UserId},
    resolution::Resolution,
    result_meta::FailureKind,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityDeniedReasonKind, CapabilityInputRef,
    CapabilityProgress, resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticCapability, SyntheticCapabilityDescriptor,
    SyntheticCapabilityEffect, SyntheticCapabilityHandler, SyntheticCapabilityInvocation,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode,
};

use crate::runtime::capability_host::notification_channels_set::{
    NotificationChannelsSetHandler, notification_channels_set_capability,
};
use ironclaw_assistant::{
    OUTBOUND_DELIVERY_TARGETS_LIST_CAPABILITY_ID, OUTBOUND_DELIVERY_TARGETS_LIST_DESCRIPTION,
    OUTBOUND_DELIVERY_TARGETS_LIST_PROVIDER_TOOL_NAME, OutboundDeliveryCapabilityInputError,
    list_outbound_delivery_targets_for_model, outbound_delivery_targets_list_input_schema,
    parse_outbound_delivery_targets_list_input,
};

/// The outbound-preference synthetics. The setter is declared mutating, so the
/// shared synthetic approval gate decides each of its calls.
pub(super) fn outbound_delivery_capabilities(
    service: Arc<dyn OutboundPreferencesProductService>,
    fallback_user_id: UserId,
) -> Result<Vec<SyntheticCapability>, AgentLoopHostError> {
    Ok(vec![
        SyntheticCapability::new(
            SyntheticCapabilityDescriptor::new(
                OUTBOUND_DELIVERY_TARGETS_LIST_CAPABILITY_ID,
                OUTBOUND_DELIVERY_TARGETS_LIST_PROVIDER_TOOL_NAME,
                OUTBOUND_DELIVERY_TARGETS_LIST_DESCRIPTION,
                outbound_delivery_targets_list_input_schema(),
                SyntheticCapabilityEffect::ReadOnly,
            )?,
            Arc::new(OutboundDeliveryTargetsListHandler {
                service: Arc::clone(&service),
                fallback_user_id: fallback_user_id.clone(),
            }),
        ),
        notification_channels_set_capability(NotificationChannelsSetHandler {
            service,
            fallback_user_id,
        })?,
    ])
}

struct OutboundDeliveryTargetsListHandler {
    service: Arc<dyn OutboundPreferencesProductService>,
    fallback_user_id: UserId,
}

#[async_trait]
impl SyntheticCapabilityHandler for OutboundDeliveryTargetsListHandler {
    fn validate_provider_arguments(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<(), AgentLoopHostError> {
        parse_outbound_delivery_targets_list_input(arguments)
            .map(|_| ())
            .map_err(input_error)
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let input =
            parse_outbound_delivery_targets_list_input(&invocation.input).map_err(input_error)?;
        let caller = caller_for_run(&invocation, &self.fallback_user_id);
        let response =
            match list_outbound_delivery_targets_for_model(self.service.as_ref(), caller, input)
                .await
            {
                Ok(response) => response,
                // A model-recoverable service failure (invalid request, not
                // found, denied, conflict, rate limit, transient unavailability)
                // must surface as a model-visible tool error so the run continues
                // and the model can adapt — NOT a terminal
                // `Err(AgentLoopHostError)`, which `ironclaw_agent_loop`'s executor
                // maps to a run-ending `HostUnavailable { stage: Capability }`.
                // Only a genuine internal bug stays terminal. See
                // `outbound_delivery_outcome`.
                Err(error) => {
                    return outbound_delivery_outcome(
                        OutboundPreferenceOperation::ListTargets,
                        error,
                    );
                }
            };
        let count = response.targets.len();
        let output = serde_json::to_value(response).map_err(|error| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                format!("outbound delivery target list output serialization failed: {error}"),
            )
        })?;
        write_completed_result(
            invocation,
            output,
            format!("found {count} delivery target(s)"),
        )
        .await
    }
}

pub(super) async fn write_completed_result(
    invocation: SyntheticCapabilityInvocation,
    output: serde_json::Value,
    safe_summary: String,
) -> Result<Resolution, AgentLoopHostError> {
    let write_result = invocation
        .result_writer
        .write_capability_result(CapabilityResultWrite {
            run_context: &invocation.run_context,
            input_ref: invocation_effective_input_ref(&invocation),
            invocation_id: InvocationId::new(),
            capability_id: &invocation.request.capability_id,
            output,
            display_preview: None,
            durable_persistence: DurablePersistence::Persist,
        })
        .await?;
    Ok(resolution::completed(
        write_result.result_ref,
        safe_summary,
        CapabilityProgress::MadeProgress,
        false,
        write_result.byte_len,
        write_result.output_digest,
        write_result.model_observation,
    ))
}

fn invocation_effective_input_ref(
    invocation: &SyntheticCapabilityInvocation,
) -> &CapabilityInputRef {
    invocation
        .request
        .approval_resume
        .as_ref()
        .map(|resume| &resume.input_ref)
        .unwrap_or(&invocation.request.input_ref)
}

/// The caller these capabilities read and write outbound state AS.
///
/// This answers an AUTHORIZATION question — whose delivery targets may this
/// call see, whose notification channels may it rewrite — so it follows the
/// run's user. Since the ephemeral-per-ping remodel a run's owner IS its
/// actor, so there is a single identity to follow.
///
/// The same identity scopes the whole approval-gate dance
/// ([`LoopRunContext::acting_resource_scope`] via [`resource_scope_for_run`],
/// plus [`settings_scope_for_run`]): every store the dance touches is
/// scope-keyed, and a raise and its resume must derive the same scope or the
/// approved capability strands. That raise/resume coverage is
/// `notification_channels_set_approval_raise_and_resume_stay_scope_matched`.
///
/// `pub(super)`: called from the sibling `notification_channels_set` module.
pub(super) fn caller_for_run(
    invocation: &SyntheticCapabilityInvocation,
    fallback_user_id: &UserId,
) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        invocation.run_context.scope.tenant_id.clone(),
        invocation.run_context.acting_user_id(fallback_user_id),
        invocation.run_context.scope.agent_id.clone(),
        invocation.run_context.scope.project_id.clone(),
    )
}

/// `pub(super)`: called from the sibling `notification_channels_set` module.
pub(super) fn input_error(error: OutboundDeliveryCapabilityInputError) -> AgentLoopHostError {
    AgentLoopHostError::new(AgentLoopHostErrorKind::InvalidInvocation, error.to_string())
}

/// Disposition an outbound-delivery service failure into either a model-visible,
/// recoverable capability outcome or a terminal host error.
///
/// As with `project_service_outcome` and `skill_activation_selection_outcome`,
/// the two arms map onto the executor's two failure paths
/// (`ironclaw_agent_loop::executor::mapping`): `CapabilityOutcome::Failed` /
/// `Denied` is handed back to the model and the run continues (so the model can
/// fix its request or tell the user), while `Err(AgentLoopHostError)` becomes a
/// run-ending `HostUnavailable { stage: Capability }`. Only a genuine internal
/// bug stays terminal — invalid input, not-found, denials, conflicts, rate
/// limits, and transient unavailability are all surfaced to the model instead of
/// killing the turn.
///
/// Safe summaries stay fixed and host-authored: `ProductSurfaceError` carries a
/// free-form `field` that could contain a forbidden delimiter/marker and remap a
/// recoverable arm into a terminal `HostUnavailable` (Invariant 2).
///
/// `pub(super)`: called from the sibling `notification_channels_set` module.
#[derive(Clone, Copy)]
pub(super) enum OutboundPreferenceOperation {
    ListTargets,
    SetNotificationChannels,
}

impl OutboundPreferenceOperation {
    fn invalid_request_summary(self) -> &'static str {
        match self {
            Self::ListTargets => "invalid outbound delivery request",
            Self::SetNotificationChannels => "invalid notification channel request",
        }
    }

    fn denied_summary(self) -> &'static str {
        match self {
            // Read-shaped: this arm serves the targets LIST call, which never
            // writes. A write-verb denial ("change the delivery target") both
            // misdescribes the refusal and names a retired concept.
            Self::ListTargets => "not permitted to list outbound delivery targets",
            Self::SetNotificationChannels => "not permitted to change notification channels",
        }
    }

    fn conflict_summary(self) -> &'static str {
        match self {
            Self::ListTargets => "outbound delivery target operation conflicted",
            Self::SetNotificationChannels => "notification channel operation conflicted",
        }
    }

    fn rate_limited_summary(self) -> &'static str {
        match self {
            Self::ListTargets => "outbound delivery target operation rate limited",
            Self::SetNotificationChannels => "notification channel operation rate limited",
        }
    }

    fn unavailable_summary(self) -> &'static str {
        match self {
            Self::ListTargets => "outbound delivery service temporarily unavailable",
            Self::SetNotificationChannels => "notification channel service temporarily unavailable",
        }
    }

    fn internal_summary(self) -> &'static str {
        match self {
            Self::ListTargets => "outbound delivery target operation failed",
            Self::SetNotificationChannels => "notification channel operation failed",
        }
    }
}

pub(super) fn outbound_delivery_outcome(
    operation: OutboundPreferenceOperation,
    error: ProductSurfaceError,
) -> Result<Resolution, AgentLoopHostError> {
    match error.code {
        ProductSurfaceErrorCode::InvalidRequest | ProductSurfaceErrorCode::NotFound => {
            Ok(resolution::failed(
                FailureKind::InputEncode,
                operation.invalid_request_summary().to_string(),
                ironclaw_loop_contracts::CapabilityFailureDetail::Diagnostic {
                    text: String::new(),
                },
            ))
        }
        ProductSurfaceErrorCode::Unauthenticated | ProductSurfaceErrorCode::Forbidden => {
            approval_denied(operation.denied_summary())
        }
        ProductSurfaceErrorCode::Conflict => Ok(resolution::failed(
            FailureKind::OperationFailed,
            operation.conflict_summary().to_string(),
            ironclaw_loop_contracts::CapabilityFailureDetail::Diagnostic {
                text: String::new(),
            },
        )),
        ProductSurfaceErrorCode::RateLimited => Ok(resolution::failed(
            FailureKind::Resource,
            operation.rate_limited_summary().to_string(),
            ironclaw_loop_contracts::CapabilityFailureDetail::Diagnostic {
                text: String::new(),
            },
        )),
        ProductSurfaceErrorCode::Unavailable => Ok(resolution::failed(
            FailureKind::Unavailable,
            operation.unavailable_summary().to_string(),
            ironclaw_loop_contracts::CapabilityFailureDetail::Diagnostic {
                text: String::new(),
            },
        )),
        ProductSurfaceErrorCode::Internal => Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            operation.internal_summary(),
        )),
    }
}

/// Build a model-visible denial `Resolution` with a fixed, host-authored summary.
/// The reason kind is a charset-safe identifier, so it never trips
/// safe-summary/identifier validation.
///
/// `pub(super)`: called from the sibling `notification_channels_set` module.
pub(super) fn approval_denied(safe_summary: &str) -> Result<Resolution, AgentLoopHostError> {
    let reason_kind = CapabilityDeniedReasonKind::unknown("outbound_delivery_approval_required")
        .map_err(|reason| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                format!("outbound delivery denial reason kind is invalid: {reason}"),
            )
        })?;
    Ok(resolution::denied(reason_kind, safe_summary.to_string()).resolution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::capability_host::assert_recoverable_failure;
    use ironclaw_loop_contracts::LoopSafeSummary;
    use ironclaw_product_contracts::surface::ProductSurfaceErrorKind;

    fn service_error(code: ProductSurfaceErrorCode) -> ProductSurfaceError {
        ProductSurfaceError {
            code,
            kind: ProductSurfaceErrorKind::Internal,
            status_code: 500,
            retryable: false,
            // A free-form `field` carrying a forbidden delimiter is the exact
            // shape that, if interpolated into a safe summary, would remap a
            // recoverable arm into a terminal `HostUnavailable` (Invariant 2).
            field: Some("slack/<channel>".to_string()),
            validation_code: None,
        }
    }

    /// The model-visible summary carried on a recoverable failure / denial.
    fn recoverable_summary(resolution: &Resolution) -> String {
        match resolution {
            Resolution::Done(outcome) => outcome.summary.as_str().to_string(),
            Resolution::Denied(denial) => denial
                .summary
                .as_ref()
                .map(|summary| summary.as_str().to_string())
                .unwrap_or_default(),
            other => panic!("expected a recoverable outcome, got {other:?}"),
        }
    }

    #[test]
    fn invalid_request_is_a_recoverable_tool_failure_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::InvalidRequest),
        )
        .expect("invalid request must be a model-visible failure, not terminal");
        assert_recoverable_failure(
            &outcome,
            ironclaw_host_api::result_meta::FailureKind::InputEncode,
        );
        LoopSafeSummary::new(recoverable_summary(&outcome))
            .expect("safe summary must satisfy the loop validator");
    }

    #[test]
    fn notification_channel_failure_names_the_operation_the_model_can_correct() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::SetNotificationChannels,
            service_error(ProductSurfaceErrorCode::InvalidRequest),
        )
        .expect("notification-channel validation stays model-visible");
        assert_eq!(
            recoverable_summary(&outcome),
            "invalid notification channel request"
        );
    }

    #[test]
    fn not_found_is_a_recoverable_tool_failure_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::NotFound),
        )
        .expect("not found must be a model-visible failure, not terminal");
        assert_recoverable_failure(
            &outcome,
            ironclaw_host_api::result_meta::FailureKind::InputEncode,
        );
    }

    #[test]
    fn unauthenticated_is_a_recoverable_denial_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::Unauthenticated),
        )
        .expect("unauthenticated must be a model-visible denial, not terminal");
        assert!(matches!(outcome, Resolution::Denied(_)));
        LoopSafeSummary::new(recoverable_summary(&outcome))
            .expect("safe summary must satisfy the loop validator");
    }

    #[test]
    fn forbidden_is_a_recoverable_denial_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::Forbidden),
        )
        .expect("forbidden must be a model-visible denial, not terminal");
        assert!(matches!(outcome, Resolution::Denied(_)));
    }

    #[test]
    fn conflict_is_a_recoverable_tool_failure_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::Conflict),
        )
        .expect("conflict must be a model-visible failure, not terminal");
        assert_recoverable_failure(
            &outcome,
            ironclaw_host_api::result_meta::FailureKind::OperationFailed,
        );
    }

    #[test]
    fn rate_limited_is_a_recoverable_tool_failure_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::RateLimited),
        )
        .expect("rate limited must be a model-visible failure, not terminal");
        assert_recoverable_failure(
            &outcome,
            ironclaw_host_api::result_meta::FailureKind::Resource,
        );
    }

    #[test]
    fn unavailable_is_a_recoverable_tool_failure_not_terminal() {
        let outcome = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::Unavailable),
        )
        .expect("transient unavailability must not kill the run");
        assert_recoverable_failure(
            &outcome,
            ironclaw_host_api::result_meta::FailureKind::Unavailable,
        );
    }

    #[test]
    fn internal_service_error_stays_terminal() {
        let error = outbound_delivery_outcome(
            OutboundPreferenceOperation::ListTargets,
            service_error(ProductSurfaceErrorCode::Internal),
        )
        .expect_err("internal bugs must stay terminal");

        assert_eq!(error.kind, AgentLoopHostErrorKind::Internal);
    }

    #[test]
    fn service_error_field_with_delimiter_does_not_reach_safe_summary() {
        // A `field` carrying a `/ < >` delimiter must never poison the fixed,
        // host-authored safe summary. Each recoverable outcome's summary must
        // still pass the loop safe-summary validator that fires at
        // `append_capability_result_ref` (the terminal-failure boundary).
        for code in [
            ProductSurfaceErrorCode::InvalidRequest,
            ProductSurfaceErrorCode::NotFound,
            ProductSurfaceErrorCode::Unauthenticated,
            ProductSurfaceErrorCode::Forbidden,
            ProductSurfaceErrorCode::Conflict,
            ProductSurfaceErrorCode::RateLimited,
            ProductSurfaceErrorCode::Unavailable,
        ] {
            let outcome = outbound_delivery_outcome(
                OutboundPreferenceOperation::ListTargets,
                service_error(code),
            )
            .unwrap_or_else(|_| panic!("{code:?} must be recoverable"));
            let summary = recoverable_summary(&outcome);
            assert!(
                !summary.contains("slack/<channel>"),
                "summary must not interpolate the service error field: {summary}"
            );
            LoopSafeSummary::new(summary).unwrap_or_else(|reason| {
                panic!("{code:?} summary must satisfy the loop validator: {reason}")
            });
        }
    }

    /// Both denial surfaces must name the operation the caller actually
    /// attempted. The list arm is read-only, so a write verb ("change the
    /// delivery target") misdescribes the refusal; and only the
    /// notification-channel write takes a lease, so a fixed "outbound delivery
    /// target" lease sentence names a retired concept on the one production
    /// path that reaches it.
    #[test]
    fn denial_summaries_name_the_attempted_operation() {
        assert_eq!(
            OutboundPreferenceOperation::ListTargets.denied_summary(),
            "not permitted to list outbound delivery targets"
        );
        assert_eq!(
            OutboundPreferenceOperation::SetNotificationChannels.denied_summary(),
            "not permitted to change notification channels"
        );

        for summary in [
            OutboundPreferenceOperation::ListTargets.denied_summary(),
            OutboundPreferenceOperation::SetNotificationChannels.denied_summary(),
        ] {
            LoopSafeSummary::new(summary.to_string())
                .expect("every denial summary must satisfy the loop validator");
        }
    }

    #[test]
    fn parse_outbound_delivery_targets_list_input_rejects_empty_channel() {
        let error =
            parse_outbound_delivery_targets_list_input(&serde_json::json!({"channel": "  "}))
                .expect_err("empty channel should fail");

        assert!(error.to_string().contains("must be a non-empty string"));
    }

    #[test]
    fn parse_outbound_delivery_targets_list_input_rejects_non_object_input() {
        let error = parse_outbound_delivery_targets_list_input(&serde_json::Value::Null)
            .expect_err("non-object input should fail");

        assert!(error.to_string().contains("input must be an object"));
    }

    #[test]
    fn parse_outbound_delivery_targets_list_input_rejects_unknown_fields() {
        let error =
            parse_outbound_delivery_targets_list_input(&serde_json::json!({"unexpected": "value"}))
                .expect_err("unknown fields should fail");

        assert!(error.to_string().contains("unsupported field `unexpected`"));
    }

    /// A run acts as the user it belongs to: the authorization identity AND the
    /// approval-gate/lease/settings scopes all follow that one user. Owner ==
    /// actor since the ephemeral-per-ping remodel, so there is no owner-vs-actor
    /// split left to pin (the retired raise/resume-under-owner-≠-actor dance is
    /// gone); an actorless host run falls back to the thread owner, then to the
    /// configured host fallback.
    #[test]
    fn authorization_and_approval_scopes_all_follow_the_run_user() {
        let user = UserId::new("user-participant").expect("user id");
        let fallback = UserId::new("user-fallback").expect("fallback id");
        let scope = ironclaw_turns::TurnScope::new_with_owner(
            ironclaw_host_api::ids::TenantId::new("tenant-shared").expect("tenant"),
            None,
            None,
            ironclaw_host_api::ids::ThreadId::new("thread-shared").expect("thread"),
            Some(user.clone()),
        );

        let with_actor = run_context_for_test(scope.clone(), Some(user.clone()));
        assert_eq!(
            with_actor.acting_user_id(&fallback),
            user,
            "a run acts as its user"
        );
        assert_eq!(
            with_actor.acting_resource_scope(&fallback).user_id,
            user,
            "the approval gate, its records and settings reads are scoped to the run user"
        );

        // A host-initiated run carries no actor: fall back to the owner, not to
        // the configured fallback identity.
        let actorless = run_context_for_test(scope, None);
        assert_eq!(actorless.acting_user_id(&fallback), user);
        assert_eq!(
            actorless.acting_resource_scope(&fallback).user_id,
            user,
            "an actorless run's gate stays scoped to the thread owner"
        );
    }

    #[allow(clippy::items_after_test_module)]
    fn run_context_for_test(
        scope: ironclaw_turns::TurnScope,
        actor: Option<UserId>,
    ) -> ironclaw_loop_contracts::LoopRunContext {
        use ironclaw_loop_contracts::RunProfileResolver as _;
        let resolved = futures::executor::block_on(
            ironclaw_loop_contracts::InMemoryRunProfileResolver::default().resolve_run_profile(
                ironclaw_loop_contracts::RunProfileResolutionRequest::interactive_default(),
            ),
        )
        .expect("profile resolves");
        let mut run_context = ironclaw_loop_contracts::LoopRunContext::new(
            scope,
            ironclaw_turns::TurnId::new(),
            ironclaw_turns::TurnRunId::new(),
            resolved,
        );
        if let Some(actor) = actor {
            run_context = run_context.with_actor(ironclaw_turns::TurnActor::new(actor));
        }
        run_context
    }
}
