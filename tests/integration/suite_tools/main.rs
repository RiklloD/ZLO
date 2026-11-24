//! Reborn integration suite — Tool dispatch, egress, MCP, disclosure, and first-party tool journeys.
//!
//! One test binary for this domain's flat (single-thread) integration
//! scenarios. Each member keeps its own file under `tests/integration/` and is
//! mounted here as a module, so the scenarios compile the shared support trees
//! and link once instead of once per file. Test names gain the member module
//! as a prefix (`<member>::<test>`); select one file's tests with
//! `cargo test --test reborn_integration_suite_tools <member>::`. See
//! `tests/integration/AGENTS.md` §"Suite binaries".

#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../../support/mod.rs"]
mod support;

#[path = "../attach.rs"]
mod attach;
#[path = "../document_edit.rs"]
mod document_edit;
#[path = "../extension_visibility.rs"]
mod extension_visibility;
#[path = "../external_tool_gate_denied_resume.rs"]
mod external_tool_gate_denied_resume;
#[path = "../hosted_mcp_registration.rs"]
mod hosted_mcp_registration;
#[path = "../http_matcher.rs"]
mod http_matcher;
#[path = "../lease_wedge.rs"]
mod lease_wedge;
#[path = "../mcp.rs"]
mod mcp;
#[path = "../outbound_target.rs"]
mod outbound_target;
#[path = "../process_port.rs"]
mod process_port;
#[path = "../profile.rs"]
mod profile;
#[path = "../project_create.rs"]
mod project_create;
#[path = "../real_egress_pipeline.rs"]
mod real_egress_pipeline;
#[path = "../secret_injection.rs"]
mod secret_injection;
#[path = "../skill_activate.rs"]
mod skill_activate;
#[path = "../surface_disclosure.rs"]
mod surface_disclosure;
#[path = "../tool_call.rs"]
mod tool_call;
#[path = "../tool_disclosure.rs"]
mod tool_disclosure;
#[path = "../web_access.rs"]
mod web_access;
