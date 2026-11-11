//! Fail-closed startup guards run by the runtime assembly: the live-traffic
//! cutover gate, the production scheduler-wake wiring check, and runtime
//! identity validation.

use super::*;

/// Gate live-traffic startup on the deployment's [`TrafficPolicy`].
///
/// §4.4: this used to be a seven-arm `match` on the composition profile, with
/// each arm spelling out its own readiness precondition. The precondition is
/// now data on the config — a required readiness state plus an optional
/// production-blocking-diagnostic veto — so this reads one value. The profile
/// still appears in the error text, as a label for the operator.
pub(super) fn enforce_runtime_cutover_gate(
    deployment: &DeploymentConfig,
    readiness: &RebornReadiness,
) -> Result<(), RebornRuntimeError> {
    let profile = deployment.profile();
    let traffic = deployment.traffic();
    if let Some(reason) = traffic.live_traffic_refusal(profile) {
        return Err(RebornRuntimeError::InvalidArgument { reason });
    }
    if let TrafficPolicy::Serve {
        required_readiness,
        veto_on_production_blocking_diagnostic,
    } = traffic
    {
        if readiness.state != required_readiness {
            return Err(RebornRuntimeError::InvalidArgument {
                reason: format!(
                    "profile={profile} cannot start Reborn runtime before readiness is validated; required_state={required_readiness:?}, state={:?}",
                    readiness.state
                ),
            });
        }
        if veto_on_production_blocking_diagnostic
            && let Some(diagnostic) = readiness
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.blocks_production)
        {
            return Err(RebornRuntimeError::InvalidArgument {
                reason: format!(
                    "profile={profile} cannot start Reborn runtime while readiness diagnostic blocks production: component={:?}, reason={:?}",
                    diagnostic.component, diagnostic.reason
                ),
            });
        }
    }
    Ok(())
}

/// Guard: production and migration-dry-run compositions always pre-mint
/// [`SchedulerWakeWiring`] in `build_production_shaped` so the
/// `HostRuntimeServices` notifier and the scheduler wake loop share exactly one
/// channel. If the wiring is `None` for those profiles it means the composition
/// contract was violated (e.g. a code path forgot to mint it), and starting the
/// runtime would silently create a divergent scheduler-local channel. Extracted
/// so the negative branch is unit-testable without a full libsql/postgres
/// substrate.
pub(super) fn check_production_scheduler_wake_wiring(
    profile: RebornCompositionProfile,
    wiring: &Option<ironclaw_turn_runner::runtime::SchedulerWakeWiring>,
) -> Result<(), RebornRuntimeError> {
    if wiring.is_none()
        && DeploymentConfig::for_profile(profile, false).requires_pre_minted_scheduler_wake()
    {
        return Err(RebornRuntimeError::InvalidArgument {
            reason: "production runtime missing scheduler wake wiring".to_string(),
        });
    }
    Ok(())
}

pub(super) struct ValidatedRuntimeIdentity {
    pub(super) tenant_id: TenantId,
    pub(super) agent_id: AgentId,
    pub(super) source_binding_ref: SourceBindingRef,
    pub(super) reply_target_binding_ref: ReplyTargetBindingRef,
}

pub(super) fn validate_runtime_identity(
    identity: RebornRuntimeIdentity,
) -> Result<ValidatedRuntimeIdentity, RebornRuntimeError> {
    let tenant_id = TenantId::new(identity.tenant_id).map_err(|reason| {
        RebornRuntimeError::InvalidArgument {
            reason: format!("tenant id: {reason}"),
        }
    })?;
    let agent_id =
        AgentId::new(identity.agent_id).map_err(|reason| RebornRuntimeError::InvalidArgument {
            reason: format!("agent id: {reason}"),
        })?;
    let source_binding_ref =
        SourceBindingRef::new(identity.source_binding_id).map_err(|reason| {
            RebornRuntimeError::InvalidArgument {
                reason: format!("source binding id: {reason}"),
            }
        })?;
    let reply_target_binding_ref = ReplyTargetBindingRef::new(identity.reply_target_binding_id)
        .map_err(|reason| RebornRuntimeError::InvalidArgument {
            reason: format!("reply target binding id: {reason}"),
        })?;
    Ok(ValidatedRuntimeIdentity {
        tenant_id,
        agent_id,
        source_binding_ref,
        reply_target_binding_ref,
    })
}
