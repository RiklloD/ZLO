//! IronClaw orchestrator: the engine that owns every orchestrated thread.
//!
//! Rust port of the t3code Hermes plugin's orchestrator v2 (T3 Code's
//! orchestration model):
//!
//! - **Delegation tree** — `t3_delegate` creates a visible child thread with a
//!   prompt-only brief; results come back to the parent as a new turn when
//!   it is idle. Depth limit 3, at most 8 live children per parent.
//! - **Durable queue and admission** — per-thread follow-ups that survive
//!   restarts and drain in order when the thread is idle and not held.
//! - **Stop** — a user Stop holds the thread and cancels every descendant
//!   task, grandchildren of finished children included.
//! - **Fork and merge-back** — native forks where the runtime has them,
//!   otherwise a bounded portable context handoff; merge-back sends the
//!   fork's delta after the fork point.
//! - **Recovery** — restart continuation for turns a backend restart cut,
//!   and automatic resume after a provider usage limit resets.
//!
//! Turns run on [`runtime::TurnRuntime`] implementations; IronClaw's agent
//! loop is the first one (wired in the product tier), CLI drivers follow.
//! State lives in `orch_*` tables of the deployment's libSQL file
//! ([`store::Store`]).

pub mod cli_approvals;
pub mod delegation;
pub mod engine;
pub mod error;
pub mod goals;
pub mod handoff;
pub mod inbox;
mod migrate;
pub mod model;
pub mod profile_runtime;
pub mod profiles;
mod recovery;
pub mod restart;
pub mod runtime;
pub mod schedule_webhooks;
pub mod schedules;
pub mod store;
pub mod telemetry;
mod thread_ops;
pub mod tools;

#[cfg(test)]
mod tests;
