//! Reborn integration suite — Chat and turn lifecycle, model recovery, prompts, suggestions, and sub-agents.
//!
//! One test binary for this domain's flat (single-thread) integration
//! scenarios. Each member keeps its own file under `tests/integration/` and is
//! mounted here as a module, so the scenarios compile the shared support trees
//! and link once instead of once per file. Test names gain the member module
//! as a prefix (`<member>::<test>`); select one file's tests with
//! `cargo test --test reborn_integration_suite_turns <member>::`. See
//! `tests/integration/AGENTS.md` §"Suite binaries".

#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../../support/mod.rs"]
mod support;

#[path = "../budget.rs"]
mod budget;
#[path = "../cancel.rs"]
mod cancel;
#[path = "../comm_context.rs"]
mod comm_context;
#[path = "../golden_payload.rs"]
mod golden_payload;
#[path = "../greeting.rs"]
mod greeting;
#[path = "../idempotent_replay.rs"]
mod idempotent_replay;
#[path = "../model_recovery.rs"]
mod model_recovery;
#[path = "../prompt_prefix_stability.rs"]
mod prompt_prefix_stability;
#[path = "../run_artifact_timings.rs"]
mod run_artifact_timings;
#[path = "../safety.rs"]
mod safety;
#[path = "../steering.rs"]
mod steering;
#[path = "../subagent_await_edge.rs"]
mod subagent_await_edge;
#[path = "../suggestions.rs"]
mod suggestions;
#[path = "../terminal_warning.rs"]
mod terminal_warning;
#[path = "../triggered_submit.rs"]
mod triggered_submit;
#[path = "../unattended_surface.rs"]
mod unattended_surface;
#[path = "../unbound_turns.rs"]
mod unbound_turns;
