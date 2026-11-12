use super::*;

/// Product-agnostic extension manifest record.
///
/// Compiled once per install/upgrade: the raw source is kept for diagnostics
/// and recompilation only; production projection reads the
/// [`ResolvedExtensionManifest`] (checklist REC-1). v2 records may still be
/// reprojected from `raw_toml` by domain crates until their cutover phases
/// land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionManifestRecord {
    raw_toml: String,
    manifest: ExtensionManifestV2,
    resolved: ResolvedExtensionManifest,
    manifest_hash: Option<ManifestHash>,
    removal_cleanup_requirements: Vec<ExtensionRemovalCleanupRequirement>,
    definition_retention: PackageDefinitionRetention,
}

/// Minimal probe used to dispatch the single parse entry point on the
/// declared schema version (checklist MAN-2).
#[derive(Deserialize)]
pub(super) struct SchemaVersionProbe {
    #[serde(default)]
    schema_version: String,
}

impl ExtensionManifestRecord {
    /// The single manifest parse entry point: dispatches on the declared
    /// `schema_version` (v2 or v3) and normalizes both into the same
    /// resolved model.
    /// `root_binding` forces callers to distinguish a materialized package, a
    /// remote-only package, and the legacy loader compatibility state.
    pub fn from_toml(
        raw_toml: impl Into<String>,
        source: ManifestSource,
        host_port_catalog: &HostPortCatalog,
        manifest_hash: Option<ManifestHash>,
        contracts: &HostApiContractRegistry,
        root: Option<VirtualPath>,
    ) -> Result<Self, ExtensionInstallationError> {
        let root_binding = root.map_or(
            PackageRootBinding::FabricateOnLoad,
            PackageRootBinding::Materialized,
        );
        Self::from_toml_with_root_binding(
            raw_toml,
            source,
            host_port_catalog,
            manifest_hash,
            contracts,
            root_binding,
        )
    }

    /// Parse while preserving the full package-root contract, including a
    /// virtual definition which must never fabricate a filesystem root.
    pub fn from_toml_with_root_binding(
        raw_toml: impl Into<String>,
        source: ManifestSource,
        host_port_catalog: &HostPortCatalog,
        manifest_hash: Option<ManifestHash>,
        contracts: &HostApiContractRegistry,
        root_binding: PackageRootBinding,
    ) -> Result<Self, ExtensionInstallationError> {
        let raw_toml = raw_toml.into();
        let probe: SchemaVersionProbe = toml::from_str(&raw_toml).map_err(|error| {
            ExtensionInstallationError::InvalidManifest {
                reason: format!("failed to parse extension manifest: {error}"),
            }
        })?;
        let (manifest, mut resolved) =
            if probe.schema_version == crate::v3::MANIFEST_SCHEMA_VERSION_V3 {
                crate::v3::parse_v3(&raw_toml, source, host_port_catalog).map_err(|error| {
                    ExtensionInstallationError::InvalidManifest {
                        reason: error.to_string(),
                    }
                })?
            } else {
                let manifest =
                    ExtensionManifestV2::parse(&raw_toml, source, host_port_catalog, contracts)?;
                let resolved = ResolvedExtensionManifest::from_v2(&manifest);
                (manifest, resolved)
            };
        resolved.root_binding = root_binding;
        Ok(Self {
            raw_toml,
            manifest,
            resolved,
            manifest_hash,
            removal_cleanup_requirements: Vec::new(),
            definition_retention: PackageDefinitionRetention::RemoveWithLastInstallation,
        })
    }

    /// Rebuild a record from its persisted resolved contract — no TOML
    /// reparse; the raw source is carried for diagnostics only (checklist
    /// REC-2).
    pub fn from_resolved(
        raw_toml: impl Into<String>,
        source: ManifestSource,
        resolved: ResolvedExtensionManifest,
        manifest_hash: Option<ManifestHash>,
    ) -> Result<Self, ExtensionInstallationError> {
        let manifest = resolved.to_internal(source)?;
        Ok(Self {
            raw_toml: raw_toml.into(),
            manifest,
            resolved,
            manifest_hash,
            removal_cleanup_requirements: Vec::new(),
            definition_retention: PackageDefinitionRetention::RemoveWithLastInstallation,
        })
    }

    /// Attach host-trusted declarative cleanup metadata to the persisted
    /// manifest record. These requirements are never parsed from extension
    /// supplied TOML; catalog construction is the only production writer.
    pub fn with_removal_cleanup_requirements(
        mut self,
        requirements: Vec<ExtensionRemovalCleanupRequirement>,
    ) -> Self {
        self.removal_cleanup_requirements = requirements;
        self
    }

    pub fn with_definition_retention(mut self, retention: PackageDefinitionRetention) -> Self {
        self.definition_retention = retention;
        self
    }

    pub fn manifest(&self) -> &ExtensionManifestV2 {
        &self.manifest
    }

    /// The persisted resolved contract this record was compiled into.
    pub fn resolved(&self) -> &ResolvedExtensionManifest {
        &self.resolved
    }

    pub fn raw_toml(&self) -> &str {
        &self.raw_toml
    }

    pub fn extension_id(&self) -> &ExtensionId {
        &self.manifest.id
    }

    pub fn manifest_hash(&self) -> Option<&ManifestHash> {
        self.manifest_hash.as_ref()
    }

    pub fn removal_cleanup_requirements(&self) -> &[ExtensionRemovalCleanupRequirement] {
        &self.removal_cleanup_requirements
    }

    pub fn definition_retention(&self) -> PackageDefinitionRetention {
        self.definition_retention
    }
}
