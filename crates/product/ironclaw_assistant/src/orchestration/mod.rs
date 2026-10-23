//! Product-tier wiring of the `ironclaw_orchestrator` engine: IronClaw's
//! agent loop as the first orchestrator runtime, the product-surface
//! decorator that routes threads through the engine, the journal observer
//! that reports turn lifecycle, and the `t3_*` model tools.

mod observer;
mod profiles;
mod runtime;
mod surface;
mod tools;

pub use observer::OrchestratorProcessObserver;
pub use profiles::{ProfileHome, profile_identity_source, profile_surface_resolver};
pub use runtime::{AgentLoopRuntime, IRONCLAW_RUNTIME_ID};
pub use surface::OrchestratedProductSurface;
pub use tools::orchestrator_capabilities;

use std::sync::Arc;

use ironclaw_orchestrator::engine::Engine;
use ironclaw_product_contracts::surface::ProductSurface;

/// Route threads through `engine`: IronClaw's agent loop becomes its runtime
/// (acting on the undecorated `inner`, so automatic turns are not mistaken for
/// user activity), the tick loop starts, and the orchestrating decorator is
/// returned. Falls back to `inner` when the runtime cannot be built.
pub fn orchestrate(
    engine: &Arc<Engine>,
    inner: Arc<dyn ProductSurface>,
    session_extension_id: Option<String>,
) -> Arc<dyn ProductSurface> {
    match AgentLoopRuntime::new(Arc::clone(&inner), session_extension_id) {
        Ok(agent_loop) => engine.register_runtime(Arc::new(agent_loop)),
        Err(error) => {
            tracing::warn!(%error, "orchestrator runtime unavailable; threads run unorchestrated");
            return inner;
        }
    }
    engine.start(std::time::Duration::from_secs(2));
    Arc::new(OrchestratedProductSurface::new(inner, Arc::clone(engine)))
}
