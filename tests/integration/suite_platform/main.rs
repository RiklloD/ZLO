//! Reborn integration suite — Durability, storage, platform wiring, and WebUI/operator surfaces.
//!
//! One test binary for this domain's flat (single-thread) integration
//! scenarios. Each member keeps its own file under `tests/integration/` and is
//! mounted here as a module, so the scenarios compile the shared support trees
//! and link once instead of once per file. Test names gain the member module
//! as a prefix (`<member>::<test>`); select one file's tests with
//! `cargo test --test reborn_integration_suite_platform <member>::`. See
//! `tests/integration/AGENTS.md` §"Suite binaries".

#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../../support/mod.rs"]
mod support;

#[path = "../backend_matrix.rs"]
mod backend_matrix;
#[path = "../config_reload.rs"]
mod config_reload;
#[path = "../durable.rs"]
mod durable;
#[path = "../environment_access.rs"]
mod environment_access;
#[path = "../hooks.rs"]
mod hooks;
#[path = "../identity_resolution_smoke.rs"]
mod identity_resolution_smoke;
#[path = "../openai_compat_embeddings.rs"]
mod openai_compat_embeddings;
#[path = "../operator_metrics.rs"]
mod operator_metrics;
#[path = "../outbound_store_durability.rs"]
mod outbound_store_durability;
#[path = "../secrets.rs"]
mod secrets;
#[path = "../tracecap.rs"]
mod tracecap;
#[path = "../webui_v2_product_api.rs"]
mod webui_v2_product_api;
#[path = "../webui_v2_router_smoke.rs"]
mod webui_v2_router_smoke;
#[path = "../wiring_parity.rs"]
mod wiring_parity;
