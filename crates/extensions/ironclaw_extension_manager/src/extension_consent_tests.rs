//! Plugin consent, driven through the lifecycle command caller the `zlo
//! extension` CLI uses (`execute_reborn_extension_lifecycle_service_command`)
//! over the full lifecycle harness, with "boot" being a rebuild over the same
//! durable backing — exactly what every `zlo` invocation does.

use ironclaw_extension_contracts::state::InstallationState;
use ironclaw_host_api::ids::ExtensionId;
use ironclaw_host_api::path::VirtualPath;
use ironclaw_product_contracts::package_consent::{
    LifecycleConsentRequestKind, LifecycleConsentSummary,
};
use ironclaw_product_contracts::package_lifecycle::{
    LifecycleExtensionSource, LifecycleProductPayload, LifecycleProductResponse,
    public_lifecycle_response_json,
};

use crate::extension_lifecycle_command::{
    RebornExtensionLifecycleCommand, execute_reborn_extension_lifecycle_service_command,
};
use crate::lifecycle_test_support::{
    ExtensionLifecycleTestServices, build_lifecycle_test_services, lifecycle_product_context,
    rebuild_lifecycle_test_services, webui_gate_resource_scope_for_owner,
};

const PLUGIN_ID: &str = "acme-notes";

async fn run(
    services: &ExtensionLifecycleTestServices,
    owner: &str,
    command: RebornExtensionLifecycleCommand,
) -> Result<
    LifecycleProductResponse,
    crate::extension_lifecycle_command::RebornExtensionLifecycleCommandError,
> {
    execute_reborn_extension_lifecycle_service_command(
        &services.lifecycle_service,
        lifecycle_product_context(webui_gate_resource_scope_for_owner(owner)),
        command,
    )
    .await
}

type McpScript =
    std::sync::Arc<ironclaw_extension_host::extension_lifecycle::hosted_mcp_test_support::HostedMcpDiscoveryNetworkScript>;

/// The plugin's MCP server, scripted: records every JSON-RPC call so a test
/// can prove nothing reached the server before consent.
fn mcp_server() -> McpScript {
    std::sync::Arc::new(
        ironclaw_extension_host::extension_lifecycle::hosted_mcp_test_support::HostedMcpDiscoveryNetworkScript::with_tool_name("echo"),
    )
}

fn egress(server: &McpScript) -> Option<std::sync::Arc<dyn ironclaw_network::NetworkHttpEgress>> {
    Some(std::sync::Arc::clone(server) as std::sync::Arc<dyn ironclaw_network::NetworkHttpEgress>)
}

async fn reboot(
    previous: &ExtensionLifecycleTestServices,
    owner: &str,
    server: &McpScript,
) -> ExtensionLifecycleTestServices {
    rebuild_lifecycle_test_services(previous, owner, egress(server)).await
}

/// Write the `zlo extension new` scaffold where local packages are
/// discovered, then "boot" so the catalog picks it up.
async fn boot_with_scaffold(owner: &str, server: &McpScript) -> ExtensionLifecycleTestServices {
    let first = build_lifecycle_test_services(owner, egress(server), false).await;
    let id = ExtensionId::new(PLUGIN_ID).expect("id");
    for (path, bytes) in
        ironclaw_extension_host::extension_scaffold::scaffold_extension_package(&id)
    {
        let path =
            VirtualPath::new(format!("/system/extensions/{PLUGIN_ID}/{path}")).expect("asset path");
        first
            .filesystem
            .write_file(&path, &bytes)
            .await
            .expect("write scaffold file");
    }
    reboot(&first, owner, server).await
}

/// Active in both places an activation publishes: the registry the
/// capability host resolves descriptors from, and the generic host snapshot
/// dispatch resolves from.
async fn plugin_is_active(services: &ExtensionLifecycleTestServices) -> (bool, bool) {
    let id = ExtensionId::new(PLUGIN_ID).expect("id");
    let in_registry = services
        .extension_management
        .active_extensions_for_test()
        .snapshot()
        .get_extension(&id)
        .is_some();
    let in_generic_host = match services.extension_management.generic_host() {
        Some(host) => host.snapshot().await.extension(PLUGIN_ID).is_some(),
        None => false,
    };
    (in_registry, in_generic_host)
}

fn consent_of(response: &LifecycleProductResponse) -> &LifecycleConsentSummary {
    response
        .consent
        .as_ref()
        .unwrap_or_else(|| panic!("response carries a consent summary: {response:?}"))
}

#[tokio::test]
async fn local_plugin_install_requires_consent_and_grant_activates_it() {
    let owner = "consent-install-owner";
    let server = mcp_server();
    let services = boot_with_scaffold(owner, &server).await;

    // Discovery: the scaffold is found as an installed (local) package.
    let search = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Search {
            query: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("search");
    let Some(LifecycleProductPayload::ExtensionSearch { extensions, .. }) = &search.payload else {
        panic!("search payload: {search:?}");
    };
    let found = extensions
        .iter()
        .find(|entry| entry.summary.package_ref.id.as_str() == PLUGIN_ID)
        .expect("scaffolded plugin is discoverable");
    assert_eq!(found.summary.source, LifecycleExtensionSource::Installed);

    // Install: recorded, but not activated — consent is required.
    let install = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Install {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("install");
    assert_eq!(install.phase, InstallationState::ConsentRequired);
    let consent = consent_of(&install).clone();
    assert!(
        consent
            .requests
            .iter()
            .any(|request| request.kind == LifecycleConsentRequestKind::Tool
                && request.title == "acme-notes.echo"),
        "the pinned tool is listed: {consent:?}"
    );
    assert!(
        consent.requests.iter().any(
            |request| request.kind == LifecycleConsentRequestKind::Network
                && request.title == "mcp.example.com"
        ),
        "the server host is listed: {consent:?}"
    );
    assert_eq!(plugin_is_active(&services).await, (false, false));
    assert!(
        server.authorized_methods().is_empty(),
        "nothing reaches the plugin's server before consent"
    );

    // The public wire carries the state value and the consent shape verbatim.
    let wire = public_lifecycle_response_json(&install).expect("wire json");
    assert_eq!(wire["phase"], "consent_required");
    assert_eq!(wire["consent"]["digest"], consent.digest.as_str());
    assert!(
        wire["consent"]["requests"]
            .as_array()
            .is_some_and(
                |requests| requests.iter().all(|request| request["kind"].is_string()
                    && request["title"].is_string()
                    && request["detail"].is_string())
            ),
        "consent requests keep the kind/title/detail shape: {wire}"
    );

    // Explicit activation is refused the same way.
    let activate = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Activate {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("activate answers");
    assert_eq!(activate.phase, InstallationState::ConsentRequired);
    assert_eq!(plugin_is_active(&services).await, (false, false));

    // A grant naming any digest but the current one fails.
    let wrong = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: "0".repeat(64),
        },
    )
    .await;
    assert!(wrong.is_err(), "a stale digest is refused: {wrong:?}");
    assert_eq!(plugin_is_active(&services).await, (false, false));

    // The right digest grants and activates.
    let granted = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: consent.digest.as_str().to_string(),
        },
    )
    .await
    .expect("grant");
    assert_eq!(granted.phase, InstallationState::Active, "{granted:?}");
    assert_eq!(plugin_is_active(&services).await, (true, true));

    // Removal drops the grant: a reinstall asks again.
    run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Remove {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("remove");
    // Same process: the catalog entry is self-contained after removal.
    let reinstall = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Install {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("reinstall");
    assert_eq!(reinstall.phase, InstallationState::ConsentRequired);
}

#[tokio::test]
async fn boot_hydration_skips_an_installed_plugin_without_consent() {
    let owner = "consent-boot-owner";
    let server = mcp_server();
    let services = boot_with_scaffold(owner, &server).await;
    let install = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Install {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("install");
    assert_eq!(install.phase, InstallationState::ConsentRequired);

    // Boot restore enables every installed package with a surface; without
    // the consent gate this one would come up active.
    let rebooted = reboot(&services, owner, &server).await;
    assert_eq!(plugin_is_active(&rebooted).await, (false, false));
    let project = rebooted
        .extension_management
        .project(
            ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef::new(
                ironclaw_product_contracts::package_lifecycle::LifecyclePackageKind::Extension,
                PLUGIN_ID,
            )
            .expect("ref"),
            &ironclaw_host_api::ids::UserId::new(owner).expect("user"),
        )
        .await
        .expect("project");
    assert_eq!(project.phase, InstallationState::ConsentRequired);
    assert!(project.consent.is_some());
}

#[tokio::test]
async fn changed_files_after_grant_report_tampered_until_consent_is_granted_again() {
    let owner = "consent-tamper-owner";
    let server = mcp_server();
    let services = boot_with_scaffold(owner, &server).await;
    let install = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Install {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("install");
    let first_digest = consent_of(&install).digest.clone();
    let granted = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: first_digest.as_str().to_string(),
        },
    )
    .await
    .expect("grant");
    assert_eq!(granted.phase, InstallationState::Active);

    // A granted plugin survives a reboot.
    let rebooted = reboot(&services, owner, &server).await;
    assert_eq!(plugin_is_active(&rebooted).await, (true, true));

    // Change a non-manifest file: the manifest digest alone would not notice.
    let schema = VirtualPath::new(format!(
        "/system/extensions/{PLUGIN_ID}/schemas/{PLUGIN_ID}/echo.input.v1.json"
    ))
    .expect("schema path");
    rebooted
        .filesystem
        .write_file(
            &schema,
            br#"{"type":"object","properties":{"cmd":{"type":"string"}}}"#,
        )
        .await
        .expect("tamper");
    let tampered = reboot(&rebooted, owner, &server).await;
    assert_eq!(
        plugin_is_active(&tampered).await,
        (false, false),
        "a tampered plugin does not activate at boot"
    );
    let activate = run(
        &tampered,
        owner,
        RebornExtensionLifecycleCommand::Activate {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("activate answers");
    assert_eq!(activate.phase, InstallationState::Tampered);
    let new_digest = consent_of(&activate).digest.clone();
    assert_ne!(new_digest, first_digest, "the digest covers every file");
    assert_eq!(
        public_lifecycle_response_json(&activate).expect("wire")["phase"],
        "tampered"
    );

    // The old grant cannot be replayed; the new digest re-grants.
    assert!(
        run(
            &tampered,
            owner,
            RebornExtensionLifecycleCommand::GrantConsent {
                id: PLUGIN_ID.to_string(),
                digest: first_digest.as_str().to_string(),
            },
        )
        .await
        .is_err()
    );
    let regranted = run(
        &tampered,
        owner,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: new_digest.as_str().to_string(),
        },
    )
    .await
    .expect("re-grant");
    assert_eq!(regranted.phase, InstallationState::Active);
    assert_eq!(plugin_is_active(&tampered).await, (true, true));
}

#[tokio::test]
async fn host_bundled_extensions_need_no_consent() {
    let owner = "consent-bundled-owner";
    let services = build_lifecycle_test_services(owner, None, false).await;
    let install = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::Install {
            id: "web-access".to_string(),
        },
    )
    .await
    .expect("install bundled");
    assert!(install.consent.is_none());
    assert_ne!(install.phase, InstallationState::ConsentRequired);
    let grant = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: "web-access".to_string(),
            digest: "0".repeat(64),
        },
    )
    .await;
    assert!(
        grant.is_err(),
        "bundled packages have nothing to consent to"
    );
}

/// `zlo extension install <folder>`: the folder is read with the upload's
/// limits and imported through the upload's validation, so a WASM component
/// package installs (and waits for consent) while the `[mcp]` scaffold —
/// which uploads refuse — is refused the same way.
#[tokio::test]
async fn folder_install_goes_through_upload_validation_and_waits_for_consent() {
    let owner = "consent-folder-owner";
    let services = build_lifecycle_test_services(owner, None, false).await;

    let scaffold_dir = tempfile::tempdir().expect("tempdir");
    let id = ExtensionId::new("acme-folder-mcp").expect("id");
    for (path, bytes) in
        ironclaw_extension_host::extension_scaffold::scaffold_extension_package(&id)
    {
        let path = scaffold_dir.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }
    let files =
        ironclaw_extension_host::read_extension_folder(scaffold_dir.path()).expect("folder reads");
    let refused = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::InstallFiles { files },
    )
    .await;
    assert!(
        refused.is_err(),
        "uploads accept wasm packages only: {refused:?}"
    );

    let wasm_dir = tempfile::tempdir().expect("tempdir");
    let manifest = r#"schema_version = "reborn.extension_manifest.v3"
id = "acme-folder"
name = "Acme Folder"
version = "0.1.0"
description = "A folder-installed WASM plugin."
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/acme-folder.wasm"

[[tools]]
id = "acme-folder.invoke"
description = "Invoke the folder plugin."
effects = ["network"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/acme-folder/invoke.input.v1.json"
"#;
    std::fs::create_dir_all(wasm_dir.path().join("wasm")).expect("mkdir");
    std::fs::create_dir_all(wasm_dir.path().join("schemas/acme-folder")).expect("mkdir");
    std::fs::write(wasm_dir.path().join("manifest.toml"), manifest).expect("write");
    std::fs::write(
        wasm_dir.path().join("wasm/acme-folder.wasm"),
        include_bytes!("../../packages/github/wasm/github_tool.wasm"),
    )
    .expect("write");
    std::fs::write(
        wasm_dir
            .path()
            .join("schemas/acme-folder/invoke.input.v1.json"),
        br#"{"type":"object"}"#,
    )
    .expect("write");
    let files =
        ironclaw_extension_host::read_extension_folder(wasm_dir.path()).expect("folder reads");
    let install = run(
        &services,
        owner,
        RebornExtensionLifecycleCommand::InstallFiles { files },
    )
    .await
    .expect("folder install");
    assert_eq!(install.phase, InstallationState::ConsentRequired);
    assert_eq!(
        install.package_ref.as_ref().map(|r| r.id.as_str()),
        Some("acme-folder")
    );
    assert!(
        consent_of(&install)
            .requests
            .iter()
            .any(|request| request.title == "acme-folder.invoke")
    );
    let installed = services
        .extension_management
        .active_extensions_for_test()
        .snapshot()
        .get_extension(&ExtensionId::new("acme-folder").expect("id"))
        .is_some();
    assert!(!installed, "not active before consent");
}

async fn install_and_grant(services: &ExtensionLifecycleTestServices, owner: &str) {
    let install = run(
        services,
        owner,
        RebornExtensionLifecycleCommand::Install {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("install");
    let digest = consent_of(&install).digest.clone();
    let granted = run(
        services,
        owner,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: digest.as_str().to_string(),
        },
    )
    .await
    .expect("grant");
    assert_eq!(granted.phase, InstallationState::Active, "{granted:?}");
}

fn manifest_path() -> VirtualPath {
    VirtualPath::new(format!("/system/extensions/{PLUGIN_ID}/manifest.toml")).expect("manifest")
}

/// A package whose on-disk manifest no longer parses drops out of the
/// filesystem catalog, but its durable installation row and persisted
/// manifest survive. Boot hydration rebuilds packages from that persisted
/// manifest, so the consent gate must classify by the persisted source and
/// fail closed when the catalog cannot vouch for the files — whether or not
/// a grant existed.
#[tokio::test]
async fn boot_does_not_hydrate_a_plugin_whose_manifest_became_invalid() {
    for granted in [false, true] {
        let owner = "consent-invalid-manifest-owner";
        let server = mcp_server();
        let services = boot_with_scaffold(owner, &server).await;
        if granted {
            install_and_grant(&services, owner).await;
        } else {
            let install = run(
                &services,
                owner,
                RebornExtensionLifecycleCommand::Install {
                    id: PLUGIN_ID.to_string(),
                },
            )
            .await
            .expect("install");
            assert_eq!(install.phase, InstallationState::ConsentRequired);
        }
        services
            .filesystem
            .write_file(&manifest_path(), b"this is = = not toml [")
            .await
            .expect("corrupt manifest");

        let rebooted = reboot(&services, owner, &server).await;
        assert_eq!(
            plugin_is_active(&rebooted).await,
            (false, false),
            "granted={granted}: neither the registry nor the generic host may publish a plugin \
             the catalog cannot verify (capabilities, channel and hook surfaces all hang off \
             the generic host snapshot entry)"
        );
        let blocked = rebooted
            .extension_management
            .consent_blocked_extension_ids()
            .await
            .expect("blocked ids");
        assert!(
            blocked.contains(PLUGIN_ID),
            "granted={granted}: the invalid plugin is consent-blocked: {blocked:?}"
        );
    }
}

/// A shared (tenant-owned) installation is the tenant admin's to consent to:
/// a member may neither record a grant nor migrate the durable manifest, so
/// nothing a member does survives a restart as an activation.
#[tokio::test]
async fn only_the_tenant_admin_may_grant_consent_for_a_shared_installation() {
    let admin = "consent-shared-admin";
    let member = "consent-shared-member";
    let server = mcp_server();
    let services = boot_with_scaffold(admin, &server).await;
    let install = run(
        &services,
        admin,
        RebornExtensionLifecycleCommand::Install {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("install");
    let first_digest = consent_of(&install).digest.clone();
    let store = services.extension_management.installation_store_handle();
    let installation_id = ironclaw_extension_registry::ExtensionInstallationId::new(PLUGIN_ID)
        .expect("installation id");
    let installation = store
        .get_installation(&installation_id)
        .await
        .expect("read")
        .expect("installed");
    store
        .upsert_installation(
            installation.with_owner(ironclaw_extension_registry::InstallationOwner::Tenant),
        )
        .await
        .expect("make shared");

    // A member cannot record the first grant.
    let denied = run(
        &services,
        member,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: first_digest.as_str().to_string(),
        },
    )
    .await;
    assert!(denied.is_err(), "a member's grant is refused: {denied:?}");
    let rebooted = reboot(&services, admin, &server).await;
    assert_eq!(plugin_is_active(&rebooted).await, (false, false));
    let blocked = rebooted
        .extension_management
        .consent_blocked_extension_ids()
        .await
        .expect("blocked ids");
    assert!(blocked.contains(PLUGIN_ID), "no grant was recorded");

    // The admin grants; then the files change. A member can neither re-grant
    // the changed content nor migrate the durable manifest to it.
    let granted = run(
        &rebooted,
        admin,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: first_digest.as_str().to_string(),
        },
    )
    .await
    .expect("admin grant");
    assert_eq!(granted.phase, InstallationState::Active, "{granted:?}");
    rebooted
        .filesystem
        .write_file(
            &manifest_path(),
            String::from_utf8(
                rebooted
                    .filesystem
                    .read_file(&manifest_path())
                    .await
                    .expect("read manifest"),
            )
            .expect("utf8")
            .replace("Echo the given text back.", "Echo the text, and more.")
            .as_bytes(),
        )
        .await
        .expect("change manifest");
    let changed = reboot(&rebooted, admin, &server).await;
    let manifest_before = changed
        .extension_management
        .installation_store_handle()
        .get_manifest(&ExtensionId::new(PLUGIN_ID).expect("id"))
        .await
        .expect("read manifest record")
        .expect("manifest record");
    let activate = run(
        &changed,
        admin,
        RebornExtensionLifecycleCommand::Activate {
            id: PLUGIN_ID.to_string(),
        },
    )
    .await
    .expect("activate answers");
    assert_eq!(activate.phase, InstallationState::Tampered);
    let new_digest = consent_of(&activate).digest.clone();
    let denied = run(
        &changed,
        member,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: PLUGIN_ID.to_string(),
            digest: new_digest.as_str().to_string(),
        },
    )
    .await;
    assert!(
        denied.is_err(),
        "a member's grant for changed content is refused: {denied:?}"
    );
    let manifest_after = changed
        .extension_management
        .installation_store_handle()
        .get_manifest(&ExtensionId::new(PLUGIN_ID).expect("id"))
        .await
        .expect("read manifest record")
        .expect("manifest record");
    assert_eq!(
        manifest_after.raw_toml(),
        manifest_before.raw_toml(),
        "a member cannot migrate the durable manifest"
    );
    let after_restart = reboot(&changed, admin, &server).await;
    assert_eq!(plugin_is_active(&after_restart).await, (false, false));
}
