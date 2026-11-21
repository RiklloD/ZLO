//! Unit-test matrix for the hook activation / projection / factory path.
//!
//! Moved out of the production module (serrrfirat's #3951 P1 finding #4: keep
//! the production hooks modules focused; the test matrix lives here).

use super::*;
// The activation logic was decomposed into focused submodules (#3951 finding
// #4); the test matrix reaches every production item — including the
// crate-private `pub(super)` ones — through these submodule globs.
use super::factory::*;
use super::projection::*;

use std::sync::Arc;

use ironclaw_hooks::dispatch::HookDispatcherBuilder;

use crate::error::RebornBuildError;
use ironclaw_extension_registry::ExtensionRegistry;

use ironclaw_extension_registry::v2::ManifestSource;
use ironclaw_extension_registry::{ExtensionManifest, ExtensionPackage};
use ironclaw_hooks::HookPhase;
use ironclaw_hooks::identity::{HookId, HookVersion};
use ironclaw_hooks::points::ObserverHookContext;
use ironclaw_hooks::registry::HookPointSpec;
use ironclaw_hooks::sink::{ObserverHook, ObserverSink};
use ironclaw_host_api::{host_port::HostPortCatalog, path::VirtualPath};

/// Canonical identity path for the TEST-ONLY first-party no-op observer.
/// Lives here (not in the production catalog) so the activation machinery
/// can be exercised end-to-end through the real composition path without
/// shipping a no-op hook in production.
const TEST_NOOP_OBSERVER_CANONICAL_PATH: &str =
    "ironclaw_composition::hooks::tests::NoOpObserverHook";

/// A test-only first-party no-op observer. Observers cannot affect
/// outcomes; this one records nothing. It proves the builtin install +
/// dispatch path end to end through `build_hook_dispatcher_builder_factory_with`.
#[derive(Debug, Default)]
struct NoOpObserverHook;

#[async_trait::async_trait]
impl ObserverHook for NoOpObserverHook {
    async fn observe(&self, _ctx: &ObserverHookContext, _sink: &mut dyn ObserverSink) {}
}

/// Test-only first-party installer: installs the [`NoOpObserverHook`] at
/// `AfterCapability`. Passed to
/// [`build_hook_dispatcher_builder_factory_with`] in place of the empty
/// production catalog so the activation machinery is covered with a real
/// first-party binding.
fn install_test_first_party_hook(
    builder: HookDispatcherBuilder,
) -> Result<HookDispatcherBuilder, RebornBuildError> {
    let hook_id = HookId::for_builtin(TEST_NOOP_OBSERVER_CANONICAL_PATH, HookVersion::ONE);
    builder
        .install_builtin_observer(
            hook_id,
            HookPhase::Telemetry,
            HookPointSpec::AfterCapability,
            Box::new(NoOpObserverHook),
        )
        .map_err(|error| RebornBuildError::InvalidConfig {
            reason: format!("failed to install test first-party no-op observer hook: {error}"),
        })
}

/// Build a single-capability `reborn.extension_manifest.v2` manifest TOML
/// for `id`, optionally carrying a `[[hooks]]` block. The capability id is
/// provider-prefixed (`<id>.run`) as `ExtensionPackage::from_manifest`
/// requires.
fn manifest_toml(id: &str, hooks_block: &str) -> String {
    format!(
        r#"schema_version = "reborn.extension_manifest.v2"
id = "{id}"
name = "{id}"
version = "0.1.0"
description = "{id} extension"
trust = "untrusted"

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
effects = ["dispatch_capability"]
default_permission = "allow"
visibility = "model"
input_schema_ref = "schemas/{id}/run.input.v1.json"
output_schema_ref = "schemas/{id}/run.output.v1.json"
prompt_doc_ref = "prompts/{id}/run.md"
{hooks_block}"#
    )
}

/// Parse `toml` into a validated package rooted at the conventional
/// extensions path and insert it into a fresh registry. `source` selects the
/// trust posture: `InstalledLocal` (untrusted → quarantine on bad hooks) vs
/// `HostBundled` (trusted → fail-closed-whole-build).
fn registry_with_manifest_source(
    id: &str,
    toml: &str,
    source: ManifestSource,
) -> ExtensionRegistry {
    let manifest = ExtensionManifest::parse(
        toml,
        source,
        &HostPortCatalog::empty(),
        &capability_provider_contracts(),
    )
    .expect("manifest parses");
    let package = ExtensionPackage::from_manifest(
        manifest,
        VirtualPath::new(format!("/system/extensions/{id}")).expect("valid root path"),
    )
    .expect("package builds from manifest");
    let mut registry = ExtensionRegistry::new();
    registry.insert(package).expect("package inserts");
    registry
}

/// As [`registry_with_manifest_source`] with the default untrusted
/// (`InstalledLocal`) source.
fn registry_with_manifest(id: &str, toml: &str) -> ExtensionRegistry {
    registry_with_manifest_source(id, toml, ManifestSource::InstalledLocal)
}

/// Extract a plain registry's hook-only projections into a
/// [`HookProjectionRegistry`] for the hook-factory tests (the factory only
/// accepts the hook-only newtype). Mirrors the production extraction.
fn projection(registry: ExtensionRegistry) -> HookProjectionRegistry {
    HookProjectionRegistry::from_projections(
        registry
            .extensions()
            .filter_map(HookProjection::from_package)
            .collect(),
    )
}

#[test]
fn config_defaults_to_disabled() {
    assert!(!HooksActivationConfig::default().is_enabled());
    assert!(!HooksActivationConfig::disabled().is_enabled());
    assert!(HooksActivationConfig::enabled().is_enabled());
}

#[test]
fn third_party_flag_requires_master_flag_and_defaults_off() {
    // Default + master-only configs must report third-party OFF.
    assert!(!HooksActivationConfig::default().is_third_party_enabled());
    assert!(!HooksActivationConfig::disabled().is_third_party_enabled());
    assert!(
        !HooksActivationConfig::enabled().is_third_party_enabled(),
        "master flag alone must NOT enable third-party"
    );
    // Sub-flag on but master off ⇒ still OFF (master gate dominates).
    assert!(
        !HooksActivationConfig::disabled()
            .with_third_party_enabled(true)
            .is_third_party_enabled(),
        "sub-flag without the master flag must stay OFF"
    );
    // Both on ⇒ ON.
    assert!(
        HooksActivationConfig::enabled()
            .with_third_party_enabled(true)
            .is_third_party_enabled(),
        "master + sub-flag both on must enable third-party"
    );
    // Setting the sub-flag must never silently flip the master flag.
    assert!(
        !HooksActivationConfig::disabled()
            .with_third_party_enabled(true)
            .is_enabled()
    );
}

#[test]
fn truthy_tokens_enable_only_canonical_values() {
    for token in ["1", "true", "TRUE", "Yes", "on", " on "] {
        assert!(is_truthy(token), "{token:?} should be truthy");
    }
    for token in ["0", "false", "", "off", "no", "enabled", "2", "tru"] {
        assert!(!is_truthy(token), "{token:?} should be falsy");
    }
}

#[test]
fn disabled_config_yields_no_factory() {
    let registry = projection(ExtensionRegistry::new());
    let result =
        build_hook_dispatcher_builder_factory(HooksActivationConfig::disabled(), &registry)
            .expect("disabled build never errors");
    assert!(result.is_none(), "flag OFF must compose no dispatcher");
}

#[test]
fn enabled_config_with_empty_production_catalog_yields_valid_zero_binding_factory() {
    // The PRODUCTION first-party catalog is empty. Flag ON + empty
    // first-party set + no extension hooks must still compose a valid
    // dispatcher — a zero-binding dispatcher, not a panic/error. This pins
    // the empty-catalog-is-valid contract.
    let registry = projection(ExtensionRegistry::new());
    let factory =
        build_hook_dispatcher_builder_factory(HooksActivationConfig::enabled(), &registry)
            .expect("enabled build with empty registry + empty catalog succeeds")
            .expect("flag ON yields a factory even with an empty catalog");
    // The factory mints a valid dispatcher with no first-party bindings.
    let dispatcher = mint(&factory).expect("mint hook builder").build_arc();
    let bindings = dispatcher.active_bindings_snapshot(HookPointSpec::AfterCapability);
    assert!(
        bindings.is_empty(),
        "empty production catalog must yield zero first-party bindings, saw {bindings:?}"
    );
}

#[test]
fn activation_installs_a_test_first_party_hook_through_the_real_path() {
    // The activation machinery is exercised end-to-end with a TEST-ONLY
    // first-party hook (not a production-shipped no-op). We drive the same
    // composition path via the `*_with` seam and confirm the test hook is
    // bound at AfterCapability.
    let registry = projection(ExtensionRegistry::new());
    let factory = build_hook_dispatcher_builder_factory_with(
        HooksActivationConfig::enabled(),
        &registry,
        None,
        None,
        install_test_first_party_hook,
    )
    .expect("enabled build with a test first-party hook succeeds")
    .expect("flag ON yields a factory");
    let dispatcher = mint(&factory).expect("mint hook builder").build_arc();
    let test_id = HookId::for_builtin(TEST_NOOP_OBSERVER_CANONICAL_PATH, HookVersion::ONE);
    let bindings = dispatcher.active_bindings_snapshot(HookPointSpec::AfterCapability);
    assert!(
        bindings.iter().any(|binding| binding.hook_id == test_id),
        "test-only first-party hook must be installed through the real composition path"
    );
}

#[test]
fn factory_mints_independent_dispatchers_per_call() {
    let registry = projection(ExtensionRegistry::new());
    let factory =
        build_hook_dispatcher_builder_factory(HooksActivationConfig::enabled(), &registry)
            .expect("enabled build succeeds")
            .expect("flag ON yields a factory");
    let a = mint(&factory).expect("mint hook builder").build_arc();
    let b = mint(&factory).expect("mint hook builder").build_arc();
    assert!(
        !Arc::ptr_eq(&a, &b),
        "each factory call must mint a fresh dispatcher (per-run isolation)"
    );
}

// ─── Helpers for the projection / quarantine test matrix ─────────────────

use ironclaw_hooks::identity::{ExtensionId as HookExtensionId, HookLocalId};

/// Derive the registrar-minted hook id for an installed predicate hook.
fn installed_hook_id(ext: &str, version: &str, local: &str) -> HookId {
    HookId::derive(
        &HookExtensionId::new(ext).expect("valid extension id"),
        version,
        &HookLocalId::new(local).expect("valid hook local id"),
        HookVersion::ONE,
    )
}

/// `[[hooks]]` block for a valid `own_capabilities` deny predicate (needs no
/// grant, no WASM runtime).
fn own_deny_hook(local: &str, target: &str) -> String {
    format!(
        r#"
[[hooks]]
id = "{local}"
kind = "before_capability"
scope = "own_capabilities"
body = {{ mode = "predicate", spec = {{ type = "deny_capability", reason = "blocked by manifest hook", when = {{ type = "name_equals", name = "{target}" }} }} }}
"#
    )
}

/// Build an [`ExtensionRegistry`] directly from `(id, source, hooks)`
/// triples (no discovery).
fn registry_with(packages: &[(&str, ManifestSource, String)]) -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::new();
    for (id, source, hooks_block) in packages {
        let manifest = ExtensionManifest::parse(
            &manifest_toml(id, hooks_block),
            *source,
            &HostPortCatalog::empty(),
            &capability_provider_contracts(),
        )
        .expect("manifest parses");
        let package = ExtensionPackage::from_manifest(
            manifest,
            VirtualPath::new(format!("/system/extensions/{id}")).expect("valid root path"),
        )
        .expect("package builds from manifest");
        registry.insert(package).expect("package inserts");
    }
    registry
}

/// Bind `registry`'s third-party packages the only way production binds a
/// third-party package's hooks: published to the active registry with its
/// consent grant's pins, visible to the run's user, through the per-run live
/// installer. Returns the dispatcher a run would get.
fn live_dispatcher(registry: ExtensionRegistry) -> Arc<ironclaw_hooks::dispatch::HookDispatcher> {
    live_dispatcher_with(registry, install_first_party_hooks)
}

fn live_dispatcher_with(
    registry: ExtensionRegistry,
    first_party: fn(HookDispatcherBuilder) -> Result<HookDispatcherBuilder, RebornBuildError>,
) -> Arc<ironclaw_hooks::dispatch::HookDispatcher> {
    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    let mut ids = Vec::new();
    for package in registry.extensions() {
        ids.push(package.id.as_str().to_string());
        active.upsert(consented(package.clone())).expect("activate");
    }
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let tenant = ironclaw_host_api::ids::TenantId::new("tenant-live").expect("tenant");
    let tenant_root = tenant_extension_root(&tenant).expect("root");
    let installer = super::live::LiveHookInstaller::new(
        true,
        tenant,
        tenant_root,
        super::live::LiveHookSources {
            active,
            filesystem: Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
            settings: Arc::new(
                ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings,
            ),
            owners: StaticOwners::tenant_shared(&id_refs),
            quarantines: Default::default(),
        },
        Arc::new(ironclaw_hooks::evaluator::PredicateEvaluator::new()),
        Arc::new(first_party),
        Vec::new(),
    )
    .expect("installer builds");
    futures::executor::block_on(installer.build(build_for(user_scope("alice"))))
        .expect("mint hook builder")
        .build_arc()
}

/// The hook module fixtures, as the bytes a consent grant pins.
fn fixture_module_files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "wasm/gate.wasm",
            wat::parse_str(PASS_GATE_WAT).expect("wat"),
        ),
        (
            "wasm/observe.wasm",
            wat::parse_str(NOOP_OBSERVER_WAT).expect("wat"),
        ),
    ]
}

/// `package` as the lifecycle publishes a consented third-party package: with
/// the grant's per-file pins (here: the fixture hook modules).
fn consented(package: ExtensionPackage) -> ExtensionPackage {
    let files = fixture_module_files();
    package.with_content_pins(
        ironclaw_extension_registry::PackageContentPins::from_files(
            files.iter().map(|(path, bytes)| (*path, bytes.as_slice())),
        )
        .with_consent_digest(FIXTURE_CONSENT_DIGEST),
    )
}

/// The consent digest the fixture pins are granted under.
const FIXTURE_CONSENT_DIGEST: &str = "fixture-generation-a";

// ─── Atomic quarantine + trust-discrimination coverage ───────────────────

/// A valid `own_capabilities` predicate hook from an untrusted
/// (`InstalledLocal`) extension installs through the real
/// `HookRegistrar::install` path at the `Installed` tier, alongside the
/// test-only first-party hook (extension install does not displace
/// first-party).
#[test]
fn valid_extension_hook_manifest_installs_at_installed_tier() {
    let registry = registry_with_manifest(
        "valid-ext",
        &manifest_toml("valid-ext", &own_deny_hook("deny-run", "valid-ext.run")),
    );

    let dispatcher = live_dispatcher_with(registry, install_test_first_party_hook);

    let expected = installed_hook_id("valid-ext", "0.1.0", "deny-run");
    let bindings = dispatcher.active_bindings_snapshot(HookPointSpec::BeforeCapability);
    assert!(
        bindings.iter().any(|binding| binding.hook_id == expected),
        "installed extension hook must be bound at BeforeCapability; saw {bindings:?}"
    );

    let test_id = HookId::for_builtin(TEST_NOOP_OBSERVER_CANONICAL_PATH, HookVersion::ONE);
    let after = dispatcher.active_bindings_snapshot(HookPointSpec::AfterCapability);
    assert!(
        after.iter().any(|binding| binding.hook_id == test_id),
        "test-only first-party hook must remain installed alongside extension hooks"
    );
}

/// A malformed hook payload from an UNTRUSTED (`InstalledLocal`) extension
/// must be QUARANTINED — the build SUCCEEDS, that extension's hook is
/// absent, and (critically) no panic. This is the third-party degradation
/// contract: an attacker-controlled installed manifest cannot crash
/// composition nor fail the whole build.
#[test]
fn malformed_installed_extension_hook_is_quarantined_not_fatal() {
    let hooks_block = r#"
[[hooks]]
id = "broken-hook"
kind = "before_capability"
body = { mode = "nonsense" }
"#;
    let registry = registry_with_manifest("broken-ext", &manifest_toml("broken-ext", hooks_block));

    let dispatcher = live_dispatcher(registry);
    assert!(
        dispatcher
            .active_bindings_snapshot(HookPointSpec::BeforeCapability)
            .is_empty(),
        "quarantined extension must contribute no bindings"
    );
}

/// The SAME malformed payload from a TRUSTED (`HostBundled`) package must
/// fail the whole build closed with `InvalidConfig` — builtin/host-bundled
/// hooks are fail-closed-whole-build, never quarantined.
#[test]
fn malformed_host_bundled_extension_hook_fails_closed() {
    let hooks_block = r#"
[[hooks]]
id = "broken-hook"
kind = "before_capability"
body = { mode = "nonsense" }
"#;
    // HostBundled ids are reserved to the `ironclaw.` prefix.
    let registry = projection(registry_with_manifest_source(
        "ironclaw.broken",
        &manifest_toml("ironclaw.broken", hooks_block),
        ManifestSource::HostBundled,
    ));

    match build_hook_dispatcher_builder_factory(HooksActivationConfig::enabled(), &registry) {
        Err(RebornBuildError::InvalidConfig { reason }) => {
            assert!(
                reason.contains("ironclaw.broken") && reason.contains("broken-hook"),
                "fail-closed error must name the offending host-bundled extension + hook, got: {reason}"
            );
        }
        Ok(_) => panic!("malformed host-bundled manifest must fail the whole build"),
        Err(other) => panic!("expected InvalidConfig, got: {other}"),
    }
}

/// Atomic quarantine: an extension with two VALID hooks and one INVALID
/// hook must install NONE of its three hooks (whole-set atomicity), while a
/// sibling valid extension's hook IS installed.
#[test]
fn extension_with_one_invalid_hook_quarantines_the_whole_set_sibling_survives() {
    let bad_set = format!(
        "{}{}{}",
        own_deny_hook("ok-1", "mixed.run"),
        own_deny_hook("ok-2", "mixed.run"),
        // invalid third hook
        r#"
[[hooks]]
id = "bad-3"
kind = "before_capability"
body = { mode = "nonsense" }
"#
    );
    let registry = registry_with(&[
        ("mixed", ManifestSource::InstalledLocal, bad_set),
        (
            "good",
            ManifestSource::InstalledLocal,
            own_deny_hook("good-1", "good.run"),
        ),
    ]);

    let dispatcher = live_dispatcher(registry);
    let bindings = dispatcher.active_bindings_snapshot(HookPointSpec::BeforeCapability);

    // None of `mixed`'s hooks installed.
    for local in ["ok-1", "ok-2", "bad-3"] {
        let id = installed_hook_id("mixed", "0.1.0", local);
        assert!(
            !bindings.iter().any(|b| b.hook_id == id),
            "atomic quarantine must drop ALL of the offending extension's hooks ({local} leaked)"
        );
    }
    // Sibling `good` survives.
    let good_id = installed_hook_id("good", "0.1.0", "good-1");
    assert!(
        bindings.iter().any(|b| b.hook_id == good_id),
        "a sibling valid extension's hooks must still install"
    );
}

/// An untrusted extension claiming `scope = same_tenant` (a wider scope
/// than its own capabilities) with no host-verified grant is QUARANTINED by
/// the registrar's trust-attenuation check — build succeeds, hook absent.
#[test]
fn installed_extension_claiming_ungranted_wider_scope_is_quarantined() {
    let hooks_block = r#"
[[hooks]]
id = "cross-tenant-deny"
kind = "before_capability"
scope = "same_tenant"
requires_grant = "cross-tenant-policy"
body = { mode = "predicate", spec = { type = "deny_capability", reason = "wider-scope deny", when = { type = "name_equals", name = "other-ext.run" } } }
"#;
    let registry = registry_with_manifest("reachy-ext", &manifest_toml("reachy-ext", hooks_block));

    let dispatcher = live_dispatcher(registry);
    assert!(
        dispatcher
            .active_bindings_snapshot(HookPointSpec::BeforeCapability)
            .is_empty(),
        "ungranted wider-scope hook must be quarantined (no binding)"
    );
}

/// Third-party WASM stays OUT: the projection registrar has no
/// `wasm_runtime`, so a WASM-bodied installed hook fails install → under
/// quarantine the extension is dropped and the build continues (Step 6
/// negative test). A sibling predicate-only extension still installs.
#[test]
fn wasm_bodied_third_party_hook_is_quarantined_build_continues() {
    let wasm_block = r#"
[[hooks]]
id = "wasm-hook"
kind = "before_capability"
scope = "own_capabilities"
body = { mode = "wasm", export = "evaluate" }
"#;
    let registry = registry_with(&[
        (
            "wasm-ext",
            ManifestSource::InstalledLocal,
            wasm_block.to_string(),
        ),
        (
            "pred-ext",
            ManifestSource::InstalledLocal,
            own_deny_hook("pred-1", "pred-ext.run"),
        ),
    ]);

    let dispatcher = live_dispatcher(registry);
    let bindings = dispatcher.active_bindings_snapshot(HookPointSpec::BeforeCapability);
    assert!(
        !bindings
            .iter()
            .any(|b| b.hook_id == installed_hook_id("wasm-ext", "0.1.0", "wasm-hook")),
        "WASM-bodied third-party hook must be quarantined (no runtime in loader registrar)"
    );
    assert!(
        bindings
            .iter()
            .any(|b| b.hook_id == installed_hook_id("pred-ext", "0.1.0", "pred-1")),
        "sibling predicate extension must still install after a WASM quarantine"
    );
}

/// Containment: a package whose root escapes the tenant root via `..` is
/// rejected by `enforce_root_containment` (FS-hardening v1).
#[test]
fn root_containment_rejects_traversal_and_non_child() {
    let tenant_root = VirtualPath::new("/system/extensions/alpha").expect("root");
    // Strict child OK.
    assert!(
        enforce_root_containment(
            &tenant_root,
            &VirtualPath::new("/system/extensions/alpha/ext-1").expect("child")
        )
        .is_ok()
    );
    // Sibling tenant is not a child.
    assert!(
        enforce_root_containment(
            &tenant_root,
            &VirtualPath::new("/system/extensions/beta/ext-1").expect("sibling")
        )
        .is_err(),
        "another tenant's tree must not be a child of this tenant root"
    );
    // The tenant root itself is not a strict child.
    assert!(enforce_root_containment(&tenant_root, &tenant_root).is_err());
}

/// `tenant_extension_root` returns the fixed `/system/extensions` root
/// (Option 1 — FS-scoped isolation; the per-tenant `RootFilesystem`, not a
/// path segment, is the isolation boundary). The signature still requires
/// identity so callers must thread it (and the containment defense knows the
/// root), but the path is profile-independent. The cross-tenant proof lives
/// in the integration test driving two distinct per-tenant filesystems.
#[test]
fn tenant_root_is_the_fixed_system_extensions_root() {
    let a = tenant_extension_root(&ironclaw_host_api::ids::TenantId::new("alpha").expect("a"))
        .expect("root a");
    let b = tenant_extension_root(&ironclaw_host_api::ids::TenantId::new("beta").expect("b"))
        .expect("root b");
    assert_eq!(a.as_str(), "/system/extensions");
    assert_eq!(b.as_str(), "/system/extensions");
}

/// DoS cap: more than `MAX_INSTALLED_EXTENSIONS_CONSIDERED` hook-bearing
/// untrusted extensions ⇒ the surplus is quarantined (skipped), and the
/// build still succeeds. We use a small synthetic set keyed off the const
/// boundary so the test stays fast yet pins the ceiling.
#[test]
fn surplus_extensions_beyond_consider_cap_are_quarantined() {
    let mut packages: Vec<(String, ManifestSource, String)> = Vec::new();
    for i in 0..(MAX_INSTALLED_EXTENSIONS_CONSIDERED + 2) {
        let id = format!("ext-{i:03}");
        let hooks = own_deny_hook("h", &format!("{id}.run"));
        packages.push((id, ManifestSource::InstalledLocal, hooks));
    }
    let refs: Vec<(&str, ManifestSource, String)> = packages
        .iter()
        .map(|(id, src, hooks)| (id.as_str(), *src, hooks.clone()))
        .collect();
    let registry = registry_with(&refs);

    let dispatcher = live_dispatcher(registry);
    let installed = dispatcher
        .active_bindings_snapshot(HookPointSpec::BeforeCapability)
        .len();
    assert!(
        installed <= MAX_INSTALLED_EXTENSIONS_CONSIDERED,
        "no more than the consider-cap of extensions may install (saw {installed})"
    );
    assert!(
        installed >= 1,
        "the first extensions under the cap must still install"
    );
}

// ─── Tenant attribution for install-time quarantine audits (#3951 P1 #3) ──

/// The tenant-attributed production entry point
/// (`build_hook_dispatcher_builder_factory_for_tenant`) must emit install-time
/// quarantine audits under the REAL tenant id, not the synthetic
/// `"reborn-hook-projection"` fallback the no-tenant convenience entry point
/// uses. Drives the same call site `build_reborn_runtime` uses, and asserts on
/// the deterministic thread-local audit capture (see `audit::test_capture`) so
/// the test is immune to `tracing`'s process-wide max-level filter under
/// parallel `cargo test`.
#[test]
fn for_tenant_entry_point_attributes_install_time_quarantine_to_real_tenant() {
    // An active, consented third-party package whose hook payload is
    // malformed → quarantined when a run's live installer validates it,
    // which must be attributed to the real tenant.
    let hooks_block = r#"
[[hooks]]
id = "broken-hook"
kind = "before_capability"
body = { mode = "nonsense" }
"#;
    let package = consented(
        registry_with_manifest("broken-ext", &manifest_toml("broken-ext", hooks_block))
            .extensions()
            .next()
            .expect("fixture package")
            .clone(),
    );
    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    active.upsert(package).expect("activate");
    let real_tenant = ironclaw_host_api::ids::TenantId::new("acme-real-tenant").expect("tenant");
    let factory = build_hook_dispatcher_builder_factory_for_tenant(
        HooksActivationConfig::enabled().with_third_party_enabled(true),
        &HookProjectionRegistry::from_projections(Vec::new()),
        &real_tenant,
        Some(super::live::LiveHookSources {
            active,
            filesystem: Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
            settings: Arc::new(
                ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings,
            ),
            owners: StaticOwners::tenant_shared(&["broken-ext"]),
            quarantines: Default::default(),
        }),
    )
    .expect("factory builds")
    .expect("flag ON yields a factory");

    let (minted, captured) = super::audit::test_capture::with_capture(|| {
        futures::executor::block_on(factory(build_for(user_scope("alice"))))
    });
    let dispatcher = minted.expect("quarantine, not a failed run").build_arc();
    assert!(
        dispatcher
            .active_bindings_snapshot(HookPointSpec::BeforeCapability)
            .is_empty()
    );

    let tenants: Vec<&str> = captured
        .iter()
        .map(|(tenant, _ext)| tenant.as_str())
        .collect();
    assert!(
        captured
            .iter()
            .any(|(tenant, ext)| tenant == real_tenant.as_str() && ext == "broken-ext"),
        "install-time quarantine audit must carry the real tenant id `{}` for the \
         quarantined extension; captured {captured:?}",
        real_tenant.as_str()
    );
    assert!(
        !tenants.contains(&"reborn-hook-projection"),
        "the tenant-attributed entry point must NOT fall back to the synthetic \
         audit tenant; captured {captured:?}"
    );
}

fn capability_provider_contracts() -> ironclaw_extension_registry::HostApiContractRegistry {
    let mut contracts = ironclaw_extension_registry::HostApiContractRegistry::new();
    contracts
        .register(std::sync::Arc::new(
            ironclaw_extension_registry::CapabilityProviderHostApiContract::new()
                .expect("capability provider contract"),
        ))
        .expect("register capability provider contract");
    contracts
}

// ─── Settings hook inventory ─────────────────────────────────────────────

/// The read-only catalog lists every projected extension hook with its
/// point and the `Installed` tier, live only when the flags allow it.
#[test]
fn hook_catalog_lists_extension_hooks_with_point_trust_and_flag_state() {
    let registry = projection(registry_with(&[(
        "catalog-ext",
        ManifestSource::InstalledLocal,
        own_deny_hook("deny-run", "catalog-ext.run"),
    )]));
    let master_only =
        super::catalog::hook_catalog(HooksActivationConfig::enabled(), Some(&registry), false);
    let hook = master_only
        .hooks
        .iter()
        .find(|hook| hook.id == "catalog-ext/deny-run")
        .expect("extension hook listed");
    assert_eq!(
        hook.trust_class,
        ironclaw_product_contracts::hooks_catalog::HookTrustClassView::Installed
    );
    assert_eq!(
        hook.source,
        ironclaw_product_contracts::hooks_catalog::HookSource::Extension
    );
    assert_eq!(hook.points, vec!["before_capability".to_string()]);
    assert_eq!(hook.owner.as_deref(), Some("catalog-ext"));
    assert!(!hook.enabled, "third-party hooks need the sub-flag too");

    let both = super::catalog::hook_catalog(
        HooksActivationConfig::enabled().with_third_party_enabled(true),
        Some(&registry),
        false,
    );
    assert!(
        both.hooks
            .iter()
            .any(|hook| hook.id == "catalog-ext/deny-run" && hook.enabled)
    );
}

/// Review finding 7: a parseable hook the registrar rejects (ungranted wider
/// scope) is reported OFF with its quarantine reason, even with every flag on,
/// and an extension quarantined as a whole takes its valid sibling hooks with
/// it — the catalog reports what the dispatcher would actually install.
#[test]
fn hook_catalog_reports_install_time_quarantine_not_just_flags() {
    let rejected = r#"
[[hooks]]
id = "cross-tenant-deny"
kind = "before_capability"
scope = "same_tenant"
requires_grant = "cross-tenant-policy"
body = { mode = "predicate", spec = { type = "deny_capability", reason = "wider-scope deny", when = { type = "name_equals", name = "other-ext.run" } } }
"#;
    let mixed = format!(
        "{}{}",
        own_deny_hook("fine", "mixed-ext.run"),
        r#"
[[hooks]]
id = "wasm-one"
kind = "before_capability"
scope = "own_capabilities"
body = { mode = "wasm", export = "evaluate" }
"#
    );
    let registry = projection(registry_with(&[
        (
            "reachy-ext",
            ManifestSource::InstalledLocal,
            rejected.to_string(),
        ),
        ("mixed-ext", ManifestSource::InstalledLocal, mixed),
        (
            "good-ext",
            ManifestSource::InstalledLocal,
            own_deny_hook("deny-run", "good-ext.run"),
        ),
    ]));
    let catalog = super::catalog::hook_catalog(
        HooksActivationConfig::enabled().with_third_party_enabled(true),
        Some(&registry),
        false,
    );
    let hook = |id: &str| {
        catalog
            .hooks
            .iter()
            .find(|hook| hook.id == id)
            .unwrap_or_else(|| panic!("{id} listed: {:?}", catalog.hooks))
    };
    let reachy = hook("reachy-ext/cross-tenant-deny");
    assert!(!reachy.enabled);
    assert!(reachy.note.as_deref().unwrap_or("").contains("Quarantined"));
    // The valid hook of a quarantined extension is off too.
    let fine = hook("mixed-ext/fine");
    assert!(!fine.enabled);
    assert!(fine.note.as_deref().unwrap_or("").contains("Quarantined"));
    assert!(!hook("mixed-ext/wasm-one").enabled);
    assert!(hook("good-ext/deny-run").enabled);
}

fn mint(
    factory: &ironclaw_turn_runner::loop_driver_host::HookDispatcherBuilderFactory,
) -> Result<
    ironclaw_hooks::dispatch::HookDispatcherBuilder,
    ironclaw_turn_runner::loop_driver_host::RebornLoopDriverHostError,
> {
    futures::executor::block_on(factory(
        ironclaw_turn_runner::loop_driver_host::HookDispatcherBuildRequest {
            scope: ironclaw_host_api::resource::ResourceScope::system(),
        },
    ))
}

/// Package hooks follow the extension lifecycle. With live sources wired, the
/// per-run factory installs a third-party package's hooks only while the
/// package is in the active registry: activation (publish) registers them at
/// the `Installed` tier on the next run, deactivation/removal (unpublish)
/// leaves the next run with none — and drops the package's resident module
/// bytes and validation cache.
#[test]
fn live_hook_sets_follow_activation_and_deactivation() {
    use ironclaw_extension_registry::SharedExtensionRegistry;
    use ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings;

    let package_registry = registry_with_manifest(
        "live-ext",
        &manifest_toml("live-ext", &own_deny_hook("deny-run", "live-ext.run")),
    );
    let package = consented(
        package_registry
            .extensions()
            .next()
            .expect("fixture package")
            .clone(),
    );
    let active = Arc::new(SharedExtensionRegistry::new(ExtensionRegistry::new()));
    let tenant = ironclaw_host_api::ids::TenantId::new("tenant-live").expect("tenant");
    let tenant_root = tenant_extension_root(&tenant).expect("root");
    let filesystem: Arc<dyn ironclaw_filesystem::RootFilesystem> =
        Arc::new(ironclaw_filesystem::InMemoryBackend::new());
    let installer = super::live::LiveHookInstaller::new(
        true,
        tenant,
        tenant_root,
        super::live::LiveHookSources {
            active: Arc::clone(&active),
            filesystem,
            settings: Arc::new(DefaultsOnlyExtensionSettings),
            owners: StaticOwners::tenant_shared(&["live-ext"]),
            quarantines: Default::default(),
        },
        Arc::new(ironclaw_hooks::evaluator::PredicateEvaluator::new()),
        Arc::new(install_first_party_hooks),
        Vec::new(),
    )
    .expect("installer builds");
    let request = || ironclaw_turn_runner::loop_driver_host::HookDispatcherBuildRequest {
        scope: ironclaw_host_api::resource::ResourceScope::system(),
    };
    let expected = installed_hook_id("live-ext", "0.1.0", "deny-run");
    let bound = |builder: HookDispatcherBuilder| {
        builder
            .build_arc()
            .active_bindings_snapshot(HookPointSpec::BeforeCapability)
            .iter()
            .any(|binding| binding.hook_id == expected)
    };

    // Installed on disk but not active: no hooks.
    let before = futures::executor::block_on(installer.build(request())).expect("build");
    assert!(!bound(before), "an inactive package contributes no hooks");

    // Activated: the next run carries the package hook.
    active.upsert(package.clone()).expect("activate");
    let during = futures::executor::block_on(installer.build(request())).expect("build");
    assert!(bound(during), "an active package's hook registers");

    // Deactivated/removed: the next run carries none, and nothing stays
    // resident for the package.
    active.remove(&package.id);
    let after = futures::executor::block_on(installer.build(request())).expect("build");
    assert!(!bound(after), "a deactivated package's hook unregisters");
    assert!(installer.resident_module_extensions().is_empty());
}

// ---- live installer: per-user visibility, module pinning, failure policy ----

/// Installation owners for the live-installer tests (the production source is
/// the lifecycle manager's owner projection).
/// Owner rows plus consent decisions keyed by (extension id, user): `None`
/// admits every generation that carries a grant digest; `Some` admits a user
/// only for the digest they decided on.
struct StaticOwners(
    std::collections::BTreeMap<
        ironclaw_host_api::ids::ExtensionId,
        ironclaw_extension_registry::InstallationOwner,
    >,
    Option<std::collections::BTreeMap<(String, String), String>>,
);

impl StaticOwners {
    fn rows(
        rows: &[(&str, ironclaw_extension_registry::InstallationOwner)],
    ) -> Arc<dyn ironclaw_extension_manager::installation_visibility::InstallationOwnerSource> {
        Self::rows_with_decisions(rows, None)
    }

    fn rows_with_decisions(
        rows: &[(&str, ironclaw_extension_registry::InstallationOwner)],
        decisions: Option<&[(&str, &str, &str)]>,
    ) -> Arc<dyn ironclaw_extension_manager::installation_visibility::InstallationOwnerSource> {
        Arc::new(Self(
            rows.iter()
                .map(|(id, owner)| {
                    (
                        ironclaw_host_api::ids::ExtensionId::new(*id).expect("extension id"),
                        owner.clone(),
                    )
                })
                .collect(),
            decisions.map(|rows| {
                rows.iter()
                    .map(|(extension, user, digest)| {
                        (
                            (extension.to_string(), user.to_string()),
                            digest.to_string(),
                        )
                    })
                    .collect()
            }),
        ))
    }

    fn tenant_shared(
        ids: &[&str],
    ) -> Arc<dyn ironclaw_extension_manager::installation_visibility::InstallationOwnerSource> {
        let rows: Vec<_> = ids
            .iter()
            .map(|id| (*id, ironclaw_extension_registry::InstallationOwner::Tenant))
            .collect();
        Self::rows(&rows)
    }
}

#[async_trait::async_trait]
impl ironclaw_extension_manager::installation_visibility::InstallationOwnerSource for StaticOwners {
    async fn installation_owners(
        &self,
    ) -> Result<
        std::collections::BTreeMap<
            ironclaw_host_api::ids::ExtensionId,
            ironclaw_extension_registry::InstallationOwner,
        >,
        ironclaw_product_contracts::error::ProductOperationFailure,
    > {
        Ok(self.0.clone())
    }

    async fn execution_consented(
        &self,
        extension_id: &ironclaw_host_api::ids::ExtensionId,
        pins: Option<&ironclaw_extension_registry::PackageContentPins>,
        _owner: Option<&ironclaw_extension_registry::InstallationOwner>,
        caller: &ironclaw_host_api::ids::UserId,
    ) -> Result<bool, ironclaw_product_contracts::error::ProductOperationFailure> {
        let Some(granted) = pins.and_then(|pins| pins.consent_digest()) else {
            return Ok(false);
        };
        Ok(match &self.1 {
            None => true,
            Some(decisions) => decisions
                .get(&(
                    extension_id.as_str().to_string(),
                    caller.as_str().to_string(),
                ))
                .is_some_and(|decided| decided == granted),
        })
    }
}

fn user_scope(user: &str) -> ironclaw_host_api::resource::ResourceScope {
    ironclaw_host_api::resource::ResourceScope::local_default(
        ironclaw_host_api::ids::UserId::new(user).expect("user id"),
        ironclaw_host_api::ids::InvocationId::new(),
    )
    .expect("scope")
}

fn build_for(
    scope: ironclaw_host_api::resource::ResourceScope,
) -> ironclaw_turn_runner::loop_driver_host::HookDispatcherBuildRequest {
    ironclaw_turn_runner::loop_driver_host::HookDispatcherBuildRequest { scope }
}

fn live_installer(
    active: &Arc<ironclaw_extension_registry::SharedExtensionRegistry>,
    filesystem: Arc<dyn ironclaw_filesystem::RootFilesystem>,
    settings: Arc<dyn ironclaw_extension_registry::settings::ExtensionSettingsReader>,
    owners: Arc<dyn ironclaw_extension_manager::installation_visibility::InstallationOwnerSource>,
) -> super::live::LiveHookInstaller {
    let tenant = ironclaw_host_api::ids::TenantId::new("tenant-live").expect("tenant");
    let tenant_root = tenant_extension_root(&tenant).expect("root");
    super::live::LiveHookInstaller::new(
        true,
        tenant,
        tenant_root,
        super::live::LiveHookSources {
            active: Arc::clone(active),
            filesystem,
            settings,
            owners,
            quarantines: Default::default(),
        },
        Arc::new(ironclaw_hooks::evaluator::PredicateEvaluator::new()),
        Arc::new(install_first_party_hooks),
        Vec::new(),
    )
    .expect("installer builds")
}

/// Extension ids with an Installed binding at `point` in a minted builder.
fn bound_extensions(
    builder: HookDispatcherBuilder,
    point: HookPointSpec,
) -> std::collections::BTreeSet<String> {
    builder
        .build_arc()
        .active_bindings_snapshot(point)
        .iter()
        .filter_map(|binding| {
            binding
                .owning_extension
                .as_ref()
                .map(|extension| extension.as_str().to_string())
        })
        .collect()
}

fn v2_package(id: &str, hooks_block: &str) -> ExtensionPackage {
    consented(
        registry_with_manifest(id, &manifest_toml(id, hooks_block))
            .extensions()
            .next()
            .expect("fixture package")
            .clone(),
    )
}

/// Per-user visibility (#5459 P1 applied to package hooks): a run installs
/// only the hooks of active packages whose installation is visible to the
/// run's user. A private install contributes to its members' runs only, a
/// tenant-shared one to everyone's, and a package with no installation row
/// to nobody's (fail closed).
#[tokio::test]
async fn live_hooks_follow_installation_visibility_per_run_user() {
    use ironclaw_extension_registry::InstallationOwner;
    use ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings;

    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    for id in ["alice-ext", "shared-ext", "orphan-ext"] {
        active
            .upsert(v2_package(
                id,
                &own_deny_hook("deny-run", &format!("{id}.run")),
            ))
            .expect("activate");
    }
    let installer = live_installer(
        &active,
        Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
        Arc::new(DefaultsOnlyExtensionSettings),
        StaticOwners::rows(&[
            (
                "alice-ext",
                InstallationOwner::user(ironclaw_host_api::ids::UserId::new("alice").unwrap()),
            ),
            ("shared-ext", InstallationOwner::Tenant),
        ]),
    );

    let alice = installer
        .build(build_for(user_scope("alice")))
        .await
        .expect("alice's run builds");
    assert_eq!(
        bound_extensions(alice, HookPointSpec::BeforeCapability),
        ["alice-ext", "shared-ext"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    let bob = installer
        .build(build_for(user_scope("bob")))
        .await
        .expect("bob's run builds");
    assert_eq!(
        bound_extensions(bob, HookPointSpec::BeforeCapability),
        ["shared-ext"].into_iter().map(String::from).collect(),
        "another user's private package contributes no hooks to bob's run"
    );
}

/// Per-run consent binds the package generation (P1 of the plugin-consent
/// review): visibility alone is not enough. With the published package's
/// pins granted under generation B, a member whose only decision names the
/// earlier generation A gets no hooks; the member who consented to B does;
/// and with generation A still published, the A decision installs it.
#[tokio::test]
async fn live_hooks_install_only_for_users_who_consented_to_the_published_generation() {
    use ironclaw_extension_registry::InstallationOwner;
    use ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings;

    let user = |id: &str| ironclaw_host_api::ids::UserId::new(id).unwrap();
    let members = || {
        StaticOwners::rows_with_decisions(
            &[(
                "swap-ext",
                InstallationOwner::users([user("alice"), user("bob")].into_iter().collect())
                    .unwrap(),
            )],
            Some(&[
                ("swap-ext", "alice", FIXTURE_CONSENT_DIGEST),
                ("swap-ext", "bob", "fixture-generation-b"),
            ]),
        )
    };
    let published = |digest: &str| {
        let package = v2_package("swap-ext", &own_deny_hook("deny-run", "swap-ext.run"));
        let pins = package
            .content_pins
            .clone()
            .expect("fixture pins")
            .with_consent_digest(digest);
        let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
            ExtensionRegistry::new(),
        ));
        active
            .upsert(package.with_content_pins(pins))
            .expect("activate");
        active
    };
    async fn hooked(
        installer: &super::live::LiveHookInstaller,
        who: &str,
    ) -> std::collections::BTreeSet<String> {
        bound_extensions(
            installer
                .build(build_for(user_scope(who)))
                .await
                .expect("run builds"),
            HookPointSpec::BeforeCapability,
        )
    }

    let generation_b = live_installer(
        &published("fixture-generation-b"),
        Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
        Arc::new(DefaultsOnlyExtensionSettings),
        members(),
    );
    assert!(
        hooked(&generation_b, "alice").await.is_empty(),
        "a decision for generation A installs nothing from generation B"
    );
    assert_eq!(
        hooked(&generation_b, "bob").await,
        ["swap-ext"].into_iter().map(String::from).collect()
    );

    let generation_a = live_installer(
        &published(FIXTURE_CONSENT_DIGEST),
        Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
        Arc::new(DefaultsOnlyExtensionSettings),
        members(),
    );
    assert_eq!(
        hooked(&generation_a, "alice").await,
        ["swap-ext"].into_iter().map(String::from).collect(),
        "the unchanged generation still installs for the user who consented to it"
    );
}

/// A v3 plugin manifest with WASM hooks and one setting.
fn v3_plugin(id: &str, hooks_block: &str) -> ExtensionPackage {
    let toml = format!(
        r#"schema_version = "reborn.extension_manifest.v3"
id = "{id}"
name = "{id}"
version = "0.1.0"
description = "{id} plugin"
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/tool.wasm"

[[tools]]
id = "{id}.run"
description = "Run {id}"
effects = []
default_permission = "allow"
visibility = "model"
input_schema_ref = "schemas/run.input.v1.json"
{hooks_block}
[[settings]]
id = "mode"
kind = "switch"
label = "Mode"
default = true
"#
    );
    let root = VirtualPath::new(format!("/system/extensions/{id}")).expect("root");
    let record = ironclaw_extension_registry::ExtensionManifestRecord::from_toml(
        toml,
        ManifestSource::InstalledLocal,
        &ironclaw_host_api::host_port::default_host_port_catalog().expect("catalog"),
        None,
        &ironclaw_extension_registry::HostApiContractRegistry::new(),
        Some(root.clone()),
    )
    .expect("v3 manifest parses");
    let manifest = ExtensionManifest::try_from(record.manifest().clone()).expect("manifest");
    consented(ExtensionPackage::from_manifest(manifest, root).expect("package"))
}

const WASM_GATE_HOOK: &str = r#"
[[hooks]]
id = "gate"
kind = "before_capability"
module = "wasm/gate.wasm"
body = { mode = "wasm", export = "evaluate" }
"#;

const WASM_OBSERVER_HOOK: &str = r#"
[[hooks]]
id = "observe"
kind = "after_capability"
module = "wasm/observe.wasm"
body = { mode = "wasm", export = "evaluate" }
"#;

const PASS_GATE_WAT: &str = r#"(module
  (import "ic:hooks/before-capability@1" "pass" (func $pass (result i32)))
  (memory (export "memory") 1)
  (func (export "evaluate") call $pass drop))"#;

const NOOP_OBSERVER_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "evaluate")))"#;

async fn plant_module(
    filesystem: &ironclaw_filesystem::InMemoryBackend,
    extension: &str,
    module: &str,
    wat_src: &str,
) {
    use ironclaw_filesystem::RootFilesystem;
    filesystem
        .write_file(
            &VirtualPath::new(format!("/system/extensions/{extension}/wasm/{module}"))
                .expect("module path"),
            &wat::parse_str(wat_src).expect("wat"),
        )
        .await
        .expect("write module");
}

/// A settings reader that parks inside `stored_values` until released, so a
/// test can interleave another build at that exact await point.
struct ParkedSettings {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ironclaw_extension_registry::settings::ExtensionSettingsReader for ParkedSettings {
    async fn stored_values(
        &self,
        _scope: &ironclaw_host_api::resource::ResourceScope,
        _extension_id: &ironclaw_host_api::ids::ExtensionId,
    ) -> Result<
        std::collections::BTreeMap<String, serde_json::Value>,
        ironclaw_extension_registry::settings::ExtensionSettingsError,
    > {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(std::collections::BTreeMap::new())
    }
}

/// A settings reader whose every read fails.
struct UnreadableSettings;

#[async_trait::async_trait]
impl ironclaw_extension_registry::settings::ExtensionSettingsReader for UnreadableSettings {
    async fn stored_values(
        &self,
        _scope: &ironclaw_host_api::resource::ResourceScope,
        extension_id: &ironclaw_host_api::ids::ExtensionId,
    ) -> Result<
        std::collections::BTreeMap<String, serde_json::Value>,
        ironclaw_extension_registry::settings::ExtensionSettingsError,
    > {
        Err(
            ironclaw_extension_registry::settings::ExtensionSettingsError::Storage {
                operation: "read",
                reason: format!(
                    "settings record for {} is unreadable",
                    extension_id.as_str()
                ),
            },
        )
    }
}

/// Concurrent dispatcher builds: build A admits a package (validated, module
/// bytes in hand) and parks in its settings read; meanwhile the package is
/// deactivated and build B prunes the shared module cache. Build A must still
/// install from the bytes it admitted rather than fail its run.
#[tokio::test]
async fn admitted_build_keeps_its_module_bytes_when_a_concurrent_build_prunes() {
    let filesystem = Arc::new(ironclaw_filesystem::InMemoryBackend::new());
    plant_module(&filesystem, "pinned-ext", "gate.wasm", PASS_GATE_WAT).await;
    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    let package = v3_plugin("pinned-ext", WASM_GATE_HOOK);
    active.upsert(package.clone()).expect("activate");
    let settings = Arc::new(ParkedSettings {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let installer = live_installer(
        &active,
        filesystem,
        Arc::clone(&settings) as _,
        StaticOwners::tenant_shared(&["pinned-ext"]),
    );

    let build_a = installer.build(build_for(user_scope("alice")));
    let interleave = async {
        settings.entered.notified().await;
        // Deactivated while A is parked: B sees no package and prunes.
        active.remove(&package.id);
        let build_b = installer
            .build(build_for(user_scope("bob")))
            .await
            .expect("build B");
        assert!(bound_extensions(build_b, HookPointSpec::BeforeCapability).is_empty());
        assert!(installer.resident_module_extensions().is_empty());
        settings.release.notify_one();
    };
    let (a, ()) = tokio::join!(build_a, interleave);
    let a = a.expect("build A installs from the module bytes it admitted");
    assert!(
        bound_extensions(a, HookPointSpec::BeforeCapability).contains("pinned-ext"),
        "the admitted run keeps the package's gate"
    );
}

/// Failure policy by hook kind when a package's settings cannot be read for
/// the run's user: after-fact observers are skipped for that run (observer
/// failure isolation) while the package's other hooks still install; a gate
/// (or prompt mutator) that would run on unread settings fails the build
/// closed instead of being skipped.
#[tokio::test]
async fn unreadable_settings_skip_observers_but_fail_gates_closed() {
    let filesystem = Arc::new(ironclaw_filesystem::InMemoryBackend::new());
    plant_module(
        &filesystem,
        "observer-ext",
        "observe.wasm",
        NOOP_OBSERVER_WAT,
    )
    .await;
    plant_module(&filesystem, "gate-ext", "gate.wasm", PASS_GATE_WAT).await;

    // Observer-only WASM: the run builds; the observer is absent, the
    // package's predicate gate (which reads no settings) still installs.
    let observer_block = format!(
        "{WASM_OBSERVER_HOOK}{}",
        own_deny_hook("deny-run", "observer-ext.run")
    );
    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    active
        .upsert(v3_plugin("observer-ext", &observer_block))
        .expect("activate");
    let installer = live_installer(
        &active,
        Arc::clone(&filesystem) as _,
        Arc::new(UnreadableSettings),
        StaticOwners::tenant_shared(&["observer-ext", "gate-ext"]),
    );
    let built = installer
        .build(build_for(user_scope("alice")))
        .await
        .expect("an observer's unreadable settings must not abort the run");
    let dispatcher = built.build_arc();
    assert!(
        dispatcher
            .active_bindings_snapshot(HookPointSpec::AfterCapability)
            .iter()
            .all(|binding| binding.owning_extension.is_none()),
        "the observer is skipped for this run"
    );
    assert!(
        dispatcher
            .active_bindings_snapshot(HookPointSpec::BeforeCapability)
            .iter()
            .any(|binding| binding
                .owning_extension
                .as_ref()
                .is_some_and(|extension| extension.as_str() == "observer-ext")),
        "the package's settings-free gate still installs"
    );

    // A WASM gate on unread settings: fail closed.
    active
        .upsert(v3_plugin("gate-ext", WASM_GATE_HOOK))
        .expect("activate");
    let Err(error) = installer.build(build_for(user_scope("alice"))).await else {
        panic!("a gate on unreadable settings fails the build closed");
    };
    assert!(
        error.to_string().contains("gate-ext"),
        "the failure names the package: {error}"
    );
}

/// A third-party package's hooks are bound to its consent grant: published
/// without the grant's pins it contributes nothing, and a hook module whose
/// bytes are not the pinned ones never compiles — both quarantined and
/// recorded for the hook inventory. The consented bytes install.
#[tokio::test]
async fn live_hooks_need_the_consent_pins_and_refuse_swapped_module_bytes() {
    use ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings;

    let filesystem = Arc::new(ironclaw_filesystem::InMemoryBackend::new());
    plant_module(&filesystem, "unpinned-ext", "gate.wasm", PASS_GATE_WAT).await;
    plant_module(&filesystem, "swapped-ext", "gate.wasm", NOOP_OBSERVER_WAT).await;
    plant_module(&filesystem, "intact-ext", "gate.wasm", PASS_GATE_WAT).await;
    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    let mut unpinned = v3_plugin("unpinned-ext", WASM_GATE_HOOK);
    unpinned.content_pins = None;
    active.upsert(unpinned).expect("activate");
    active
        .upsert(v3_plugin("swapped-ext", WASM_GATE_HOOK))
        .expect("activate");
    active
        .upsert(v3_plugin("intact-ext", WASM_GATE_HOOK))
        .expect("activate");
    let quarantines = super::live::LiveHookQuarantines::default();
    let tenant = ironclaw_host_api::ids::TenantId::new("tenant-live").expect("tenant");
    let installer = super::live::LiveHookInstaller::new(
        true,
        tenant.clone(),
        tenant_extension_root(&tenant).expect("root"),
        super::live::LiveHookSources {
            active,
            filesystem,
            settings: Arc::new(DefaultsOnlyExtensionSettings),
            owners: StaticOwners::tenant_shared(&["unpinned-ext", "swapped-ext", "intact-ext"]),
            quarantines: quarantines.clone(),
        },
        Arc::new(ironclaw_hooks::evaluator::PredicateEvaluator::new()),
        Arc::new(install_first_party_hooks),
        Vec::new(),
    )
    .expect("installer builds");
    let built = installer
        .build(build_for(user_scope("alice")))
        .await
        .expect("quarantine, not a failed run");
    assert_eq!(
        bound_extensions(built, HookPointSpec::BeforeCapability),
        ["intact-ext"].into_iter().map(String::from).collect(),
        "only the package whose module is the consented one installs"
    );
    assert!(
        quarantines
            .reason("unpinned-ext")
            .is_some_and(|reason| reason.contains("no consent grant")),
        "{:?}",
        quarantines.reason("unpinned-ext")
    );
    assert!(
        quarantines
            .reason("swapped-ext")
            .is_some_and(|reason| reason.contains("tampered")),
        "{:?}",
        quarantines.reason("swapped-ext")
    );
    assert!(quarantines.reason("intact-ext").is_none());
    assert!(
        !installer
            .resident_module_extensions()
            .contains("swapped-ext"),
        "refused bytes are never cached"
    );
}

/// The hook inventory a caller sees narrows third-party packages by the same
/// (consent-filtered) installation visibility a run applies: a private
/// package is listed to its member only, a tenant-shared one to everyone,
/// and a recorded live quarantine is shown with its reason.
#[tokio::test]
async fn hook_inventory_lists_third_party_hooks_only_to_users_who_can_see_them() {
    use ironclaw_extension_registry::InstallationOwner;
    use ironclaw_product_contracts::hooks_catalog::HookCatalogSource;

    let active = Arc::new(ironclaw_extension_registry::SharedExtensionRegistry::new(
        ExtensionRegistry::new(),
    ));
    for id in ["alice-ext", "shared-ext"] {
        active
            .upsert(v2_package(
                id,
                &own_deny_hook("deny-run", &format!("{id}.run")),
            ))
            .expect("activate");
    }
    let config = HooksActivationConfig::enabled().with_third_party_enabled(true);
    let catalog = super::catalog::CallerHookCatalog {
        base: super::catalog::hook_catalog(config, None, false),
        config,
        live: Some(super::live::LiveHookSources {
            active,
            filesystem: Arc::new(ironclaw_filesystem::InMemoryBackend::new()),
            settings: Arc::new(
                ironclaw_extension_registry::settings::DefaultsOnlyExtensionSettings,
            ),
            owners: StaticOwners::rows(&[
                (
                    "alice-ext",
                    InstallationOwner::user(ironclaw_host_api::ids::UserId::new("alice").unwrap()),
                ),
                ("shared-ext", InstallationOwner::Tenant),
            ]),
            quarantines: Default::default(),
        }),
    };
    let ids = |catalog: ironclaw_product_contracts::hooks_catalog::HookCatalog| {
        catalog
            .hooks
            .into_iter()
            .filter(|hook| {
                hook.source == ironclaw_product_contracts::hooks_catalog::HookSource::Extension
            })
            .map(|hook| (hook.id, hook.enabled))
            .collect::<Vec<_>>()
    };
    let alice = ids(catalog
        .catalog(&ironclaw_host_api::ids::UserId::new("alice").unwrap())
        .await);
    assert_eq!(
        alice,
        [
            ("alice-ext/deny-run".to_string(), true),
            ("shared-ext/deny-run".to_string(), true)
        ]
    );
    let bob = ids(catalog
        .catalog(&ironclaw_host_api::ids::UserId::new("bob").unwrap())
        .await);
    assert_eq!(
        bob,
        [("shared-ext/deny-run".to_string(), true)],
        "another user's private plugin is absent from bob's inventory"
    );
}
