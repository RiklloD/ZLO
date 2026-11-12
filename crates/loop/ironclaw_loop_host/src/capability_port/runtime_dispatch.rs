use super::*;

pub(super) async fn dispatch_runtime_capability(
    runtime: &(dyn HostRuntime + Send + Sync),
    context: ExecutionContext,
    capability_id: CapabilityId,
    estimate: ResourceEstimate,
    input: serde_json::Value,
) -> Result<RuntimeCapabilityOutcome, HostRuntimeError> {
    if is_process_sandbox_capability(&capability_id) {
        runtime
            .spawn_capability((context, capability_id, estimate, input))
            .await
    } else {
        runtime
            .invoke_capability((context, capability_id, estimate, input))
            .await
    }
}

pub(super) async fn dispatch_runtime_capability_resume(
    runtime: &(dyn HostRuntime + Send + Sync),
    context: ExecutionContext,
    approval_request_id: ApprovalRequestId,
    capability_id: CapabilityId,
    estimate: ResourceEstimate,
    input: serde_json::Value,
) -> Result<RuntimeCapabilityOutcome, HostRuntimeError> {
    if is_process_sandbox_capability(&capability_id) {
        runtime
            .resume_spawn_capability((context, approval_request_id, capability_id, estimate, input))
            .await
    } else {
        runtime
            .resume_capability((context, approval_request_id, capability_id, estimate, input))
            .await
    }
}

/// Auth-resume dispatch: always uses `auth_resume_capability` (no spawn
/// variant; sandbox spawns do not go through approval/auth gates).
pub(super) async fn dispatch_runtime_capability_auth_resume(
    runtime: &(dyn HostRuntime + Send + Sync),
    context: ExecutionContext,
    capability_id: CapabilityId,
    estimate: ResourceEstimate,
    input: serde_json::Value,
    approval_request_id: Option<ApprovalRequestId>,
) -> Result<RuntimeCapabilityOutcome, HostRuntimeError> {
    runtime
        .auth_resume_capability((context, capability_id, estimate, input, approval_request_id))
        .await
}

pub(super) async fn dispatch_runtime_capability_auth_decline(
    runtime: &(dyn HostRuntime + Send + Sync),
    context: ExecutionContext,
    capability_id: CapabilityId,
) -> Result<RuntimeCapabilityOutcome, HostRuntimeError> {
    runtime
        .decline_auth_capability((context, capability_id))
        .await
}
