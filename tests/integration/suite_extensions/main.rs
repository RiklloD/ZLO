//! Reborn integration suite — Extensions, channels, delivery, and auth journeys.
//!
//! One test binary for this domain's flat (single-thread) integration
//! scenarios. Each member keeps its own file under `tests/integration/` and is
//! mounted here as a module, so the scenarios compile the shared support trees
//! and link once instead of once per file. Test names gain the member module
//! as a prefix (`<member>::<test>`); select one file's tests with
//! `cargo test --test reborn_integration_suite_extensions <member>::`. See
//! `tests/integration/AGENTS.md` §"Suite binaries".

#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../../support/mod.rs"]
mod support;

// Shared OAuth fixtures for `oauth_connect` and `oauth_popup_journeys`,
// mounted once so the crate compiles one copy.
#[path = "../auth/common.rs"]
mod auth_common;
#[path = "../auth/auth_failure.rs"]
mod auth_failure;
#[path = "../auth/auth_gate.rs"]
mod auth_gate;
#[path = "../channel_connection_projection.rs"]
mod channel_connection_projection;
#[path = "../delivery_user_journeys.rs"]
mod delivery_user_journeys;
#[path = "../extension_delivery.rs"]
mod extension_delivery;
#[path = "../extension_ingress.rs"]
mod extension_ingress;
#[path = "../extension_runtime.rs"]
mod extension_runtime;
#[path = "../extension_user_lifecycle_isolation.rs"]
mod extension_user_lifecycle_isolation;
#[path = "../notification_inbox_user_isolation.rs"]
mod notification_inbox_user_isolation;
#[path = "../auth/oauth_connect.rs"]
mod oauth_connect;
#[path = "../auth/oauth_popup_journeys.rs"]
mod oauth_popup_journeys;
#[path = "../auth/oauth_refresh.rs"]
mod oauth_refresh;
#[path = "../plugin_ui.rs"]
mod plugin_ui;
#[path = "../auth/reopen_resume_through_gate.rs"]
mod reopen_resume_through_gate;
