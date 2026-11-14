//! `builtin.notification_channels_set` synthetic capability — the second
//! capability `outbound_delivery::outbound_delivery_capabilities` wires, split
//! out of the sibling `outbound_delivery` module to keep both files under the
//! repo's file-size budget (`.claude/rules/architecture.md` §5: a `.rs` file
//! over 1,500 lines needs either an owning tracking issue or a decomposition;
//! this is the decomposition).
//!
//! `outbound_delivery` and this module are SIBLING children of `capability_host`
//! (not ancestor/descendant), so — unlike `ironclaw_assistant`'s
//! `outbound_preferences::notification_channels` split, which reaches its
//! parent's private items for free through Rust's ancestor-visibility rule —
//! every helper and type this module shares with `outbound_delivery` had to be
//! widened to `pub(super)` on that side (visible to their common parent
//! `capability_host` and all its descendants, which includes this module). Those
//! shared items (`ApprovedDispatchLease`, `ApprovedResumeDecision`,
//! `OutboundDeliveryApprovalSettingsDecision`, `write_completed_result`) still
//! live in `outbound_delivery.rs` and carry their own doc-comments; only the
//! visibility changed, not the behavior.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_assistant::OutboundPreferencesProductService;
use ironclaw_host_api::{ids::UserId, resolution::Resolution};
use ironclaw_loop_contracts::{AgentLoopHostError, AgentLoopHostErrorKind};

use super::outbound_delivery::{
    OutboundPreferenceOperation, caller_for_run, input_error, outbound_delivery_outcome,
    write_completed_result,
};
use ironclaw_assistant::{
    OUTBOUND_NOTIFICATION_CHANNELS_SET_CAPABILITY_ID,
    OUTBOUND_NOTIFICATION_CHANNELS_SET_DESCRIPTION,
    OUTBOUND_NOTIFICATION_CHANNELS_SET_PROVIDER_TOOL_NAME, notification_channels_set_input_schema,
    parse_notification_channels_set_input, set_notification_channels_for_model,
};
use ironclaw_loop_host::{
    SyntheticApproval, SyntheticCapability, SyntheticCapabilityDescriptor,
    SyntheticCapabilityEffect, SyntheticCapabilityHandler, SyntheticCapabilityInvocation,
};

/// Host-authored, model-independent summary for the notification-channels
/// approval gate: the persisted gate record the approver renders and the
/// loop-facing outcome share this one string, and it interpolates nothing
/// caller-controlled (a target id may legally carry `/ < >`, which would trip
/// the safe-summary validator and kill the run).
const NOTIFICATION_CHANNELS_APPROVAL_GATE_SUMMARY: &str =
    "changing the notification channel list requires approval";

/// Applies the notification channel list. Its approval is the shared synthetic
/// gate's (the capability is declared mutating), so by the time `invoke` runs
/// the call is allowed.
pub(super) struct NotificationChannelsSetHandler {
    pub(super) service: Arc<dyn OutboundPreferencesProductService>,
    pub(super) fallback_user_id: UserId,
}

#[async_trait]
impl SyntheticCapabilityHandler for NotificationChannelsSetHandler {
    fn validate_provider_arguments(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<(), AgentLoopHostError> {
        parse_notification_channels_set_input(arguments)
            .map(|_| ())
            .map_err(input_error)
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let channels_input =
            parse_notification_channels_set_input(&invocation.input).map_err(input_error)?;
        let caller = caller_for_run(&invocation, &self.fallback_user_id);
        let response = match set_notification_channels_for_model(
            self.service.as_ref(),
            caller,
            channels_input,
        )
        .await
        {
            Ok(response) => response,
            // See `outbound_delivery_outcome`: recoverable service errors are
            // model-visible failures, not terminal host errors.
            Err(error) => {
                return outbound_delivery_outcome(
                    OutboundPreferenceOperation::SetNotificationChannels,
                    error,
                );
            }
        };
        // The safe summary stays fixed and delimiter-free: only a `usize`
        // count is interpolated, never a raw target id (a target id may legally
        // carry `/ < >`, which would trip the loop's safe-summary validator and
        // kill the run). The model still gets every applied channel from the
        // result `output`.
        let channel_count = response.channels.len();
        let output = serde_json::to_value(response).map_err(|error| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                format!("notification channels set output serialization failed: {error}"),
            )
        })?;
        write_completed_result(
            invocation,
            output,
            format!("set {channel_count} notification channel(s)"),
        )
        .await
    }
}

/// Build the `builtin.notification_channels_set` synthetic capability.
pub(super) fn notification_channels_set_capability(
    handler: NotificationChannelsSetHandler,
) -> Result<SyntheticCapability, AgentLoopHostError> {
    Ok(SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            OUTBOUND_NOTIFICATION_CHANNELS_SET_CAPABILITY_ID,
            OUTBOUND_NOTIFICATION_CHANNELS_SET_PROVIDER_TOOL_NAME,
            OUTBOUND_NOTIFICATION_CHANNELS_SET_DESCRIPTION,
            notification_channels_set_input_schema(),
            SyntheticCapabilityEffect::Mutating(SyntheticApproval::with_input_reason(
                NOTIFICATION_CHANNELS_APPROVAL_GATE_SUMMARY,
                |input| {
                    let targets = parse_notification_channels_set_input(input)
                        .map_or(0, |channels| channels.target_ids().len());
                    format!("Set the notification channel list ({targets} target(s))")
                },
            )),
        )?,
        Arc::new(handler),
    ))
}

#[cfg(test)]
mod tests {
    use ironclaw_loop_contracts::LoopSafeSummary;

    #[test]
    fn notification_channels_set_summary_is_fixed_and_count_only() {
        // Same invariant as `outbound_delivery`'s
        // `set_delivery_target_summary_is_fixed_and_validator_safe`: the
        // completion summary must never interpolate a raw, model-controlled
        // target id (which may legally contain a forbidden delimiter). The
        // notification-channels handler interpolates only a `usize` count,
        // which can never carry a delimiter, so every count must validate.
        for channel_count in [0usize, 1, 8] {
            LoopSafeSummary::new(format!("set {channel_count} notification channel(s)"))
                .unwrap_or_else(|reason| {
                    panic!("count-only summary must satisfy the loop validator: {reason}")
                });
        }
    }
}
