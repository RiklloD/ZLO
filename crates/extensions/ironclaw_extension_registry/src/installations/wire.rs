use super::*;

#[derive(Debug)]
pub(super) enum SaveRowError {
    CasConflict,
    NotFound,
    Store(ExtensionInstallationError),
}

impl From<ExtensionInstallationError> for SaveRowError {
    fn from(error: ExtensionInstallationError) -> Self {
        Self::Store(error)
    }
}

impl SaveRowError {
    pub(super) fn into_installation_error(self) -> ExtensionInstallationError {
        match self {
            Self::CasConflict => store_unavailable_error("extension installation row CAS conflict"),
            Self::NotFound => store_unavailable_error("extension installation row disappeared"),
            Self::Store(error) => error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WireManifestRecord {
    pub(super) raw_toml: String,
    pub(super) source: WireManifestSource,
    pub(super) resolved: Option<ResolvedExtensionManifest>,
    pub(super) manifest_hash: Option<ManifestHash>,
    pub(super) removal_cleanup_requirements: Vec<ExtensionRemovalCleanupRequirement>,
    pub(super) definition_retention: PackageDefinitionRetention,
}

#[derive(Serialize)]
pub(super) struct WireManifestRecordRef<'a> {
    pub(super) raw_toml: &'a str,
    pub(super) source: WireManifestSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resolved: &'a Option<ResolvedExtensionManifest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) manifest_hash: &'a Option<ManifestHash>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) removal_cleanup_requirements: &'a Vec<ExtensionRemovalCleanupRequirement>,
    #[serde(skip_serializing_if = "is_default_definition_retention")]
    pub(super) definition_retention: PackageDefinitionRetention,
}

impl Serialize for WireManifestRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        WireManifestRecordRef {
            raw_toml: &self.raw_toml,
            source: self.source,
            resolved: &self.resolved,
            manifest_hash: &self.manifest_hash,
            removal_cleanup_requirements: &self.removal_cleanup_requirements,
            definition_retention: self.definition_retention,
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
pub(super) struct WireManifestRecordValue {
    pub(super) raw_toml: String,
    pub(super) source: WireManifestSource,
    #[serde(default)]
    pub(super) resolved: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) manifest_hash: Option<ManifestHash>,
    #[serde(default)]
    pub(super) removal_cleanup_requirements: Vec<ExtensionRemovalCleanupRequirement>,
    #[serde(default)]
    pub(super) definition_retention: PackageDefinitionRetention,
}

impl<'de> Deserialize<'de> for WireManifestRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireManifestRecordValue::deserialize(deserializer)?;
        let resolved = wire
            .resolved
            .map(normalize_resolved_root_binding)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            raw_toml: wire.raw_toml,
            source: wire.source,
            resolved,
            manifest_hash: wire.manifest_hash,
            removal_cleanup_requirements: wire.removal_cleanup_requirements,
            definition_retention: wire.definition_retention,
        })
    }
}

pub(super) fn normalize_resolved_root_binding(
    mut resolved: serde_json::Value,
) -> Result<ResolvedExtensionManifest, String> {
    let fields = resolved
        .as_object_mut()
        .ok_or_else(|| "resolved extension manifest must be an object".to_string())?;
    let legacy_root = fields.remove("root");
    let had_legacy_root = legacy_root.is_some();
    let legacy_binding = match legacy_root {
        None | Some(serde_json::Value::Null) => PackageRootBinding::FabricateOnLoad,
        Some(root) => PackageRootBinding::Materialized(
            serde_json::from_value(root)
                .map_err(|error| format!("invalid legacy extension package root: {error}"))?,
        ),
    };
    if let Some(binding) = fields.get("root_binding") {
        let binding: PackageRootBinding = serde_json::from_value(binding.clone())
            .map_err(|error| format!("invalid extension package root binding: {error}"))?;
        if had_legacy_root && binding != legacy_binding {
            return Err("legacy root and root_binding disagree".to_string());
        }
    } else {
        fields.insert(
            "root_binding".to_string(),
            serde_json::to_value(legacy_binding)
                .map_err(|error| format!("failed to normalize package root binding: {error}"))?,
        );
    }
    serde_json::from_value(resolved)
        .map_err(|error| format!("invalid resolved extension manifest: {error}"))
}

impl WireManifestRecord {
    pub(super) fn into_manifest_record(
        self,
    ) -> Result<ExtensionManifestRecord, ExtensionInstallationError> {
        let resolved = self.resolved.ok_or_else(|| {
            invalid_installation_error("extension manifest row was not resolved during store load")
        })?;
        ExtensionManifestRecord::from_resolved(
            self.raw_toml,
            self.source.into_manifest_source(),
            resolved,
            self.manifest_hash,
        )
        .map(|record| {
            record
                .with_removal_cleanup_requirements(self.removal_cleanup_requirements)
                .with_definition_retention(self.definition_retention)
        })
    }
}

impl From<&ExtensionManifestRecord> for WireManifestRecord {
    fn from(record: &ExtensionManifestRecord) -> Self {
        Self {
            raw_toml: record.raw_toml().to_string(),
            source: WireManifestSource::from_manifest_source(record.manifest().source),
            resolved: Some(record.resolved().clone()),
            manifest_hash: record.manifest_hash().cloned(),
            removal_cleanup_requirements: record.removal_cleanup_requirements().to_vec(),
            definition_retention: record.definition_retention(),
        }
    }
}

pub(super) fn is_default_definition_retention(value: &PackageDefinitionRetention) -> bool {
    *value == PackageDefinitionRetention::RemoveWithLastInstallation
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WireManifestSource {
    HostBundled,
    InstalledLocal,
    RegistryInstalled,
    UserRegistered,
}

impl WireManifestSource {
    pub(super) fn from_manifest_source(source: ManifestSource) -> Self {
        match source {
            ManifestSource::HostBundled => Self::HostBundled,
            ManifestSource::InstalledLocal => Self::InstalledLocal,
            ManifestSource::RegistryInstalled => Self::RegistryInstalled,
            ManifestSource::UserRegistered => Self::UserRegistered,
        }
    }

    pub(super) fn into_manifest_source(self) -> ManifestSource {
        match self {
            Self::HostBundled => ManifestSource::HostBundled,
            Self::InstalledLocal => ManifestSource::InstalledLocal,
            Self::RegistryInstalled => ManifestSource::RegistryInstalled,
            Self::UserRegistered => ManifestSource::UserRegistered,
        }
    }
}

#[cfg(test)]
mod tests {

    use ironclaw_host_api::{host_port::HostPortCatalog, path::VirtualPath};

    use super::*;
    use crate::ManifestSource;
    use crate::installations::test_support::*;

    /// Wire round-trip: `ManifestSource::UserRegistered` survives
    /// serialize -> deserialize with the documented `"user_registered"`
    /// wire label.
    #[test]
    fn wire_manifest_source_round_trips_user_registered() {
        let wire = WireManifestSource::from_manifest_source(ManifestSource::UserRegistered);
        assert_eq!(wire, WireManifestSource::UserRegistered);
        let json = serde_json::to_string(&wire).expect("serialize wire manifest source");
        assert_eq!(json, "\"user_registered\"");
        let back: WireManifestSource =
            serde_json::from_str(&json).expect("deserialize wire manifest source");
        assert_eq!(back.into_manifest_source(), ManifestSource::UserRegistered);
    }

    /// A full manifest record with `ManifestSource::UserRegistered` survives
    /// the same wire round trip used to persist/reload installation rows.
    #[test]
    fn wire_manifest_record_round_trips_user_registered_source() {
        let record = ExtensionManifestRecord::from_toml_with_root_binding(
            manifest_toml("user-registered-fixture"),
            ManifestSource::UserRegistered,
            &HostPortCatalog::empty(),
            None,
            &capability_provider_contracts(),
            PackageRootBinding::FabricateOnLoad,
        )
        .expect("user-registered manifest record");
        let wire = WireManifestRecord::from(&record);
        let payload = serde_json::to_string(&wire).expect("serialize wire manifest record");
        let deserialized: WireManifestRecord =
            serde_json::from_str(&payload).expect("deserialize wire manifest record");
        let round_tripped = deserialized
            .into_manifest_record()
            .expect("round-tripped record rebuilds");
        assert_eq!(
            round_tripped.manifest().source,
            ManifestSource::UserRegistered
        );
    }

    #[test]
    fn manifest_wire_writer_emits_only_the_canonical_root_binding() {
        let root = VirtualPath::new("/system/extensions/wire-root").expect("root");
        let record = ExtensionManifestRecord::from_toml_with_root_binding(
            manifest_toml("wire-root"),
            ManifestSource::HostBundled,
            &HostPortCatalog::empty(),
            None,
            &capability_provider_contracts(),
            PackageRootBinding::Materialized(root.clone()),
        )
        .expect("manifest record");
        let payload = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
        let resolved = payload["resolved"].as_object().expect("resolved object");
        assert!(!resolved.contains_key("root"));
        assert_eq!(
            serde_json::from_value::<PackageRootBinding>(resolved["root_binding"].clone())
                .expect("root binding"),
            PackageRootBinding::Materialized(root)
        );
    }

    #[test]
    fn manifest_wire_reads_both_legacy_root_shapes() {
        let record = manifest_record("legacy-root", None);
        let canonical = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
        for (legacy_root, expected) in [
            (serde_json::Value::Null, PackageRootBinding::FabricateOnLoad),
            (
                serde_json::json!("/system/extensions/legacy-root"),
                PackageRootBinding::Materialized(
                    VirtualPath::new("/system/extensions/legacy-root").expect("root"),
                ),
            ),
        ] {
            let mut payload = canonical.clone();
            let resolved = payload["resolved"]
                .as_object_mut()
                .expect("resolved object");
            resolved.remove("root_binding");
            resolved.insert("root".to_string(), legacy_root);
            let wire: WireManifestRecord =
                serde_json::from_value(payload).expect("legacy root remains readable");
            assert_eq!(wire.resolved.expect("resolved").root_binding, expected);
        }

        let mut payload = canonical;
        payload["resolved"]
            .as_object_mut()
            .expect("resolved object")
            .remove("root_binding");
        let wire: WireManifestRecord =
            serde_json::from_value(payload).expect("omitted legacy root remains readable");
        assert_eq!(
            wire.resolved.expect("resolved").root_binding,
            PackageRootBinding::FabricateOnLoad
        );
    }

    #[test]
    fn manifest_wire_normalizes_the_immediately_preceding_v3_channel_shape() {
        use ironclaw_extension_contracts::channel::DeliveryTransport;

        let record = manifest_record("legacy-channel", None);
        let mut payload = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
        payload["resolved"]["channel"] = serde_json::json!({
            "id": "notifications",
            "display_name": "Legacy notifications",
            "inbound": false,
            "outbound": true,
            "notifications": true,
            "conversation_model": "continuous",
            "presentation": {
                "supports_markdown": false,
                "max_message_chars": 1500,
            },
        });

        let wire: WireManifestRecord = serde_json::from_value(payload)
            .expect("a persisted pre-split v3 channel remains readable");
        let resolved = wire.resolved.expect("resolved manifest");
        let channel = resolved.channel.expect("legacy channel remains active");
        assert!(!channel.supports_reply());
        assert_eq!(channel.delivery_transport(), Some(DeliveryTransport::Push));
        assert!(channel.requires_enrollment());
    }

    #[test]
    fn manifest_wire_accepts_agreeing_legacy_and_canonical_roots() {
        let root = VirtualPath::new("/system/extensions/dual-root").expect("root");
        let record = ExtensionManifestRecord::from_toml_with_root_binding(
            manifest_toml("dual-root"),
            ManifestSource::HostBundled,
            &HostPortCatalog::empty(),
            None,
            &capability_provider_contracts(),
            PackageRootBinding::Materialized(root.clone()),
        )
        .expect("manifest record");
        let mut payload = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
        payload["resolved"]
            .as_object_mut()
            .expect("resolved object")
            .insert("root".to_string(), serde_json::json!(root.as_str()));
        let wire: WireManifestRecord =
            serde_json::from_value(payload).expect("agreeing dual fields remain readable");
        assert_eq!(
            wire.resolved.expect("resolved").root_binding,
            PackageRootBinding::Materialized(root)
        );
    }

    #[test]
    fn manifest_wire_rejects_disagreeing_legacy_and_canonical_roots() {
        let record = manifest_record("dual-root", None);
        let mut payload = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
        let resolved = payload["resolved"]
            .as_object_mut()
            .expect("resolved object");
        resolved.insert(
            "root".to_string(),
            serde_json::json!("/system/extensions/dual-root"),
        );
        let error = serde_json::from_value::<WireManifestRecord>(payload)
            .expect_err("disagreeing dual root fields must fail closed");
        assert!(error.to_string().contains("root_binding disagree"));
    }

    #[test]
    fn user_registered_virtual_binding_round_trips_without_a_root() {
        let record = ExtensionManifestRecord::from_toml_with_root_binding(
            manifest_toml("virtual-fixture"),
            ManifestSource::UserRegistered,
            &HostPortCatalog::empty(),
            None,
            &capability_provider_contracts(),
            PackageRootBinding::Virtual,
        )
        .expect("virtual manifest record");
        assert_eq!(
            record.resolved().package_root_binding(),
            &PackageRootBinding::Virtual
        );
        assert_eq!(
            record.resolved().materialized_root(),
            Err(crate::PackageRootError::Virtual)
        );

        let payload = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
        assert!(payload["resolved"].get("root").is_none());
        let wire: WireManifestRecord = serde_json::from_value(payload).expect("round trip");
        assert_eq!(
            wire.resolved.expect("resolved").root_binding,
            PackageRootBinding::Virtual
        );
    }

    /// Back-compat: rows persisted before `UserRegistered` existed (only the
    /// three original wire labels) still deserialize under the extended
    /// enum.
    #[test]
    fn existing_persisted_manifest_source_rows_still_deserialize() {
        for (json, expected) in [
            (r#""host_bundled""#, ManifestSource::HostBundled),
            (r#""installed_local""#, ManifestSource::InstalledLocal),
            (r#""registry_installed""#, ManifestSource::RegistryInstalled),
        ] {
            let wire: WireManifestSource = serde_json::from_str(json)
                .expect("pre-existing wire manifest source label still deserializes");
            assert_eq!(wire.into_manifest_source(), expected);
        }
    }
}
