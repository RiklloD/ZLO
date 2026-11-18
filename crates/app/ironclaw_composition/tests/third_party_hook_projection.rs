//! The boot-time hook projection is trusted-only.
//!
//! Third-party (`InstalledLocal` / `RegistryInstalled`) packages contribute
//! hooks only through the live, consent-bound installer
//! (`observability::hooks::live`): per run, from the active extension
//! registry, for users whose own consent grant covers the package's files,
//! from module bytes verified against that grant. The boot projection
//! ([`build_hook_projection_registry`]) and the non-live replay factory
//! ([`build_hook_dispatcher_builder_factory`]) are therefore not a path for
//! them at all — not even when a third-party package is handed to them
//! directly. These tests drive those public entry points and pin that.
//!
//! The registrar's trust-tier rules (Installed tier, restricted sinks,
//! quarantine of an invalid set) are pinned on the live path in
//! `src/observability/hooks/tests.rs`, and on a real run by
//! `tests/plugin_package_surfaces.rs`.

#![deny(clippy::let_underscore_must_use)]

use ironclaw_composition::{
    HooksActivationConfig, build_hook_dispatcher_builder_factory, build_hook_projection_registry,
};
use ironclaw_extension_registry::{
    ExtensionManifest, ExtensionPackage, ExtensionRegistry, ManifestSource,
};
use ironclaw_hooks::registry::HookPointSpec;
use ironclaw_host_api::path::VirtualPath;

fn manifest(id: &str) -> String {
    format!(
        r#"schema_version = "reborn.extension_manifest.v2"
id = "{id}"
name = "{id}"
version = "0.1.0"
description = "{id} extension"
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/{id}.wasm"

[[host_api]]
id = "ironclaw.capability_provider/v1"
section = "capability_provider.tools"

[capability_provider.tools]

[[capability_provider.tools.capabilities]]
id = "{id}.run"
description = "Run {id}"
effects = ["network"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/{id}/run.input.v1.json"
required_host_ports = ["host.runtime.http_egress"]

[[hooks]]
id = "deny-run"
kind = "before_capability"
scope = "own_capabilities"
body = {{ mode = "predicate", spec = {{ type = "deny_capability", reason = "blocked", when = {{ type = "name_equals", name = "{id}.run" }} }} }}
"#
    )
}

fn third_party_registry(ids: &[&str], source: ManifestSource) -> ExtensionRegistry {
    let contracts =
        ironclaw_extension_host::product_extension_host_api_contract_registry().expect("contracts");
    let ports = ironclaw_host_api::host_port::default_host_port_catalog().expect("host ports");
    let mut registry = ExtensionRegistry::new();
    for id in ids {
        let manifest =
            ExtensionManifest::parse(&manifest(id), source, &ports, &contracts).expect("parses");
        assert!(!manifest.hooks.is_empty(), "the fixture declares a hook");
        let package = ExtensionPackage::from_manifest(
            manifest,
            VirtualPath::new(format!("/system/extensions/{id}")).expect("root"),
        )
        .expect("package");
        registry.insert(package).expect("insert");
    }
    registry
}

fn mint(
    factory: &ironclaw_turn_runner::loop_driver_host::HookDispatcherBuilderFactory,
) -> ironclaw_hooks::dispatch::HookDispatcherBuilder {
    futures::executor::block_on(factory(
        ironclaw_turn_runner::loop_driver_host::HookDispatcherBuildRequest {
            scope: ironclaw_host_api::resource::ResourceScope::system(),
        },
    ))
    .expect("mint hook builder")
}

/// A third-party package — consented or not, installed or registry-sourced —
/// never reaches a run through the boot projection, with both hook flags on.
#[tokio::test]
async fn boot_projection_never_carries_third_party_hooks() {
    let config = HooksActivationConfig::enabled().with_third_party_enabled(true);
    for source in [
        ManifestSource::InstalledLocal,
        ManifestSource::RegistryInstalled,
    ] {
        let registry = build_hook_projection_registry(
            third_party_registry(&["boot-plugin-a", "boot-plugin-b"], source),
            config,
        )
        .await
        .expect("projection registry assembles");
        let factory = build_hook_dispatcher_builder_factory(config, &registry)
            .expect("factory builds")
            .expect("flag ON yields a factory");
        let bound = mint(&factory)
            .build_arc()
            .active_bindings_snapshot(HookPointSpec::BeforeCapability);
        assert!(
            bound.is_empty(),
            "{source:?}: a third-party hook must not bind through the boot path: {bound:?}"
        );
    }
}
