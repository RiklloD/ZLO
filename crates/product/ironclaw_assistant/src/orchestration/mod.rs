//! Product-tier wiring of the `ironclaw_orchestrator` engine: IronClaw's
//! agent loop as the first orchestrator runtime, the product-surface
//! decorator that routes threads through the engine, the journal observer
//! that reports turn lifecycle, the per-model-call telemetry tap, and the `t3_*` model tools.

mod external;
#[cfg(test)]
mod external_tests;
mod inbox;
mod observer;
mod profiles;
mod runtime;
mod schedules;
mod surface;
mod telemetry;
mod tools;

pub use observer::OrchestratorProcessObserver;
pub use profiles::{
    ProfileHome, ProfileSupermemoryBindings, profile_identity_source, profile_surface_resolver,
};
pub use runtime::{AgentLoopRuntime, IRONCLAW_RUNTIME_ID};
pub use surface::OrchestratedProductSurface;
pub use telemetry::OrchestratorTelemetrySink;
pub use tools::orchestrator_capabilities;

use std::sync::Arc;

use ironclaw_cli_providers::bridge::CliBridge;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_product_contracts::surface::ProductSurface;

/// Route threads through `engine`: IronClaw's agent loop becomes its runtime
/// (acting on the undecorated `inner`, so automatic turns are not mistaken for
/// user activity), the tick loop starts, and the orchestrating decorator is
/// returned; `cli_bridge` streams CLI threads and answers their tool-call
/// approvals. Falls back to `inner` when the runtime cannot be built.
pub fn orchestrate(
    engine: &Arc<Engine>,
    inner: Arc<dyn ProductSurface>,
    session_extension_id: Option<String>,
    cli_bridge: Option<Arc<CliBridge>>,
) -> Arc<dyn ProductSurface> {
    match AgentLoopRuntime::new(Arc::clone(&inner), session_extension_id) {
        Ok(agent_loop) => engine.register_runtime(Arc::new(agent_loop)),
        Err(error) => {
            tracing::warn!(%error, "orchestrator runtime unavailable; threads run unorchestrated");
            return inner;
        }
    }
    engine.set_schedule_planner(Arc::new(schedules::RoutineSchedulePlanner::local()));
    engine.start(std::time::Duration::from_secs(2));
    Arc::new(OrchestratedProductSurface::new(
        inner,
        Arc::clone(engine),
        cli_bridge,
    ))
}
