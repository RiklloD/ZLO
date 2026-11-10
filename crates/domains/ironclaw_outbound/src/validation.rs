use crate::NOTIFICATION_TARGETS_CAP;
use crate::{
    CommunicationPreferenceRecord, DeliveryDefaultScope, DeliveryFailureKind,
    OutboundDeliveryAttempt, OutboundDeliveryStatus, OutboundError, UpdateDeliveryStatusRequest,
};

pub(crate) fn validate_delivery_attempt(
    attempt: &OutboundDeliveryAttempt,
) -> Result<(), OutboundError> {
    validate_delivery_scope_candidate(&attempt.scope, &attempt.candidate)?;
    validate_delivery_status(attempt.status, attempt.failure_kind)
}

pub(crate) fn validate_delivery_scope_candidate(
    scope: &ironclaw_host_api::turn::TurnScope,
    candidate: &crate::OutboundPushCandidate,
) -> Result<(), OutboundError> {
    if scope.tenant_id != candidate.tenant_id
        || scope.agent_id != candidate.agent_id
        || scope.project_id != candidate.project_id
        || scope.thread_id != candidate.thread_id
    {
        return Err(OutboundError::InvalidRequest {
            reason: "delivery candidate scope does not match request scope",
        });
    }
    Ok(())
}

pub(crate) fn validate_delivery_status_request(
    request: &UpdateDeliveryStatusRequest,
) -> Result<(), OutboundError> {
    validate_delivery_status(request.status, request.failure_kind)
}

fn validate_delivery_status(
    status: OutboundDeliveryStatus,
    failure_kind: Option<DeliveryFailureKind>,
) -> Result<(), OutboundError> {
    match (status, failure_kind) {
        // In-flight coordinator states and `Unknown` (outcome ambiguous, not
        // a known failure) never carry a failure kind.
        (
            OutboundDeliveryStatus::Prepared
            | OutboundDeliveryStatus::Sending
            | OutboundDeliveryStatus::Pending
            | OutboundDeliveryStatus::NoTarget
            | OutboundDeliveryStatus::Delivered
            | OutboundDeliveryStatus::Unknown,
            None,
        ) => Ok(()),
        (OutboundDeliveryStatus::Failed | OutboundDeliveryStatus::DeadLettered, Some(_)) => Ok(()),
        (
            OutboundDeliveryStatus::Prepared
            | OutboundDeliveryStatus::Sending
            | OutboundDeliveryStatus::Pending
            | OutboundDeliveryStatus::NoTarget
            | OutboundDeliveryStatus::Delivered
            | OutboundDeliveryStatus::Unknown,
            Some(_),
        ) => Err(OutboundError::InvalidRequest {
            reason: "non-failed delivery statuses must not include failure kind",
        }),
        (OutboundDeliveryStatus::Failed | OutboundDeliveryStatus::DeadLettered, None) => {
            Err(OutboundError::InvalidRequest {
                reason: "failed delivery statuses require failure kind",
            })
        }
    }
}

pub(crate) fn validate_delivery_identity(
    existing: &OutboundDeliveryAttempt,
    incoming: &OutboundDeliveryAttempt,
) -> Result<(), OutboundError> {
    // `attempted_at` records the first reservation and is not part of the
    // idempotency identity: a replay after reopen necessarily arrives at a
    // later wall-clock time but must resolve to the same durable attempt.
    if existing.delivery_id != incoming.delivery_id
        || existing.scope != incoming.scope
        || existing.candidate != incoming.candidate
    {
        return Err(OutboundError::Backend);
    }
    Ok(())
}

pub(crate) fn validate_communication_preference(
    record: &CommunicationPreferenceRecord,
) -> Result<(), OutboundError> {
    if record.notification_targets.len() > NOTIFICATION_TARGETS_CAP {
        return Err(OutboundError::InvalidRequest {
            reason: "communication preference has too many notification targets",
        });
    }
    match &record.scope {
        DeliveryDefaultScope::Personal { tenant_id, user_id } => {
            if tenant_id.as_str().is_empty() {
                return Err(OutboundError::InvalidRequest {
                    reason: "communication preference tenant is required",
                });
            }
            if user_id.as_str().is_empty() {
                return Err(OutboundError::InvalidRequest {
                    reason: "communication preference user is required",
                });
            }
        }
        DeliveryDefaultScope::SharedAgent {
            tenant_id,
            agent_id,
            project_id,
        } => {
            if tenant_id.as_str().is_empty() {
                return Err(OutboundError::InvalidRequest {
                    reason: "communication preference tenant is required",
                });
            }
            if agent_id.as_str().is_empty() {
                return Err(OutboundError::InvalidRequest {
                    reason: "communication preference shared agent is required",
                });
            }
            if project_id
                .as_ref()
                .is_some_and(|project_id| project_id.as_str().is_empty())
            {
                return Err(OutboundError::InvalidRequest {
                    reason: "communication preference project is required when present",
                });
            }
        }
    }
    if record.updated_by.as_str().is_empty() {
        return Err(OutboundError::InvalidRequest {
            reason: "communication preference updater is required",
        });
    }
    Ok(())
}
