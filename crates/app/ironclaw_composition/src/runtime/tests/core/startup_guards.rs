//! Startup guards: the live-traffic cutover gate and production scheduler-wake
//! wiring.

use super::*;

fn readiness_for_runtime_gate(
    profile: RebornCompositionProfile,
    state: RebornReadinessState,
    diagnostics: Vec<crate::RebornReadinessDiagnostic>,
) -> RebornReadiness {
    RebornReadiness {
        profile,
        state,
        services: crate::RebornServiceReadiness {
            host_runtime: true,
            turn_coordinator: true,
            product_auth: true,
        },
        workers: crate::RebornWorkerReadiness {
            turn_runner: true,
            trigger_poller: false,
        },
        diagnostics,
    }
}

/// Drive the cutover gate the way production does: build the deployment from
/// the profile, then gate on its `TrafficPolicy`.
fn cutover_gate(
    profile: RebornCompositionProfile,
    readiness: &crate::RebornReadiness,
) -> Result<(), RebornRuntimeError> {
    crate::runtime::startup_guards::enforce_runtime_cutover_gate(
        &crate::deployment::DeploymentConfig::for_profile(profile, false),
        readiness,
    )
}

#[test]
fn runtime_cutover_gate_allows_validated_production_readiness() {
    let readiness = readiness_for_runtime_gate(
        RebornCompositionProfile::Production,
        RebornReadinessState::ProductionValidated,
        Vec::new(),
    );

    cutover_gate(RebornCompositionProfile::Production, &readiness)
        .expect("validated production runtime can start");
}

#[test]
fn runtime_cutover_gate_rejects_blocking_production_diagnostic() {
    let readiness = readiness_for_runtime_gate(
        RebornCompositionProfile::Production,
        RebornReadinessState::ProductionValidated,
        vec![
            crate::RebornReadinessDiagnostic::production_blocker(
                RebornCompositionProfile::Production,
                crate::RebornReadinessDiagnosticComponent::RuntimePolicy,
                crate::RebornReadinessDiagnosticReason::LocalOnly,
            )
            .expect("production profile should create a blocker"),
        ],
    );

    let error = cutover_gate(RebornCompositionProfile::Production, &readiness)
        .expect_err("blocking production diagnostic prevents runtime start");
    let RebornRuntimeError::InvalidArgument { reason } = error else {
        panic!("expected invalid argument, got {error:?}");
    };
    assert!(reason.contains("RuntimePolicy"), "reason: {reason}");
    assert!(reason.contains("LocalOnly"), "reason: {reason}");
}

#[test]
fn runtime_cutover_gate_rejects_migration_dry_run_runtime_start() {
    let readiness = readiness_for_runtime_gate(
        RebornCompositionProfile::MigrationDryRun,
        RebornReadinessState::MigrationDryRunValidated,
        Vec::new(),
    );

    let error = cutover_gate(RebornCompositionProfile::MigrationDryRun, &readiness)
        .expect_err("migration-dry-run cannot start live runtime");
    let RebornRuntimeError::InvalidArgument { reason } = error else {
        panic!("expected invalid argument, got {error:?}");
    };
    assert!(reason.contains("migration-dry-run"), "reason: {reason}");
}

#[test]
fn runtime_cutover_gate_allows_standalone_readiness() {
    let readiness = readiness_for_runtime_gate(
        RebornCompositionProfile::Standalone,
        RebornReadinessState::DevOnly,
        vec![crate::RebornReadinessDiagnostic::standalone()],
    );

    cutover_gate(RebornCompositionProfile::Standalone, &readiness)
        .expect("standalone runtime is not production traffic");
}

#[test]
fn runtime_cutover_gate_allows_hosted_single_tenant_readiness() {
    let readiness = readiness_for_runtime_gate(
        RebornCompositionProfile::HostedSingleTenant,
        RebornReadinessState::HostedSingleTenantValidated,
        Vec::new(),
    );

    cutover_gate(RebornCompositionProfile::HostedSingleTenant, &readiness)
        .expect("validated hosted single-tenant runtime can start");
}

#[test]
fn runtime_cutover_gate_rejects_standalone_readiness_for_hosted_single_tenant() {
    let readiness = readiness_for_runtime_gate(
        RebornCompositionProfile::HostedSingleTenant,
        RebornReadinessState::DevOnly,
        vec![crate::RebornReadinessDiagnostic::standalone()],
    );

    let error = cutover_gate(RebornCompositionProfile::HostedSingleTenant, &readiness)
        .expect_err("hosted single-tenant runtime requires hosted readiness");
    let RebornRuntimeError::InvalidArgument { reason } = error else {
        panic!("expected invalid argument, got {error:?}");
    };
    assert!(reason.contains("hosted-single-tenant"), "reason: {reason}");
    assert!(
        reason.contains("HostedSingleTenantValidated"),
        "reason: {reason}"
    );
}

// ── scheduler wake wiring guard unit tests ───────────────────────────────
// These exercise `check_production_scheduler_wake_wiring` directly so the
// fail-closed negative branch is covered without needing a full libsql /
// postgres substrate.  The guard is gated on the same `libsql | postgres`
// cfg as the production composition path it protects.

#[test]
fn production_scheduler_wake_guard_rejects_production_with_absent_wiring() {
    let err = crate::runtime::startup_guards::check_production_scheduler_wake_wiring(
        RebornCompositionProfile::Production,
        &None,
    )
    .expect_err(
        "production runtime with absent scheduler wake wiring must be rejected fail-closed",
    );
    let RebornRuntimeError::InvalidArgument { reason } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        reason.contains("production runtime missing scheduler wake wiring"),
        "reason should name the missing wiring, got: {reason}"
    );
}

#[test]
fn production_scheduler_wake_guard_rejects_migration_dry_run_with_absent_wiring() {
    let err = crate::runtime::startup_guards::check_production_scheduler_wake_wiring(
        RebornCompositionProfile::MigrationDryRun,
        &None,
    )
    .expect_err("migration-dry-run with absent scheduler wake wiring must be rejected fail-closed");
    let RebornRuntimeError::InvalidArgument { reason } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        reason.contains("production runtime missing scheduler wake wiring"),
        "reason should name the missing wiring, got: {reason}"
    );
}

#[test]
fn production_scheduler_wake_guard_passes_standalone_with_absent_wiring() {
    // Standalone never mints scheduler wake wiring; the guard must not
    // reject it (the scheduler loop mints its own channel on that path).
    crate::runtime::startup_guards::check_production_scheduler_wake_wiring(
        RebornCompositionProfile::Standalone,
        &None,
    )
    .expect("standalone is exempt from the scheduler wake wiring requirement");
}
