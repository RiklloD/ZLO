use ironclaw_filesystem::InMemoryBackend;

use super::*;

pub(super) async fn installation_store() -> ExtensionInstallationStore {
    ExtensionInstallationStore::load_at(
        Arc::new(InMemoryBackend::new()),
        VirtualPath::new("/system/extensions/.installations/test").expect("valid root"),
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("filesystem store")
}

pub(super) fn capability_provider_contracts() -> crate::HostApiContractRegistry {
    let mut contracts = crate::HostApiContractRegistry::new();
    contracts
        .register(std::sync::Arc::new(
            crate::CapabilityProviderHostApiContract::new().expect("contract"),
        ))
        .expect("register capability provider contract");
    contracts
}

pub(super) fn manifest_record(extension_id: &str, hash: Option<&str>) -> ExtensionManifestRecord {
    ExtensionManifestRecord::from_toml(
        manifest_toml(extension_id),
        ManifestSource::HostBundled,
        &HostPortCatalog::empty(),
        hash.map(|value| ManifestHash::new(value).expect("hash")),
        &capability_provider_contracts(),
        None,
    )
    .expect("manifest record")
}

pub(super) fn installation(extension_id: &str, hash: Option<&str>) -> ExtensionInstallation {
    let extension_id = ExtensionId::new(extension_id.to_string()).expect("extension id");
    ExtensionInstallation::new(
        ExtensionInstallationId::new(extension_id.as_str().to_string()).expect("installation id"),
        extension_id.clone(),
        ExtensionManifestRef::new(
            extension_id,
            hash.map(|value| ManifestHash::new(value).expect("hash")),
        ),
        Vec::new(),
        Utc::now(),
        InstallationOwner::Tenant,
    )
    .expect("installation")
}

pub(super) fn pending_installation(
    extension_id: &str,
    hash: Option<&str>,
) -> ExtensionInstallation {
    let extension_id = ExtensionId::new(extension_id.to_string()).expect("extension id");
    ExtensionInstallation::new(
        ExtensionInstallationId::new(extension_id.as_str().to_string()).expect("installation id"),
        extension_id.clone(),
        ExtensionManifestRef::new(
            extension_id,
            hash.map(|value| ManifestHash::new(value).expect("hash")),
        ),
        Vec::new(),
        Utc::now(),
        InstallationOwner::Tenant,
    )
    .expect("pending installation")
}

/// A "not yet discovered" manifest: declares no capabilities, matching
/// the CAS guards in `take_v2_preparation_lease` / `finish_v2_preparation`
/// / `finish_v2_preparation_checkpoint`, which treat an empty `tools` list
/// as the pending-discovery state.
pub(super) fn pending_manifest_record(
    extension_id: &str,
    hash: Option<&str>,
) -> ExtensionManifestRecord {
    let record = manifest_record(extension_id, hash);
    let mut resolved = record.resolved().clone();
    resolved.tools = Vec::new();
    ExtensionManifestRecord::from_resolved(
        record.raw_toml(),
        record.manifest().source,
        resolved,
        record.manifest_hash().cloned(),
    )
    .expect("pending manifest record")
}

pub(super) fn manifest_toml(extension_id: &str) -> String {
    format!(
        r#"
schema_version = "reborn.extension_manifest.v2"
id = "{extension_id}"
name = "{extension_id}"
version = "0.1.0"
description = "test extension"
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/{extension_id}.wasm"

[[host_api]]
id = "ironclaw.capability_provider/v1"
section = "capability_provider.tools"

[capability_provider.tools]

[[capability_provider.tools.capabilities]]
id = "{extension_id}.read"
description = "read"
effects = ["network"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/read.input.json"
output_schema_ref = "schemas/read.output.json"
"#
    )
}
