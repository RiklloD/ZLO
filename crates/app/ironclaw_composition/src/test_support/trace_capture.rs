//! Trace-capture `TurnEventSink` test support (C-TRACECAP seam).

/// Build the SAME `TraceCaptureTurnEventSink` production fans into
/// `DefaultPlannedRuntimeParts.turn_event_sink` inside `build_reborn_runtime`
/// (`runtime.rs`, "Autonomous Trace Commons capture"), seeded with the
/// runtime owner's tenant-scoped trace scope key — the identical
/// `trace_scope_key(tenant, owner)` recipe, so the test path never drifts
/// from production wiring. Capture stays policy-gated per scope (inert until
/// the scope enrolls), exactly as in production.
///
/// Returns the sink alongside the EXACT scope key it was seeded with, so the
/// caller has a single source of truth instead of recomputing
/// `trace_scope_key(tenant_id, actor_user_id)` a second time (which risks the
/// two call sites drifting if either recipe ever changes independently).
#[cfg(feature = "test-support")]
pub fn trace_capture_turn_event_sink_for_test(
    thread_service: std::sync::Arc<dyn ironclaw_threads::SessionThreadService>,
    tenant_id: &str,
    actor_user_id: &str,
) -> (std::sync::Arc<dyn ironclaw_turns::TurnEventSink>, String) {
    let scope = ironclaw_trace_commons::contribution::trace_scope_key(tenant_id, actor_user_id);
    let observed_scopes: ironclaw_trace_commons::capture::ObservedTraceScopes =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::from([
            scope.clone(),
        ])));
    let sink = std::sync::Arc::new(
        ironclaw_turn_runner::trace_capture::TraceCaptureTurnEventSink::new(
            thread_service,
            observed_scopes,
        ),
    );
    (sink, scope)
}

/// Build the SAME operator-metrics `TurnEventSink` production fans into the
/// runtime's turn-event sink inside `build_reborn_runtime`, recording into
/// the caller's registry instead of the process-wide one.
#[cfg(feature = "test-support")]
pub fn operator_metrics_turn_event_sink_for_test(
    metrics: std::sync::Arc<ironclaw_operator::OperatorMetrics>,
) -> std::sync::Arc<dyn ironclaw_turns::TurnEventSink> {
    std::sync::Arc::new(
        crate::observability::operator_metrics::OperatorMetricsTurnEventSink::new(metrics),
    )
}

/// Production's runtime event sink (`metered_runtime_event_sink`, the helper
/// both production backend assemblies call), recording into the caller's
/// registry instead of the process-wide one.
#[cfg(feature = "test-support")]
pub fn operator_metered_runtime_event_sink_for_test(
    log: std::sync::Arc<dyn ironclaw_event_log::DurableEventLog>,
    config: ironclaw_event_store::EventBatchConfig,
    metrics: std::sync::Arc<ironclaw_operator::OperatorMetrics>,
) -> std::sync::Arc<dyn ironclaw_event_log::NonBlockingEventSink> {
    crate::observability::operator_metrics::metered_runtime_event_sink(log, config, metrics)
}

/// Production's outermost provider decorator (`metered_model_provider`, the
/// helper `wrap_swappable_gateway` calls), recording into the caller's
/// registry instead of the process-wide one.
#[cfg(feature = "test-support")]
pub fn operator_metered_model_provider_for_test(
    provider: std::sync::Arc<dyn ironclaw_llm::LlmProvider>,
    metrics: std::sync::Arc<ironclaw_operator::OperatorMetrics>,
) -> std::sync::Arc<dyn ironclaw_llm::LlmProvider> {
    crate::observability::operator_metrics::metered_model_provider(provider, metrics)
}
